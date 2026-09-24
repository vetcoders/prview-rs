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

/// Whether this run replayed `check_name` from a cache of known or unknown age.
///
/// Only [`TaskKind::Check`] entries answer. A context artifact backed by the
/// same tool is different work under the same id, and its replay says nothing
/// about the gate row this caveat is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReplayedCacheAge {
    Known(u64),
    Unknown,
}

fn replayed_cache_age(
    ledger: &crate::ledger::TaskLedger,
    check_name: &str,
) -> Option<ReplayedCacheAge> {
    use crate::ledger::{TaskKind, TaskState};

    let tool = crate::check_id::check_id_from_name(check_name);
    let entries = ledger.entries();
    entries
        .iter()
        .rev()
        .find(|entry| entry.kind == TaskKind::Check && entry.key.tool == tool)
        .and_then(|entry| match &entry.state {
            TaskState::Cached {
                cache_age_secs: Some(age),
                ..
            } => Some(ReplayedCacheAge::Known(*age)),
            TaskState::Cached {
                cache_age_secs: None,
                ..
            } => Some(ReplayedCacheAge::Unknown),
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
        scope,
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
        snapshot_integrity,
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
        inline.cargo_audit.as_ref(),
        clean_comparison.cargo_audit_lock_proof(),
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
        // Additive (schema 3.1): how much of this check's suite the run decided
        // had to execute, and why. Only the checks that own an ecosystem's test
        // scope carry it.
        // Pinned to the EXECUTED result, not to the policy row: `scope.mode`
        // is a claim about a command that ran, and a row for a check that never
        // ran has no command to describe.
        if let Some(report) =
            executed_check.and_then(|check| scope.and_then(|scope| scope.report_for_check(check)))
            && let Some(row) = gate_checks.last_mut()
        {
            row["scope"] = json!(report);
        }

        // A verdict may rest on evidence this run never produced. That is true
        // for a stale failure holding the merge AND for a stale pass allowing a
        // clean decision after the compiler/toolchain changed. Name every old
        // replayed gate row; the ledger lookup already proves this exact check
        // came from cache, while the caveat remains advisory-only.
        match replayed_cache_age(ledger, &eval.name) {
            Some(ReplayedCacheAge::Known(age)) if age > STALE_CACHE_CAVEAT_MAX_AGE_SECS => {
                stale_cache_caveats.push(json!({
                    "check_id": eval.check_id,
                    "check_name": eval.name,
                    "cache_age_secs": age,
                    "age_status": "stale",
                    "threshold_secs": STALE_CACHE_CAVEAT_MAX_AGE_SECS,
                }));
            }
            Some(ReplayedCacheAge::Unknown) => {
                // A future mtime, missing legacy metadata, or an unreadable
                // timestamp cannot be presented as fresh evidence. Preserve
                // the unknown instead of inventing zero, and surface the same
                // advisory caveat as an explicitly old replay.
                stale_cache_caveats.push(json!({
                    "check_id": eval.check_id,
                    "check_name": eval.name,
                    "cache_age_secs": null,
                    "age_status": "unknown",
                    "threshold_secs": STALE_CACHE_CAVEAT_MAX_AGE_SECS,
                }));
            }
            _ => {}
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

    if let Some(integrity) = snapshot_integrity {
        review_caveats.extend(integrity.apply_review(&mut worst_confidence, &mut worst_merge));
        if integrity.requires_review() {
            enforcement_disposition.raise_to(EnforcementDisposition::ReviewRequired);
        }
    }

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
    all_review_caveats.extend(cargo_audit_baseline_review_caveats(
        inline,
        clean_comparison.cargo_audit_lock_proof(),
    ));
    all_review_caveats.extend(semgrep_partial_parse_review_caveats(checks));
    // Advisory only: a narrower test run is still a real result, but a reviewer
    // must be told the suite was not exhaustive. Never moves the verdict.
    all_review_caveats.extend(
        scope
            .map(|scope| scope.review_caveats(checks))
            .unwrap_or_default(),
    );
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

    // Provenance/confidence, deliberately NOT a verified failure: when the run's
    // substrate and a check's own row disagree, the evidence may describe
    // another tree — that is a reason to doubt the evidence, not a defect of the
    // product under review. The rows are re-derived from the SAME pure function
    // and the SAME inputs `PROVENANCE.json` publishes (the checks of this run,
    // the reviewed target, and the operator cleanliness frozen before the run),
    // so the two artifacts cannot name different contradictions.
    let provenance = detect_provenance_contradictions(
        RunProvenance {
            target_sha: &resolved_target.commit_id,
            operator_worktree_clean: clean_comparison.operator_worktree_clean(),
        },
        checks,
    );
    //
    // The signal strings come from `ProvenanceConsistency::review_caveats` — the
    // single renderer the dashboard context (and through it report.json and the
    // "Copy PR comment" projection) reads as well, so no surface can name a
    // contradiction another one spells differently or omits.
    all_review_caveats.extend(provenance.review_caveats());

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
            "source": config.policy.source.as_ref().map(|path| path.display().to_string()),
            "origin": if config.policy.source.is_some() { "file" } else { "builtin-default" }
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
        // Additive and OUTSIDE `decision` for the same reason: a substrate
        // contradiction ranks no axis of the verdict. It says the evidence the
        // axes rest on may not describe the reviewed commit, and every row is
        // also carried as a review signal so no reader has to parse this array
        // to see it.
        "provenance_contradictions": provenance.contradictions,
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
            // The pack has exactly one browser entry point: `dashboard.html` by
            // default, `review.html` under `--no-dashboard`. Naming the file
            // that was not generated handed every static-report consumer a dead
            // link out of the canonical decision, so this follows the HTML the
            // run actually wrote.
            "dashboard": if config.create_dashboard {
                "dashboard.html"
            } else {
                "review.html"
            }
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
    md.push_str(&format!(
        "- Quality checks passed: `{}`\n- Policy has no hard blockers: `{}`\n- Allow merge: `{}`\n\n",
        quality_pass, policy_allow_merge, decision_fields.allow_merge,
    ));
    if decision.state == MergeDecisionState::Hold && policy_allow_merge && !quality_pass {
        md.push_str(
            "A non-blocking check can still fail quality. `HOLD` recommends reviewing \
             that failed evidence even though policy has no hard blocker; the canonical \
             verdict above remains authoritative.\n\n",
        );
    }
    if !provenance.is_empty() {
        let _ = write!(
            md,
            "{} provenance contradiction{} (`{}`) {} detected: the run and at least one check \
             describe different substrates. This is a confidence problem about the evidence, not \
             a verified check failure — no quality failure or blocking issue is derived from it, \
             and the verdict above is unchanged. Re-read every affected result as possibly \
             describing another tree; the rows are listed below and in \
             `00_summary/PROVENANCE.json`.\n\n",
            provenance.contradictions.len(),
            if provenance.contradictions.len() == 1 {
                ""
            } else {
                "s"
            },
            crate::artifacts::signal::PROVENANCE_CONTRADICTION_CODE,
            if provenance.contradictions.len() == 1 {
                "was"
            } else {
                "were"
            },
        );
    }
    append_review_signals(&mut md, all_review_caveats.iter().map(String::as_str));
    // Which changed paths were excluded from TEST SELECTION, and by which rule.
    // Published here so the call can be challenged without reading the source:
    // a reviewer who disagrees that a path is neutral can argue with the named
    // rule. It changes nothing else about the review — these files are still in
    // the diff, the artifacts, the signals and the verdict.
    if let Some(neutral) = scope.map(|scope| scope.non_participating.as_slice())
        && !neutral.is_empty()
    {
        md.push_str("## Test scope\n\n");
        md.push_str(
            "These changed paths did not take part in choosing which tests to run. They are \
             still reviewed everywhere else.\n\n",
        );
        md.push_str("| Path | Rule |\n|---|---|\n");
        for entry in neutral {
            let _ = writeln!(md, "| `{}` | `{}` |", entry.path, entry.rule);
        }
        md.push('\n');
    }
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

/// Render the canonical caveats without truncating or reclassifying them.
/// Continuation lines stay within their Markdown list item.
pub(super) fn append_review_signals<'a>(
    md: &mut String,
    caveats: impl IntoIterator<Item = &'a str>,
) {
    let caveats: Vec<_> = caveats.into_iter().collect();
    if caveats.is_empty() {
        return;
    }
    if !md.ends_with("\n\n") {
        md.push('\n');
    }
    let _ = writeln!(md, "## Review signals ({})\n", caveats.len());
    for caveat in caveats {
        let _ = writeln!(md, "- {}", caveat.replace('\n', "\n  "));
    }
    md.push('\n');
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
    cargo_audit: Option<&super::findings::CargoAuditGateEvidence>,
    cargo_audit_lock_proof: super::verdict::CargoAuditLockProof,
) -> EffectivePolicyOutcome {
    use crate::policy::engine::{AnalysisStatus, MergeRecommendation, PolicyConclusion};

    let mut worst_confidence = AnalysisStatus::Complete;
    let mut worst_merge = MergeRecommendation::Approve;
    let mut blocking_issues = Vec::new();
    let mut advisory_caveats = Vec::new();
    let mut effective_evals = Vec::with_capacity(evaluations.len());

    for eval in evaluations {
        let preexisting_only = preexisting_quality_failure_names.contains(eval.name.as_str());
        // Cargo audit is the one check whose blocker and whose downgrade both
        // have counts behind them, and the incident this addresses was the gate
        // refusing to quote them. Everything else keeps the generic wording.
        let audit = cargo_audit.filter(|_| eval.name.eq_ignore_ascii_case("cargo audit"));
        let effective_eval = effective_quality_gate_eval(eval, preexisting_only, audit);
        // The confidence axis bumps for EVERY check, including pre-existing-only
        // ones. The downgrade only neutralises the finding/merge impact — it must
        // not launder a degraded/incomplete analysis into Complete (R5-24). Since
        // a downgraded eval carries merge_impact = Approve, bumping the merge axis
        // here is a no-op for it, so only its (preserved) confidence propagates.
        bump_effective_gate_axes(&mut worst_confidence, &mut worst_merge, &effective_eval);
        if !preexisting_only {
            if effective_eval.conclusion == PolicyConclusion::Blocked {
                let detail = audit
                    .and_then(|audit| cargo_audit_blocker_detail(audit, cargo_audit_lock_proof));
                blocking_issues.push(match detail {
                    Some(detail) => format!(
                        "{} ({}): {detail}",
                        eval.name,
                        display_raw_status(&eval.raw_status)
                    ),
                    None => format!("{} ({})", eval.name, display_raw_status(&eval.raw_status)),
                });
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
    cargo_audit: Option<&super::findings::CargoAuditGateEvidence>,
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
    // "outside the change" is a location claim, and cargo audit has no location
    // to speak of: its proof is about `Cargo.lock`, so it states that proof
    // instead of borrowing a sentence that does not describe it.
    effective.reason = Some(
        cargo_audit
            .map(cargo_audit_preexisting_reason)
            .unwrap_or_else(|| "pre-existing findings outside the change".to_string()),
    );
    effective
}

/// Why a downgraded `Cargo audit` is advisory: the lock-based proof, in words,
/// with the counts the baseline caveat also carries.
fn cargo_audit_preexisting_reason(audit: &super::findings::CargoAuditGateEvidence) -> String {
    let proof = if audit.lock_changed {
        "unchanged vs base audit"
    } else {
        "Cargo.lock unchanged by this PR"
    };
    format!(
        "pre-existing: {proof} ({} advisor{})",
        audit.preexisting,
        if audit.preexisting == 1 { "y" } else { "ies" }
    )
}

/// What a blocking `Cargo audit` is blocking on, when the run has something to
/// say about it.
///
/// Four facts can carry the sentence, in order of how much they say:
/// advisories the diff introduced (named from the same key set they are counted
/// from), advisories with no base to compare against, an unreadable advisory
/// report, and — when the counts are silent — a withheld lockfile provenance
/// proof. The last one matters because a revoked proof is precisely how a
/// pre-existing-only audit ends up `unclassified` and blocking: the counts then
/// read `new=0, pre-existing=N` while the decision shows a bare
/// `Cargo audit (Failed)`, which is the mute blocker this text exists to
/// abolish. "Unclassified" names the outcome; the gap names the cause.
///
/// The unreadable report earns its own branch because it is a DIFFERENT cause
/// wearing the same zero counts. `status == "current-unavailable"` means
/// `cargo audit` produced nothing this run could parse, so every count
/// collapses to zero and the lockfile — proven or not — never entered the
/// question: there was no advisory to tie to it. Folding that case into the
/// provenance branch named the wrong cause, and named it with a count of zero.
///
/// No branch asserts a count it does not have. A count is printed only where it
/// is non-zero and earned; `{N} advisories not shown to predate this change`
/// qualifies a real `pre-existing`, and when that number is zero the gap states
/// itself alone.
///
/// `None` only when there is genuinely nothing to add — the proof held and the
/// counts are empty — so a bare blocker stays bare rather than gaining a
/// sentence asserting zero of everything.
fn cargo_audit_blocker_detail(
    audit: &super::findings::CargoAuditGateEvidence,
    lock_proof: super::verdict::CargoAuditLockProof,
) -> Option<String> {
    if audit.new > 0 {
        let ids = if audit.new_advisories.is_empty() {
            String::new()
        } else {
            format!(" ({})", audit.new_advisories.join(", "))
        };
        let plural = if audit.new == 1 { "y" } else { "ies" };
        // "New" is relative to the base audit; "introduced" additionally says
        // the target's own lockfile carries it, which only the proof shows.
        // Without it the count stays — it is still the most specific fact —
        // but the claim is qualified by the premise that is missing.
        return Some(match lock_proof {
            super::verdict::CargoAuditLockProof::TargetLock => format!(
                "{} new advisor{plural} introduced{ids}, {} pre-existing",
                audit.new, audit.preexisting
            ),
            super::verdict::CargoAuditLockProof::Unproven(gap) => format!(
                "{} new advisor{plural} vs the base audit{ids}, {} pre-existing; {}",
                audit.new,
                audit.preexisting,
                gap.gate_note()
            ),
        });
    }
    if audit.unknown > 0 {
        return Some(format!(
            "{} advisor{} with no base comparison (baseline {})",
            audit.unknown,
            if audit.unknown == 1 { "y" } else { "ies" },
            audit.status
        ));
    }
    if audit.status == "current-unavailable" {
        return Some(
            "no readable advisory report (baseline current-unavailable), so no \
             advisory could be classified either way"
                .to_string(),
        );
    }
    if let super::verdict::CargoAuditLockProof::Unproven(gap) = lock_proof {
        if audit.preexisting == 0 {
            return Some(gap.gate_note().to_string());
        }
        return Some(format!(
            "{} ({} advisor{} not shown to predate this change)",
            gap.gate_note(),
            audit.preexisting,
            if audit.preexisting == 1 { "y" } else { "ies" }
        ));
    }
    None
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
            file: None,
            line: None,
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

    fn review_signal_documents(count: usize) -> (String, String, Vec<String>) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let summary = tmp.path().join("00_summary");
        fs::create_dir(&summary).expect("summary directory");
        let mut config = test_config();
        config.profile.kind = crate::config::ProfileKind::Generic;
        let skipped: Vec<_> = (0..count)
            .map(|i| SkippedCheck {
                id: format!("audit_{i}"),
                name: format!("Audit {i}"),
                reason: format!("not requested: {i}\nadditional context: {i}"),
            })
            .collect();
        let inline = InlineFindingsSummary {
            cargo_audit: None,
            status: "passed".into(),
            findings_count: 0,
            dashboard_findings: vec![],
        };
        let coverage = empty_coverage();
        let (target, bases) = resolved_refs();
        generate_merge_gate(MergeGateInput {
            dir: &summary,
            config: &config,
            ledger: &empty_ledger(),
            scope: None,
            checks: &[],
            heuristics: None,
            inline: &inline,
            breaking: &[],
            rust_api_delta: None,
            coverage: &coverage,
            diffs: &[],
            skipped_checks: &skipped,
            resolved_target: &target,
            resolved_bases: &bases,
            clean_comparison: CleanComparison::for_test(true, true),
            snapshot_integrity: None,
        })
        .expect("merge gate");
        generate_ai_index(tmp.path(), &config, &[], &[], &coverage).expect("index");
        let gate: serde_json::Value =
            serde_json::from_slice(&fs::read(summary.join("MERGE_GATE.json")).unwrap()).unwrap();
        let caveats = serde_json::from_value(gate["decision"]["review_caveats"].clone())
            .expect("canonical string list");
        (
            fs::read_to_string(summary.join("MERGE_GATE.md")).unwrap(),
            fs::read_to_string(tmp.path().join("AI_INDEX.md")).unwrap(),
            caveats,
        )
    }

    #[test]
    fn merge_gate_markdown_explains_a_nonblocking_quality_failure_hold() {
        let tmp = tempfile::tempdir().unwrap();
        let mut config = test_config();
        config
            .policy
            .checks
            .insert("cargo_test".into(), PolicySeverity::Warn);
        let checks = [CheckResult {
            name: "Cargo test".into(),
            status: crate::checks::CheckStatus::Failed,
            duration: std::time::Duration::from_secs(1),
            output: "test result: FAILED. 0 passed; 1 failed".into(),
            cached: false,
            provenance: None,
        }];
        let inline = InlineFindingsSummary {
            cargo_audit: None,
            status: "passed".into(),
            findings_count: 0,
            dashboard_findings: vec![],
        };
        let (target, bases) = resolved_refs();
        generate_merge_gate(MergeGateInput {
            dir: tmp.path(),
            config: &config,
            ledger: &empty_ledger(),
            scope: None,
            checks: &checks,
            heuristics: None,
            inline: &inline,
            breaking: &[],
            rust_api_delta: None,
            coverage: &empty_coverage(),
            diffs: &[],
            skipped_checks: &[],
            resolved_target: &target,
            resolved_bases: &bases,
            clean_comparison: CleanComparison::for_test(true, true),
            snapshot_integrity: None,
        })
        .unwrap();
        let gate: serde_json::Value =
            serde_json::from_slice(&fs::read(tmp.path().join("MERGE_GATE.json")).unwrap()).unwrap();
        assert_eq!(gate["decision"]["verdict"], "CONDITIONAL");
        assert_eq!(gate["decision"]["recommended_label"], "HOLD");
        assert_eq!(gate["decision"]["quality_pass"], false);
        assert_eq!(gate["decision"]["policy_allow_merge"], true);
        assert_eq!(gate["checks"][0]["blocking"], false);
        let md = fs::read_to_string(tmp.path().join("MERGE_GATE.md")).unwrap();
        assert!(md.contains("- Quality checks passed: `false`"));
        assert!(md.contains("- Policy has no hard blockers: `true`"));
        assert!(md.contains("- Allow merge: `false`"));
        assert!(md.contains("A non-blocking check can still fail quality"));
    }

    /// Run the gate over `checks` with everything else neutral, on a run whose
    /// operator tree was frozen clean.
    fn run_gate_over(checks: &[CheckResult]) -> (serde_json::Value, String) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let config = test_config();
        let inline = InlineFindingsSummary {
            cargo_audit: None,
            status: "passed".into(),
            findings_count: 0,
            dashboard_findings: vec![],
        };
        let (target, bases) = resolved_refs();
        generate_merge_gate(MergeGateInput {
            dir: tmp.path(),
            config: &config,
            ledger: &empty_ledger(),
            scope: None,
            checks,
            heuristics: None,
            inline: &inline,
            breaking: &[],
            rust_api_delta: None,
            coverage: &empty_coverage(),
            diffs: &[],
            skipped_checks: &[],
            resolved_target: &target,
            resolved_bases: &bases,
            clean_comparison: CleanComparison::for_test(true, true),
            snapshot_integrity: None,
        })
        .expect("merge gate");
        (
            serde_json::from_slice(&fs::read(tmp.path().join("MERGE_GATE.json")).unwrap()).unwrap(),
            fs::read_to_string(tmp.path().join("MERGE_GATE.md")).unwrap(),
        )
    }

    fn passing_check_on(cwd: &str, tree_state: crate::checks::TreeState) -> CheckResult {
        CheckResult {
            name: "Cargo fmt".into(),
            status: CheckStatus::Passed,
            duration: Duration::from_secs(1),
            output: String::new(),
            cached: false,
            provenance: Some(crate::checks::CheckProvenance {
                command: "cargo fmt --check".into(),
                tool_version: None,
                cwd: cwd.into(),
                target_sha: Some(resolved_refs().0.commit_id),
                tree_state: Some(tree_state),
                exit_code: Some(0),
                started_at: "2026-09-11T10:00:00+02:00".into(),
                finished_at: "2026-09-11T10:00:01+02:00".into(),
                hard_fail_signatures: Vec::new(),
                cache_key: None,
                executed_scope: None,
            }),
        }
    }

    /// A substrate contradiction is a confidence problem, not a verified
    /// failure: it must be named as a review signal and a typed row, and it
    /// must not manufacture a quality failure or a blocking issue.
    #[test]
    fn a_substrate_contradiction_is_reported_as_a_provenance_signal() {
        let (gate, md) = run_gate_over(&[passing_check_on(
            "/repo",
            crate::checks::TreeState::LocalDirty,
        )]);

        let rows = gate["provenance_contradictions"]
            .as_array()
            .expect("provenance_contradictions array");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["code"], "PROVENANCE_CONTRADICTION");
        assert_eq!(rows[0]["kind"], "operator-worktree-state");
        assert_eq!(rows[0]["check_id"], "cargo_fmt");

        let caveats = gate["decision"]["review_caveats"]
            .as_array()
            .expect("review caveats");
        assert_eq!(
            caveats
                .iter()
                .filter(|caveat| caveat
                    .as_str()
                    .is_some_and(|text| text.starts_with("PROVENANCE_CONTRADICTION")))
                .count(),
            1,
            "the contradiction must also be readable as a review signal"
        );
        assert_eq!(
            gate["decision"]["quality_failures"]
                .as_array()
                .expect("quality failures")
                .len(),
            0,
            "a provenance contradiction must not be counted as a verified failure"
        );
        assert_eq!(
            gate["decision"]["blocking_issues"]
                .as_array()
                .expect("blocking issues")
                .len(),
            0
        );
        assert!(
            md.contains("1 provenance contradiction (`PROVENANCE_CONTRADICTION`) was detected")
        );
        assert!(md.contains("not a verified check failure"));
        assert!(md.contains("## Review signals (1)"));
    }

    /// Regression: a run whose rows agree keeps the gate it always had, and
    /// gains only an empty typed array.
    #[test]
    fn a_run_without_contradictions_keeps_its_gate_unchanged() {
        let (agreeing, md) = run_gate_over(&[passing_check_on(
            "/tmp/snapshot",
            crate::checks::TreeState::Snapshot,
        )]);

        assert_eq!(agreeing["decision"]["verdict"], "PASS");
        assert_eq!(agreeing["decision"]["allow_merge"], true);
        assert!(
            agreeing["provenance_contradictions"]
                .as_array()
                .expect("provenance_contradictions array")
                .is_empty()
        );
        assert!(
            !md.contains("provenance contradiction"),
            "no contradiction, no wording about one"
        );
    }

    /// One canonical review-signal list, not three that happen to agree.
    ///
    /// The contradiction used to reach `MERGE_GATE.json` alone: report.json's
    /// `gate.review_caveats`, the dashboard, and the dashboard's "Copy PR
    /// comment" (which reads that very field out of the embedded report) all
    /// derive from the dashboard context, and the context was built with no
    /// provenance at all. The pack therefore named the disagreement in the
    /// artifact machines read and hid it from the three a human reads. Every
    /// surface now takes the string from
    /// `ProvenanceConsistency::review_caveats`, so the signal is not merely
    /// present in each — it is the identical string.
    #[test]
    fn one_provenance_signal_reaches_gate_report_and_dashboard_identically() {
        let checks = [passing_check_on(
            "/repo",
            crate::checks::TreeState::LocalDirty,
        )];
        let (target, bases) = resolved_refs();
        let provenance = detect_provenance_contradictions(
            RunProvenance {
                target_sha: &target.commit_id,
                // The same value `CleanComparison::for_test(true, true)` gives
                // the gate below, so all three surfaces judge one substrate.
                operator_worktree_clean: Some(true),
            },
            &checks,
        );
        let expected = provenance.review_caveats();
        assert_eq!(
            expected.len(),
            1,
            "fixture must plant exactly one contradiction"
        );

        let (gate, _md) = run_gate_over(&checks);
        let gate_caveats: Vec<String> = gate["decision"]["review_caveats"]
            .as_array()
            .expect("gate review caveats")
            .iter()
            .map(|value| value.as_str().expect("caveat string").to_string())
            .collect();

        let tmp = tempfile::tempdir().expect("tempdir");
        let config = test_config();
        let inline = InlineFindingsSummary {
            cargo_audit: None,
            status: "passed".into(),
            findings_count: 0,
            dashboard_findings: vec![],
        };
        let dashboard = build_dashboard_context(DashboardContextInput {
            config: &config,
            scope: None,
            checks: &checks,
            heuristics: None,
            inline: &inline,
            breaking: Vec::new(),
            rust_api_delta: None,
            coverage: empty_coverage(),
            diff_dir: tmp.path(),
            skipped_checks: Vec::new(),
            out_dir: tmp.path(),
            diffs: &[],
            ownership_map: Vec::new(),
            clean_comparison: CleanComparison::for_test(true, true),
            snapshot_integrity: None,
            provenance: &provenance,
        });

        crate::artifacts::report::generate(&crate::artifacts::report::ReportInput {
            dir: tmp.path(),
            config: &config,
            diffs: &[],
            checks: &checks,
            resolved_target: &target,
            resolved_bases: &bases,
            ctx: &dashboard,
            run_started_at: "2026-09-11T00:00:00Z",
            heuristics: None,
            regression: None,
            scope: None,
            provenance: &provenance,
        })
        .expect("report.json");
        let report: serde_json::Value =
            serde_json::from_slice(&fs::read(tmp.path().join("report.json")).expect("read report"))
                .expect("parse report.json");
        let report_caveats: Vec<String> = report["gate"]["review_caveats"]
            .as_array()
            .expect("report review caveats")
            .iter()
            .map(|value| value.as_str().expect("caveat string").to_string())
            .collect();

        let expected_signals: Vec<&String> = expected.iter().collect();
        for (surface, caveats) in [
            ("MERGE_GATE.json decision.review_caveats", &gate_caveats),
            (
                "dashboard context review_caveats",
                &dashboard.review_caveats,
            ),
            ("report.json gate.review_caveats", &report_caveats),
        ] {
            let signals: Vec<&String> = caveats
                .iter()
                .filter(|caveat| {
                    caveat.starts_with(crate::artifacts::signal::PROVENANCE_CONTRADICTION_CODE)
                })
                .collect();
            assert_eq!(
                signals, expected_signals,
                "{surface} must carry the identical provenance review signal"
            );
        }
    }

    #[test]
    fn review_signals_markdown_lists_all_canonical_caveats_in_both_readers() {
        let (gate, index, caveats) = review_signal_documents(13);
        assert_eq!(caveats.len(), 13);
        for document in [gate, index] {
            assert!(document.contains("## Review signals (13)\n\n"));
            for caveat in &caveats {
                let entry = format!("- {}\n", caveat.replace('\n', "\n  "));
                assert!(document.contains(&entry), "missing signal: {caveat}");
            }
        }
    }

    #[test]
    fn review_signals_markdown_omits_empty_sections_in_both_readers() {
        let (gate, index, caveats) = review_signal_documents(0);
        assert!(caveats.is_empty());
        assert!(!gate.contains("## Review signals"));
        assert!(!index.contains("## Review signals"));
    }

    #[test]
    fn merge_gate_serializes_the_exact_shared_rust_api_view() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let config = test_config();
        let inline = InlineFindingsSummary {
            cargo_audit: None,
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
            scope: None,
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
            snapshot_integrity: None,
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
    fn run_gate_with_cached_semgrep_age_status(
        cache_age_secs: Option<u64>,
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
            cargo_audit: None,
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
                cache_age_secs,
                origin: SubstrateKey::default(),
            },
            queued_at: None,
            started_at: None,
        });

        generate_merge_gate(MergeGateInput {
            dir: tmp.path(),
            config: &config,
            ledger: &ledger,
            scope: None,
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
            snapshot_integrity: None,
        })
        .expect("merge gate");

        serde_json::from_slice(&std::fs::read(tmp.path().join("MERGE_GATE.json")).unwrap()).unwrap()
    }

    fn run_gate_with_cached_semgrep_status(
        cache_age_secs: u64,
        status: CheckStatus,
    ) -> serde_json::Value {
        run_gate_with_cached_semgrep_age_status(Some(cache_age_secs), status)
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
        assert_eq!(caveats[0]["age_status"], "stale");
    }

    /// Unknown age is not fresh age. A future mtime after clock rollback (or a
    /// legacy entry with no usable timestamp) must remain visible when its
    /// cached PASS supports a clean decision.
    #[test]
    fn a_passing_row_replayed_from_a_cache_of_unknown_age_is_named() {
        let gate = run_gate_with_cached_semgrep_age_status(None, CheckStatus::Passed);
        let caveats = gate["stale_cache_caveats"]
            .as_array()
            .expect("stale_cache_caveats is an array");

        assert_eq!(gate["checks"][0]["status"], "passed");
        assert_eq!(caveats.len(), 1, "one unverifiable passing row, one caveat");
        assert_eq!(caveats[0]["check_id"], "semgrep_scan");
        assert!(caveats[0]["cache_age_secs"].is_null());
        assert_eq!(caveats[0]["age_status"], "unknown");

        let fresh = run_gate_with_cached_semgrep_status(60, CheckStatus::Passed);
        assert_eq!(
            gate["decision"], fresh["decision"],
            "an unknown-age caveat remains advisory-only"
        );
        assert_eq!(gate["checks"], fresh["checks"]);
        assert_eq!(gate["inline_findings"], fresh["inline_findings"]);
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
            cargo_audit: None,
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
            scope: None,
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
            snapshot_integrity: None,
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
            cargo_audit: None,
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
            scope: None,
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
            snapshot_integrity: None,
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
            cargo_audit: None,
            status: "failed".to_string(),
            findings_count: 1,
            dashboard_findings: vec![DashboardFinding {
                file: None,
                line: None,
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
            scope: None,
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
            snapshot_integrity: None,
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
            cargo_audit: None,
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
            scope: None,
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
            snapshot_integrity: None,
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
            cargo_audit: None,
            status: "failed".to_string(),
            findings_count: 1,
            dashboard_findings: vec![DashboardFinding {
                file: None,
                line: None,
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
            scope: None,
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
            snapshot_integrity: None,
        })
        .expect("merge gate");

        let raw =
            std::fs::read_to_string(tmp.path().join("MERGE_GATE.json")).expect("read gate json");
        let gate: serde_json::Value = serde_json::from_str(&raw).expect("parse gate json");
        let dashboard = build_dashboard_context(DashboardContextInput {
            config: &config,
            scope: None,
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
            snapshot_integrity: None,
            provenance: &ProvenanceConsistency::default(),
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
            cargo_audit: None,
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
            scope: None,
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
            snapshot_integrity: None,
        })
        .expect("merge gate");
        let raw =
            std::fs::read_to_string(tmp.path().join("MERGE_GATE.json")).expect("read gate json");
        let gate: serde_json::Value = serde_json::from_str(&raw).expect("parse gate json");

        let dashboard = build_dashboard_context(DashboardContextInput {
            config: &config,
            scope: None,
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
            snapshot_integrity: None,
            provenance: &ProvenanceConsistency::default(),
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
        let downgraded = compute_effective_policy_outcome(
            &summary.evaluations,
            &preexisting,
            None,
            CargoAuditLockProof::TargetLock,
        );
        assert!(downgraded.blocking_issues.is_empty());
        assert!(downgraded.advisory_caveats.is_empty());
        assert_eq!(downgraded.worst_merge, MergeRecommendation::Approve);

        // Not pre-existing: the same failure keeps its policy impact.
        let kept = compute_effective_policy_outcome(
            &summary.evaluations,
            &std::collections::BTreeSet::new(),
            None,
            CargoAuditLockProof::TargetLock,
        );
        assert_eq!(kept.worst_merge, MergeRecommendation::ReviewRequired);
        assert_eq!(kept.advisory_caveats.len(), 1);
    }

    fn cargo_audit_evidence(
        status: &'static str,
        new: usize,
        preexisting: usize,
        unknown: usize,
        lock_changed: bool,
        new_advisories: &[&str],
    ) -> super::super::findings::CargoAuditGateEvidence {
        super::super::findings::CargoAuditGateEvidence {
            status,
            new,
            preexisting,
            unknown,
            lock_changed,
            new_advisories: new_advisories.iter().map(|id| id.to_string()).collect(),
        }
    }

    fn cargo_audit_check(status: CheckStatus) -> CheckResult {
        CheckResult {
            name: "Cargo audit".to_string(),
            status,
            duration: Duration::from_millis(900),
            output: "{}".to_string(),
            cached: false,
            provenance: None,
        }
    }

    /// The bridging sentence the incident pack was missing: a downgraded audit
    /// must SAY what made it pre-existing, and a blocking one must SAY what it
    /// is blocking on. `Cargo audit (Failed)` beside `new=0, pre-existing=2` is
    /// the contradiction this text exists to close.
    #[test]
    fn a_downgraded_cargo_audit_states_its_lock_proof() {
        use crate::policy::engine::PolicyEngine;

        let config = test_config();
        let engine = PolicyEngine::new(&config);
        let checks = vec![cargo_audit_check(CheckStatus::Failed)];
        let summary = engine.evaluate_all(&checks, &[]);
        let mut preexisting = std::collections::BTreeSet::new();
        preexisting.insert("Cargo audit");

        let untouched_lock = compute_effective_policy_outcome(
            &summary.evaluations,
            &preexisting,
            Some(&cargo_audit_evidence("not-required", 0, 2, 0, false, &[])),
            CargoAuditLockProof::TargetLock,
        );
        assert!(untouched_lock.blocking_issues.is_empty());
        let reason = untouched_lock.effective_evals[0]
            .reason
            .as_deref()
            .expect("a downgraded check must carry its reason");
        assert_eq!(
            reason,
            "pre-existing: Cargo.lock unchanged by this PR (2 advisories)"
        );

        let compared_lock = compute_effective_policy_outcome(
            &summary.evaluations,
            &preexisting,
            Some(&cargo_audit_evidence("available", 0, 1, 0, true, &[])),
            CargoAuditLockProof::TargetLock,
        );
        assert_eq!(
            compared_lock.effective_evals[0].reason.as_deref(),
            Some("pre-existing: unchanged vs base audit (1 advisory)"),
            "a changed lock cites the base comparison, not an untouched file"
        );

        // Every other check keeps the location-shaped sentence, which is true
        // of them and was never true of cargo audit.
        let others = compute_effective_policy_outcome(
            &engine.evaluate_all(&[semgrep_check()], &[]).evaluations,
            &std::collections::BTreeSet::from(["Semgrep scan"]),
            None,
            CargoAuditLockProof::TargetLock,
        );
        assert_eq!(
            others.effective_evals[0].reason.as_deref(),
            Some("pre-existing findings outside the change")
        );
    }

    #[test]
    fn a_blocking_cargo_audit_names_its_new_advisories() {
        use crate::policy::engine::PolicyEngine;

        let mut config = test_config();
        config.policy.mode = crate::policy::PolicyMode::Block;
        let engine = PolicyEngine::new(&config);
        let checks = vec![cargo_audit_check(CheckStatus::Failed)];
        let summary = engine.evaluate_all(&checks, &[]);
        let none_preexisting = std::collections::BTreeSet::new();

        let introduced = compute_effective_policy_outcome(
            &summary.evaluations,
            &none_preexisting,
            Some(&cargo_audit_evidence(
                "available",
                2,
                3,
                0,
                true,
                &[
                    "RUSTSEC-2026-0001 in openssl 0.9.0",
                    "RUSTSEC-2026-0002 in chrono 0.4.19",
                ],
            )),
            CargoAuditLockProof::TargetLock,
        );
        assert_eq!(
            introduced.blocking_issues,
            vec![
                "Cargo audit (Failed): 2 new advisories introduced \
                 (RUSTSEC-2026-0001 in openssl 0.9.0, RUSTSEC-2026-0002 in chrono 0.4.19), \
                 3 pre-existing"
                    .to_string()
            ],
            "the sentence names every advisory it counts, and calls the set what it is"
        );

        // An unknown base is a different sentence, because it is a different
        // fact: nothing was compared, so nothing was introduced.
        let unknown_base = compute_effective_policy_outcome(
            &summary.evaluations,
            &none_preexisting,
            Some(&cargo_audit_evidence("unavailable", 0, 0, 2, true, &[])),
            CargoAuditLockProof::TargetLock,
        );
        assert_eq!(
            unknown_base.blocking_issues,
            vec![
                "Cargo audit (Failed): 2 advisories with no base comparison \
                 (baseline unavailable)"
                    .to_string()
            ]
        );

        // Nothing to add: the proof held and the counts are empty, so the
        // blocker stays as bare as it always was rather than gaining a sentence
        // that asserts zero of everything.
        let silent = compute_effective_policy_outcome(
            &summary.evaluations,
            &none_preexisting,
            Some(&cargo_audit_evidence("not-required", 0, 0, 0, false, &[])),
            CargoAuditLockProof::TargetLock,
        );
        assert_eq!(
            silent.blocking_issues,
            vec!["Cargo audit (Failed)".to_string()]
        );
    }

    /// The incident in the revoke direction: an audit whose counts say
    /// `new=0, pre-existing=2` blocks because its provenance proof was
    /// withheld, and the decision used to render that as a bare
    /// `Cargo audit (Failed)` — the same mute blocker, reproduced by the very
    /// mechanism built to abolish it. Each gap states itself.
    #[test]
    fn a_withheld_lock_proof_is_stated_rather_than_left_unclassified() {
        use crate::policy::engine::PolicyEngine;

        let mut config = test_config();
        config.policy.mode = crate::policy::PolicyMode::Block;
        let engine = PolicyEngine::new(&config);
        let checks = vec![cargo_audit_check(CheckStatus::Failed)];
        let summary = engine.evaluate_all(&checks, &[]);
        let none_preexisting = std::collections::BTreeSet::new();
        let evidence = cargo_audit_evidence("not-required", 0, 2, 0, false, &[]);

        for (gap, expected) in [
            (
                LockProofGap::NoTargetLock,
                "Cargo audit (Failed): provenance proof unavailable: no Cargo.lock \
                 in the target tree (2 advisories not shown to predate this change)",
            ),
            (
                LockProofGap::DirtyLock,
                "Cargo audit (Failed): provenance proof unavailable: Cargo.lock dirty \
                 or rewritten in the scanned tree (2 advisories not shown to predate this change)",
            ),
            (
                LockProofGap::RelocatedCargoRoot,
                "Cargo audit (Failed): provenance proof unavailable: the reviewed commit \
                 moved the cargo root away from the configured one (2 advisories not \
                 shown to predate this change)",
            ),
            (
                LockProofGap::AuditConfigChanged,
                "Cargo audit (Failed): provenance proof unavailable: the cargo-audit \
                 configuration (.cargo/audit.toml) changed (2 advisories not shown to \
                 predate this change)",
            ),
            (
                LockProofGap::UnknownProvenance,
                "Cargo audit (Failed): provenance proof unavailable: the scanned tree \
                 could not be tied to the target commit (2 advisories not shown to \
                 predate this change)",
            ),
        ] {
            let outcome = compute_effective_policy_outcome(
                &summary.evaluations,
                &none_preexisting,
                Some(&evidence),
                CargoAuditLockProof::Unproven(gap),
            );
            assert_eq!(
                outcome.blocking_issues,
                vec![expected.to_string()],
                "{gap:?} must say why the audit could not be classified"
            );
        }

        // Counts still outrank the proof note: a named new advisory is the more
        // specific fact and keeps the sentence. It is not called "introduced",
        // though — new against the base audit, but without the proof the
        // audited lock is not shown to be the target's — and the missing
        // premise qualifies the count instead of disappearing behind it.
        let introduced = compute_effective_policy_outcome(
            &summary.evaluations,
            &none_preexisting,
            Some(&cargo_audit_evidence(
                "available",
                1,
                0,
                0,
                true,
                &["RUSTSEC-2026-0003 in serde 1.0.0"],
            )),
            CargoAuditLockProof::Unproven(LockProofGap::DirtyLock),
        );
        assert_eq!(
            introduced.blocking_issues,
            vec![
                "Cargo audit (Failed): 1 new advisory vs the base audit \
                 (RUSTSEC-2026-0003 in serde 1.0.0), 0 pre-existing; provenance proof \
                 unavailable: Cargo.lock dirty or rewritten in the scanned tree"
                    .to_string()
            ]
        );
        for issue in &introduced.blocking_issues {
            assert!(
                !issue.contains("introduced"),
                "an unproven lock does not license \"introduced\": {issue}"
            );
        }
    }

    /// The zero-count shapes of the same blocking row: the sentence must name
    /// the fact that actually withheld the classification, and must not assert
    /// a count it does not have.
    ///
    /// Two distinct causes used to render as one. When `cargo audit` produces
    /// no readable report the baseline collapses to
    /// `status=current-unavailable` with every count zero — the report is the
    /// missing thing, not the lockfile — yet the row blocked with a sentence
    /// about lock provenance plus `(0 advisories not shown to predate this
    /// change)`. That is the wrong cause and an assertion of zero, which is
    /// exactly what `cargo_audit_blocker_detail`'s contract forbids.
    #[test]
    fn a_zero_count_blocker_names_its_cause_without_asserting_a_count() {
        use crate::policy::engine::PolicyEngine;

        let mut config = test_config();
        config.policy.mode = crate::policy::PolicyMode::Block;
        let engine = PolicyEngine::new(&config);
        let checks = vec![cargo_audit_check(CheckStatus::Failed)];
        let summary = engine.evaluate_all(&checks, &[]);
        let none_preexisting = std::collections::BTreeSet::new();

        // An unreadable report: the cause is the report, whatever the lock did.
        let unreadable = cargo_audit_evidence("current-unavailable", 0, 0, 0, false, &[]);
        for gap in [
            LockProofGap::NoTargetLock,
            LockProofGap::DirtyLock,
            LockProofGap::RelocatedCargoRoot,
            LockProofGap::AuditConfigChanged,
            LockProofGap::UnknownProvenance,
        ] {
            let outcome = compute_effective_policy_outcome(
                &summary.evaluations,
                &none_preexisting,
                Some(&unreadable),
                CargoAuditLockProof::Unproven(gap),
            );
            assert_eq!(
                outcome.blocking_issues,
                vec![
                    "Cargo audit (Failed): no readable advisory report (baseline \
                     current-unavailable), so no advisory could be classified either way"
                        .to_string()
                ],
                "{gap:?}: the unreadable report is the cause, not the lockfile"
            );
        }

        // A readable report with nothing in it: the gap is the cause and it is
        // named, but there is no count to qualify, so none is printed.
        let empty = cargo_audit_evidence("not-required", 0, 0, 0, false, &[]);
        let outcome = compute_effective_policy_outcome(
            &summary.evaluations,
            &none_preexisting,
            Some(&empty),
            CargoAuditLockProof::Unproven(LockProofGap::NoTargetLock),
        );
        assert_eq!(
            outcome.blocking_issues,
            vec![
                "Cargo audit (Failed): provenance proof unavailable: no Cargo.lock \
                 in the target tree"
                    .to_string()
            ]
        );
        for issue in &outcome.blocking_issues {
            assert!(
                !issue.contains("0 advisor"),
                "a zero count is never asserted: {issue}"
            );
        }
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

        assert_eq!(gate["decision"]["verdict"].as_str(), Some("CONDITIONAL"));
        assert_eq!(gate["decision"]["allow_merge"].as_bool(), Some(false));
        assert_eq!(
            gate["decision"]["analysis_status"].as_str(),
            Some("degraded")
        );
        assert_eq!(
            gate["decision"]["merge_recommendation"].as_str(),
            Some("approve")
        );
        assert_eq!(
            gate["decision"]["enforcement_disposition"].as_str(),
            Some("review_required")
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
        let outcome = compute_effective_policy_outcome(
            &summary.evaluations,
            &preexisting,
            None,
            CargoAuditLockProof::TargetLock,
        );
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
            cargo_audit: None,
            status: "warnings".to_string(),
            findings_count: 1,
            dashboard_findings: vec![DashboardFinding {
                file: None,
                line: None,
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
            scope: None,
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
            snapshot_integrity: None,
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
            cargo_audit: None,
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
            scope: None,
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
            snapshot_integrity: None,
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
            cargo_audit: None,
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
            scope: None,
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
            snapshot_integrity: None,
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

    /// Schema 3.1: the gate row for a check that owns an ecosystem's test scope
    /// states how much of that suite ran and why. The merge gate is where a
    /// reviewer decides whether the evidence is enough, so a narrowed suite
    /// must be visible exactly there.
    #[test]
    fn the_gate_row_publishes_the_test_scope_of_the_checks_that_own_one() {
        use crate::checks::scope::{ScopeDecision, ScopeDecisions};

        let tmp = tempfile::tempdir().expect("tempdir");
        let summary = tmp.path().join("00_summary");
        fs::create_dir(&summary).expect("summary directory");
        let config = test_config();
        let inline = InlineFindingsSummary {
            cargo_audit: None,
            status: "passed".into(),
            findings_count: 0,
            dashboard_findings: vec![],
        };
        let coverage = empty_coverage();
        let (target, bases) = resolved_refs();
        let checks = ["Cargo test", "Clippy"].map(|name| CheckResult {
            name: name.to_string(),
            status: CheckStatus::Passed,
            duration: std::time::Duration::from_secs(1),
            output: String::new(),
            cached: false,
            provenance: None,
        });
        let scope = ScopeDecisions {
            cargo: ScopeDecision::Full {
                reason: "manifest or lockfile changed: Cargo.lock".to_string(),
                inputs: Some(3),
            },
            vitest: ScopeDecision::Full {
                reason: "no JavaScript or TypeScript source detected".to_string(),
                inputs: Some(3),
            },
            non_participating: vec![crate::checks::scope::NonParticipatingPath {
                path: "CHANGELOG.md".to_string(),
                rule: "root-changelog".to_string(),
            }],
        };

        generate_merge_gate(MergeGateInput {
            dir: &summary,
            config: &config,
            ledger: &empty_ledger(),
            scope: Some(&scope),
            checks: &checks,
            heuristics: None,
            inline: &inline,
            breaking: &[],
            rust_api_delta: None,
            coverage: &coverage,
            diffs: &[],
            skipped_checks: &[],
            resolved_target: &target,
            resolved_bases: &bases,
            clean_comparison: CleanComparison::for_test(true, true),
            snapshot_integrity: None,
        })
        .expect("merge gate");

        let gate: serde_json::Value =
            serde_json::from_slice(&fs::read(summary.join("MERGE_GATE.json")).unwrap()).unwrap();
        assert_eq!(gate["schema_version"], "3.1");
        let rows = gate["checks"].as_array().expect("gate rows");
        let cargo_test = rows
            .iter()
            .find(|row| row["name"] == "Cargo test")
            .expect("cargo test row");
        assert_eq!(cargo_test["scope"]["mode"], "full");
        assert_eq!(
            cargo_test["scope"]["reason"],
            "manifest or lockfile changed: Cargo.lock"
        );
        assert!(
            cargo_test["scope"]["selected"].is_null(),
            "a full run selected nothing and must not report a selection count"
        );
        assert_eq!(
            cargo_test["scope"]["non_participating"][0]["path"], "CHANGELOG.md",
            "the gate row must name every path kept out of test selection"
        );
        assert_eq!(
            cargo_test["scope"]["non_participating"][0]["rule"], "root-changelog",
            "and the rule that decided it, so the call can be challenged"
        );
        let clippy = rows
            .iter()
            .find(|row| row["name"] == "Clippy")
            .expect("clippy row");
        assert!(
            clippy.get("scope").is_none(),
            "a check with no test suite to scope must not claim a scope"
        );
        let md = fs::read_to_string(summary.join("MERGE_GATE.md")).expect("gate markdown");
        assert!(
            md.contains("## Test scope")
                && md.contains("`CHANGELOG.md`")
                && md.contains("`root-changelog`"),
            "the human gate must show path -> rule too, got:\n{md}"
        );
        assert!(
            gate["decision"]["review_caveats"]
                .as_array()
                .expect("caveat list")
                .iter()
                .all(|caveat| !caveat
                    .as_str()
                    .unwrap_or_default()
                    .contains("change-scoped")),
            "no run was narrowed, so no narrowing caveat may be raised"
        );
    }
}
