//! Merge decision: verdict computation, quality-failure classification, review caveats.

use super::*;

// ── Dashboard context ──────────────────────────────────────────────

/// Per-check gate info for dashboard display.
pub(crate) struct CheckGateEntry {
    pub name: String,
    pub id: String,
    pub blocking: bool,
    pub class: &'static str,
    pub severity: &'static str,
}

/// Inline finding for dashboard display.
#[derive(Debug, Clone)]
pub(crate) struct DashboardFinding {
    pub level: &'static str,
    pub check_name: String,
    pub check_id: String,
    pub message: String,
    pub in_diff: Option<bool>,
    /// Source location reported by the tool, never inferred from a log filename.
    pub file: Option<String>,
    pub line: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum QualityFailureClass {
    Introduced,
    Preexisting,
    Mixed,
    Unclassified,
}

impl QualityFailureClass {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            QualityFailureClass::Introduced => "introduced",
            QualityFailureClass::Preexisting => "pre-existing",
            QualityFailureClass::Mixed => "mixed",
            QualityFailureClass::Unclassified => "unclassified",
        }
    }
}

/// Which check status produced a quality-summary entry.
///
/// The summary deliberately mixes two kinds of signal: hard failures
/// (`Failed`/`Error`) and warning-level baseline signals (`Warnings`) that are
/// admitted so the pre-existing downgrade can be computed for them. Only the
/// first kind may fail the quality gate — a warning is an advisory signal by
/// definition, and calling it a failure was the "warning→failure" lie.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum QualityFailureOrigin {
    /// The check reported `Failed` or `Error`.
    Failure,
    /// The check reported `Warnings`.
    Warning,
}

impl QualityFailureOrigin {
    /// Wire name used in `MERGE_GATE.json`.
    ///
    /// The origin is not an internal detail: without it a consumer reading
    /// `introduced_quality_failures: ["Rustfmt"]` next to `quality_pass: true`
    /// sees a self-contradicting pack, because the array says "failure" and the
    /// flag says the entry never gated. Naming the origin is what makes the two
    /// readable together.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Failure => "failure",
            Self::Warning => "warning",
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct QualityFailureDetail {
    pub name: String,
    pub classification: QualityFailureClass,
    pub origin: QualityFailureOrigin,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct QualityFailureSummary {
    pub(crate) quality_failures: Vec<String>,
    pub(crate) introduced_quality_failures: Vec<String>,
    pub(crate) preexisting_quality_failures: Vec<String>,
    pub(crate) mixed_quality_failures: Vec<String>,
    pub(crate) unclassified_quality_failures: Vec<String>,
    pub(crate) details: Vec<QualityFailureDetail>,
}

impl QualityFailureSummary {
    /// Returns true when there are FAILURES that are new or indeterminate.
    ///
    /// Two independent filters apply, and both are load-bearing:
    ///
    /// * **Origin.** Only entries whose check actually failed
    ///   (`QualityFailureOrigin::Failure`) can fail the quality gate. Entries
    ///   admitted from `Warnings` checks are here purely so the pre-existing
    ///   downgrade can be computed for them; a warning is advisory by
    ///   definition and must never be reported as a failed quality check —
    ///   regardless of how it classifies, `Unclassified` included. It still
    ///   reaches the verdict through the policy engine (Warnings → Advisory →
    ///   ReviewRequired), which keeps a CONDITIONAL verdict; what changes is
    ///   the truth of the label, not the verdict.
    /// * **Classification.** Purely pre-existing failures do NOT count — they
    ///   existed before this diff and should not block the gate. Introduced,
    ///   mixed, and unclassified failures are all considered "new" because they
    ///   either definitely or possibly originate from the current change
    ///   (fail-closed).
    pub(crate) fn has_new_failures(&self) -> bool {
        self.details.iter().any(|detail| {
            detail.origin == QualityFailureOrigin::Failure
                && !matches!(detail.classification, QualityFailureClass::Preexisting)
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MergeDecisionState {
    Allow,
    AllowWithReview,
    Hold,
    Block,
}

impl MergeDecisionState {
    pub(crate) fn hero_class(self) -> &'static str {
        match self {
            MergeDecisionState::Allow => "merge-allow",
            MergeDecisionState::AllowWithReview => "merge-review",
            MergeDecisionState::Hold => "merge-hold",
            MergeDecisionState::Block => "merge-block",
        }
    }

    pub(crate) fn hero_label(self) -> &'static str {
        match self {
            MergeDecisionState::Allow => "ALLOW MERGE",
            MergeDecisionState::AllowWithReview => "ALLOW WITH REVIEW",
            MergeDecisionState::Hold => "HOLD MERGE",
            MergeDecisionState::Block => "BLOCK MERGE",
        }
    }

    pub(crate) fn card_badge_class(self) -> &'static str {
        match self {
            MergeDecisionState::Allow => "mdb-pass",
            MergeDecisionState::AllowWithReview | MergeDecisionState::Hold => "mdb-hold",
            MergeDecisionState::Block => "mdb-fail",
        }
    }

    pub(crate) fn card_label(self) -> &'static str {
        match self {
            MergeDecisionState::Allow => "GO",
            MergeDecisionState::AllowWithReview => "GO WITH REVIEW",
            MergeDecisionState::Hold => "HOLD",
            MergeDecisionState::Block => "BLOCK",
        }
    }

    pub(crate) fn gate_label(self) -> &'static str {
        match self {
            MergeDecisionState::Allow => "MERGE",
            MergeDecisionState::AllowWithReview => "MERGE WITH REVIEW",
            MergeDecisionState::Hold => "HOLD",
            MergeDecisionState::Block => "BLOCK",
        }
    }

    pub(crate) fn card_class(self) -> &'static str {
        match self {
            MergeDecisionState::Allow => "alert-success",
            MergeDecisionState::AllowWithReview | MergeDecisionState::Hold => "alert-warning",
            MergeDecisionState::Block => "alert-error",
        }
    }
}

pub(crate) struct MergeDecisionView {
    pub state: MergeDecisionState,
    pub reason: String,
    pub review_caveats: Vec<String>,
}

/// The single coherent derivation of the scalar decision fields from the two
/// authoritative axes (`analysis_status` + `merge_recommendation`) plus
/// `quality_pass`.
///
/// `allow_merge` is DERIVED here and nowhere else: it is true **iff** the
/// verdict is a clean `PASS`. This makes the contradictory state
/// `allow_merge: true` beside a `CONDITIONAL`/`BLOCK` verdict unrepresentable
/// (PV-03). The separate "policy did not hard-block" axis (`policy_allow_merge`)
/// stays owned by the caller and is not conflated with the recommendation.
#[derive(Debug, Clone, Copy)]
pub(crate) struct DerivedDecision {
    pub verdict: &'static str,
    pub allow_merge: bool,
    pub recommended_merge: bool,
}

pub(crate) fn derive_decision(
    analysis_status: crate::policy::engine::AnalysisStatus,
    merge_recommendation: crate::policy::engine::MergeRecommendation,
    quality_pass: bool,
) -> DerivedDecision {
    let verdict = merge_recommendation.legacy_verdict(analysis_status, quality_pass);
    DerivedDecision {
        verdict,
        allow_merge: verdict == "PASS",
        recommended_merge: merge_recommendation.legacy_recommended_merge(),
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct BreakingChangeBreakdown {
    pub removed_symbols: usize,
    pub signature_changes: usize,
    pub new_env_requirements: usize,
    /// Symbols that moved to another file (same name + kind) and are typically
    /// still re-exported — non-breaking. Surfaced as a clarifier so the gate
    /// caveat does not report module splits as mass removals (P1-08).
    pub relocated_symbols: usize,
}

impl BreakingChangeBreakdown {
    pub fn has_any(&self) -> bool {
        // Relocated symbols alone are non-breaking and must not raise a caveat.
        self.removed_symbols > 0 || self.signature_changes > 0 || self.new_env_requirements > 0
    }

    pub fn summary_parts(&self) -> Vec<String> {
        let mut parts = Vec::new();
        if self.removed_symbols > 0 {
            parts.push(format!(
                "{} removed public symbol{}",
                self.removed_symbols,
                if self.removed_symbols == 1 { "" } else { "s" }
            ));
        }
        if self.signature_changes > 0 {
            parts.push(format!(
                "{} signature change{}",
                self.signature_changes,
                if self.signature_changes == 1 { "" } else { "s" }
            ));
        }
        if self.new_env_requirements > 0 {
            parts.push(format!(
                "{} new env requirement{}",
                self.new_env_requirements,
                if self.new_env_requirements == 1 {
                    ""
                } else {
                    "s"
                }
            ));
        }
        // Only consumed when has_any() is true, so a relocation-only diff stays
        // caveat-free; alongside real breaks it clarifies module-move noise.
        if self.relocated_symbols > 0 {
            parts.push(format!(
                "{} relocated/re-exported (non-breaking)",
                self.relocated_symbols
            ));
        }
        parts
    }

    pub fn summary(&self) -> Option<String> {
        let parts = self.summary_parts();
        if parts.is_empty() {
            None
        } else {
            Some(parts.join(", "))
        }
    }
}

pub(crate) fn breaking_change_breakdown(breaking: &[BreakingFinding]) -> BreakingChangeBreakdown {
    let removed_symbols = breaking
        .iter()
        .filter(|f| matches!(&f.kind, BreakingKind::RemovedSymbol { .. }))
        .count();
    let signature_changes = breaking
        .iter()
        .filter(|f| matches!(&f.kind, BreakingKind::ChangedSignature { .. }))
        .count();
    let relocated_symbols = breaking
        .iter()
        .filter(|f| matches!(&f.kind, BreakingKind::RelocatedSymbol { .. }))
        .count();
    let new_env_requirements = breaking
        .iter()
        .filter(|f| matches!(&f.kind, BreakingKind::NewEnvRequirement { .. }))
        .count();

    BreakingChangeBreakdown {
        removed_symbols,
        signature_changes,
        new_env_requirements,
        relocated_symbols,
    }
}

/// Escalate the merge axis when the diff carries genuine breaking API changes.
///
/// Real breaking findings (removed public symbols, changed signatures, new env
/// requirements — relocations are excluded, they are non-breaking) raise the
/// merge recommendation from `Approve` to `ReviewRequired`, which turns the
/// verdict PASS → CONDITIONAL. It NEVER produces a `Block` and NEVER downgrades
/// an axis that is already `ReviewRequired`/`Block` for another reason: the axis
/// only ratchets upward.
///
/// Gated by the `[gate] breaking_escalation` knob (default on). When the knob is
/// off this is a no-op and returns `None`, so the breaking findings still surface
/// as an informational caveat (via `build_review_caveats`) with no verdict impact.
///
/// Returns the explicit escalation reason caveat when it fired, so every artifact
/// surface (console, report.json, MERGE_GATE.json) can show the identical reason.
pub(crate) fn apply_breaking_escalation(
    enabled: bool,
    breaking: &[BreakingFinding],
    worst_merge: &mut crate::policy::engine::MergeRecommendation,
) -> Option<String> {
    use crate::policy::engine::MergeRecommendation;

    if !enabled {
        return None;
    }
    let breakdown = breaking_change_breakdown(breaking);
    if !breakdown.has_any() {
        return None;
    }
    let count =
        breakdown.removed_symbols + breakdown.signature_changes + breakdown.new_env_requirements;
    if *worst_merge == MergeRecommendation::Approve {
        *worst_merge = MergeRecommendation::ReviewRequired;
    }
    Some(format!(
        "breaking API change detected: {} finding{}",
        count,
        if count == 1 { "" } else { "s" }
    ))
}

/// Apply the existing breaking-escalation policy to the canonical Rust API
/// delta. Added-only deltas are informational. Unknown facts always degrade
/// confidence and require review because absence cannot be proven; this does
/// not change any policy default or create a blocking outcome.
pub(crate) fn apply_rust_api_delta_outcome(
    enabled: bool,
    view: Option<&api_delta::ApiArtifactView>,
    worst_confidence: &mut crate::policy::engine::AnalysisStatus,
    worst_merge: &mut crate::policy::engine::MergeRecommendation,
) -> crate::policy::engine::EnforcementDisposition {
    use crate::policy::engine::{AnalysisStatus, EnforcementDisposition, MergeRecommendation};

    let Some(view) = view else {
        return EnforcementDisposition::Clean;
    };
    let confirmed_breaking = view.findings.iter().any(|finding| {
        finding.confidence == api_delta::ApiDeltaConfidence::Confirmed
            && matches!(
                finding.kind,
                api_delta::ApiDeltaKind::Removed
                    | api_delta::ApiDeltaKind::Changed
                    | api_delta::ApiDeltaKind::Relocated
                    | api_delta::ApiDeltaKind::VisibilityChanged
            )
    });
    if enabled && confirmed_breaking && *worst_merge == MergeRecommendation::Approve {
        *worst_merge = MergeRecommendation::ReviewRequired;
    }

    if view.counts.unknown > 0 {
        if *worst_confidence == AnalysisStatus::Complete {
            *worst_confidence = AnalysisStatus::Degraded;
        }
        if *worst_merge == MergeRecommendation::Approve {
            *worst_merge = MergeRecommendation::ReviewRequired;
        }
    }

    if (enabled && confirmed_breaking) || view.counts.unknown > 0 {
        EnforcementDisposition::ReviewRequired
    } else {
        EnforcementDisposition::Clean
    }
}

/// Exact operator caveats derived from the same serialized view used by both
/// API artifacts. IDs are included so consumers can join caveats to evidence
/// without recounting or reparsing Markdown.
pub(crate) fn rust_api_delta_review_caveats(
    view: Option<&api_delta::ApiArtifactView>,
) -> Vec<String> {
    let Some(view) = view else {
        return Vec::new();
    };
    let breaking = view
        .findings
        .iter()
        .filter(|finding| {
            finding.confidence == api_delta::ApiDeltaConfidence::Confirmed
                && matches!(
                    finding.kind,
                    api_delta::ApiDeltaKind::Removed
                        | api_delta::ApiDeltaKind::Changed
                        | api_delta::ApiDeltaKind::Relocated
                        | api_delta::ApiDeltaKind::VisibilityChanged
                )
        })
        .collect::<Vec<_>>();
    let unknown = view
        .findings
        .iter()
        .filter(|finding| finding.confidence == api_delta::ApiDeltaConfidence::Unknown)
        .collect::<Vec<_>>();
    let mut caveats = Vec::new();
    if !breaking.is_empty() {
        caveats.push(format!(
            "Rust API delta: {} confirmed breaking finding{} [{}]",
            breaking.len(),
            if breaking.len() == 1 { "" } else { "s" },
            breaking
                .iter()
                .map(|finding| finding.id.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if !unknown.is_empty() {
        caveats.push(format!(
            "Rust API delta: {} unknown finding{} [{}]",
            unknown.len(),
            if unknown.len() == 1 { "" } else { "s" },
            unknown
                .iter()
                .map(|finding| finding.id.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    caveats
}

pub(crate) fn build_review_caveats(
    breaking: &[BreakingFinding],
    coverage: &CoverageDelta,
    findings_count: usize,
) -> Vec<String> {
    let mut caveats = Vec::new();

    let breaking_breakdown = breaking_change_breakdown(breaking);
    if breaking_breakdown.has_any() {
        caveats.push(breaking_breakdown.summary_parts().join(" · "));
    }

    if let Some(pct) = coverage.pct
        && pct < 80
    {
        let mut coverage_caveat = format!("{}% coverage heuristic", pct);
        if coverage_has_rust_inline_test_blind_spot(coverage) {
            coverage_caveat.push_str(" (Rust inline #[cfg(test)] modules may be missed)");
        }
        caveats.push(coverage_caveat);
    }

    if !coverage.ghost_tests.is_empty() {
        caveats.push(format!(
            "{} orphaned test candidate{}",
            coverage.ghost_tests.len(),
            if coverage.ghost_tests.len() == 1 {
                ""
            } else {
                "s"
            }
        ));
    }

    if findings_count > 0 {
        caveats.push(format!(
            "{} inline finding{}",
            findings_count,
            if findings_count == 1 { "" } else { "s" }
        ));
    }

    caveats
}

pub(crate) fn rust_quality_review_caveats(
    _config: &Config,
    _checks: &[CheckResult],
) -> Vec<String> {
    // Rust quality signal gaps are now handled by PolicyEngine::evaluate_skip().
    // Skipped checks with block/warn severity produce review caveats automatically.
    Vec::new()
}

pub(crate) fn cargo_audit_review_caveats(checks: &[CheckResult]) -> Vec<String> {
    checks
        .iter()
        .find(|c| c.name.eq_ignore_ascii_case("cargo audit"))
        .and_then(|check| cargo_audit_informational_summary(&check.output))
        .map(|summary| vec![format!("Cargo audit note: {summary}")])
        .unwrap_or_default()
}

pub(crate) fn semgrep_partial_parse_review_caveats(checks: &[CheckResult]) -> Vec<String> {
    let Some(check) = checks
        .iter()
        .find(|check| check.name.eq_ignore_ascii_case("semgrep scan"))
        .filter(|check| crate::checks::semgrep_output_reports_scan_errors(&check.output))
    else {
        return Vec::new();
    };

    let paths = crate::checks::semgrep_scan_error_paths(&check.output);
    let detail = if paths.is_empty() {
        "affected file names were not present in Semgrep output".to_string()
    } else {
        let shown = paths
            .iter()
            .take(10)
            .cloned()
            .collect::<Vec<_>>()
            .join(", ");
        if paths.len() > 10 {
            format!("{shown}, +{} more", paths.len() - 10)
        } else {
            shown
        }
    };
    vec![format!(
        "Semgrep analysis was partial; incompletely parsed files: {detail}"
    )]
}

/// The baseline counts as a review caveat — qualified by the proof that earned
/// them, or by the gap that did not.
///
/// `cargo_audit_baseline_counts` classifies against the lockfile, and
/// `pre-existing=N` is a PROVENANCE claim: these advisories predate the change.
/// [`CargoAuditLockProof`] is what makes that claim true, so when the proof is
/// `Unproven` the number is still the right count of advisories and the wrong
/// thing to state as settled. Left bare it contradicted the blocker standing
/// beside it in the same decision — `pre-existing=2` against `2 advisories not
/// shown to predate this change` — with `preexisting_quality_failures` empty
/// and nothing telling the reader which surface to believe.
///
/// The qualifier is rendered from [`LockProofGap::gate_note`], the same words
/// the blocking line uses, so the two surfaces cannot drift into describing one
/// gap two ways. It also repairs a second misreading the counts carry on their
/// own: `status=not-required` means "the lock did not change, so no base audit
/// was needed", which about a target with NO lockfile reads as an untouched
/// file rather than an absent one. Naming the absent lockfile in the same
/// sentence settles it.
///
/// The counts themselves are untouched — classification is the proof's job, not
/// this renderer's. This is the single place the note's text is published (the
/// `note`-level finding it clones is filtered out of `operator_findings`), so
/// qualifying it here leaves no second, unqualified copy behind.
pub(super) fn cargo_audit_baseline_review_caveats(
    inline: &InlineFindingsSummary,
    lock_proof: CargoAuditLockProof,
) -> Vec<String> {
    inline
        .dashboard_findings
        .iter()
        .find(|finding| finding.check_id == "cargo_audit_baseline")
        .map(|finding| {
            let counts = &finding.message;
            let CargoAuditLockProof::Unproven(gap) = lock_proof else {
                return vec![counts.clone()];
            };
            let note = gap.gate_note();
            // The number comes from `InlineFindingsSummary::cargo_audit`, the
            // same structured value the note above was rendered from — never
            // parsed back out of that human-facing string, which would make
            // this surface depend on its wording.
            let preexisting = inline
                .cargo_audit
                .as_ref()
                .map_or(0, |audit| audit.preexisting);
            // Nothing to qualify when nothing was claimed: a zero count is not
            // a provenance claim, so it does not get a disclaimer either.
            let caveat = if preexisting > 0 {
                format!(
                    "{counts} ({note}; pre-existing={preexisting} is not shown to \
                     predate this change)"
                )
            } else {
                format!("{counts} ({note})")
            };
            vec![caveat]
        })
        .unwrap_or_default()
}

#[cfg(test)]
#[test]
fn semgrep_partial_caveat_names_unparsed_files() {
    let checks = [CheckResult {
        name: "Semgrep scan".to_string(),
        status: CheckStatus::Warnings,
        duration: std::time::Duration::ZERO,
        output: r#"{"results":[],"errors":[{"path":"src/ffi.rs"}]}"#.to_string(),
        cached: false,
        provenance: None,
    }];
    let caveats = semgrep_partial_parse_review_caveats(&checks);
    assert_eq!(caveats.len(), 1);
    assert!(caveats[0].contains("src/ffi.rs"));
}

pub(crate) fn build_merge_decision_view(
    policy_allow_merge: bool,
    quality_pass: bool,
    recommended_merge: bool,
    quality_failures: &[String],
    quality_failure_details: &[QualityFailureDetail],
    blocking_issues: &[String],
    review_caveats: Vec<String>,
) -> MergeDecisionView {
    // Mirrors `QualityFailureSummary::has_new_failures`: only a real failure
    // (not a warning-level signal) that is new or indeterminate holds the merge.
    // Keeping the two predicates aligned is what stops the hero label from
    // reading HOLD while `quality_pass` is true.
    let has_new_quality_failures = quality_failure_details.iter().any(|detail| {
        detail.origin == QualityFailureOrigin::Failure
            && !matches!(detail.classification, QualityFailureClass::Preexisting)
    }) || (quality_failure_details.is_empty()
        && !quality_failures.is_empty());

    let state = if !policy_allow_merge {
        MergeDecisionState::Block
    } else if recommended_merge {
        if review_caveats.is_empty() {
            MergeDecisionState::Allow
        } else {
            MergeDecisionState::AllowWithReview
        }
    } else if has_new_quality_failures {
        // New or unclassified failures belong to the change and remain a HOLD.
        // Pure pre-existing findings are advisory only.
        MergeDecisionState::Hold
    } else if !review_caveats.is_empty() {
        // Policy permits the merge and no check actually failed — only advisory
        // review signals remain (warnings, inline findings, audit notes). This
        // is "mergeable with advisories", NOT a hold: the label must not read
        // like a stop sign when status is ALLOW and allow_merge is true.
        MergeDecisionState::AllowWithReview
    } else {
        // Not an explicit approve and nothing concrete to show - stay
        // conservative and hold for human review.
        MergeDecisionState::Hold
    };

    let reason = match state {
        MergeDecisionState::Allow => "All quality gates passed".to_string(),
        MergeDecisionState::AllowWithReview => format!(
            "{}{} review signal{} need attention",
            quality_failure_reason_text(quality_failures, quality_failure_details)
                .map(|reason| format!("{reason}; "))
                .unwrap_or_else(|| "Quality gates passed, but ".to_string()),
            review_caveats.len(),
            if review_caveats.len() == 1 { "" } else { "s" }
        ),
        MergeDecisionState::Hold => {
            if !quality_pass && !quality_failures.is_empty() {
                quality_failure_reason_text(quality_failures, quality_failure_details)
                    .unwrap_or_else(|| {
                        format!(
                            "{} quality check{} failed: {}",
                            quality_failures.len(),
                            if quality_failures.len() == 1 { "" } else { "s" },
                            quality_failures.join(", ")
                        )
                    })
            } else if !review_caveats.is_empty() {
                format!(
                    "{}review required: {} signal{} need attention",
                    quality_failure_reason_text(quality_failures, quality_failure_details)
                        .map(|reason| format!("{reason}; "))
                        .unwrap_or_default(),
                    review_caveats.len(),
                    if review_caveats.len() == 1 { "" } else { "s" }
                )
            } else {
                "Merge not recommended".to_string()
            }
        }
        MergeDecisionState::Block => {
            if !blocking_issues.is_empty() {
                format!(
                    "{} blocking issue{} found: {}",
                    blocking_issues.len(),
                    if blocking_issues.len() == 1 { "" } else { "s" },
                    blocking_issues.join(", ")
                )
            } else if !quality_pass {
                "Blocking policy violations detected".to_string()
            } else {
                "Merge blocked by policy".to_string()
            }
        }
    };

    MergeDecisionView {
        state,
        reason,
        review_caveats,
    }
}

/// Whether a check's finding *locations* are an exhaustive baseline signal — so
/// that "every reported location lies outside the diff" genuinely proves the
/// failure is pre-existing debt and may be downgraded off the merge gate.
///
/// True only for per-location scanners, linters and formatters (semgrep,
/// eslint, stylelint, ruff, prettier, rustfmt, cargo audit) where each finding
/// is an independent, locally-scoped issue whose absence from the diff means it
/// predates the change. Formatters qualify because `cargo fmt --check` /
/// `prettier --check` report per-file format deltas that do not depend on
/// compiling the whole project.
///
/// False (the safe default) for whole-project gates — `cargo test`, `cargo
/// check`, `clippy`, `tsc`, `vitest`/`tests`, `pytest`, type checkers (`mypy`)
/// — where a single boolean failure can be *caused* by the diff even though the
/// failing location sits in an unchanged file. `clippy` belongs here, NOT with
/// the formatters: `cargo clippy -- -D warnings` is also a whole-project
/// compile gate, so a public-API change in the diff can break compilation of a
/// downstream module outside the diff. For these the location set is
/// symptomatic, not exhaustive, so a pure out-of-diff failure must never be
/// trusted as pre-existing.
pub(crate) fn check_id_is_baseline_signal(check_id: &str) -> bool {
    matches!(
        check_id,
        "semgrep_scan" | "eslint" | "stylelint" | "ruff" | "prettier" | "rustfmt" | "cargo_audit"
    )
}

/// Whether the `Cargo.lock` the live `cargo audit` read is provably the
/// analysed target's lockfile.
///
/// This is cargo audit's substrate proof, and it exists because R2-9 — "a dirty
/// worktree can pass an uncommitted finding off as out-of-diff" — is the wrong
/// evidence for this check. A `rustfmt`/`eslint`/`semgrep` finding IS a source
/// file, so uncommitted source bytes can forge its out-of-diff position. A
/// cargo-audit advisory is not: it lives in `Cargo.lock` × the advisory
/// database, and no amount of edited source can move it. Gating it on
/// whole-tree cleanliness made the pack state `new=0, pre-existing=2` in the
/// baseline caveat and `BLOCK … Cargo audit (Failed)` in the decision, with
/// nothing bridging the two.
///
/// The proof that IS load-bearing is lockfile provenance, and it covers every
/// branch of the baseline comparison at once, because `in_diff` already carries
/// the rest of the evidence:
///
/// * lock untouched by the diff → every advisory is `in_diff == Some(false)`,
///   and the downgrade is sound exactly when the scanned lock was the target's;
/// * lock changed with a base audit → `in_diff` is a real `current ∖ base`
///   comparison, sound under the same condition and no other;
/// * lock changed with no base audit → every row is `in_diff == None`, so
///   R5-23 keeps the check Unclassified whatever this proof says (R3-14/R4-20
///   likewise stay in force upstream of it).
///
/// A freshly published advisory against an unchanged lock is therefore
/// pre-existing debt newly revealed, not debt this PR introduced — the correct
/// reading, since the PR did not touch a single dependency.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CargoAuditLockProof {
    /// The audited lockfile is the target's: either the run scanned a snapshot
    /// materialised at the target commit, or it scanned the local checkout and
    /// the lockfile itself carried no uncommitted change.
    TargetLock,
    /// Provenance not established — no downgrade. The variant names WHICH
    /// premise failed, because a check that blocks for want of a proof owes the
    /// reader that sentence: `Cargo audit (Failed)` beside `new=0,
    /// pre-existing=2` is exactly the mute blocker this line of work exists to
    /// close, and revoking a proof reproduces it in the other direction.
    Unproven(LockProofGap),
}

/// Which premise of the lockfile proof was missing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LockProofGap {
    /// The target commit's tree carries no `Cargo.lock` at all, so the audit
    /// read a lockfile `cargo audit` generated from the registry rather than one
    /// this repository committed.
    NoTargetLock,
    /// The scanned tree's lockfile differed from the target's: it carried an
    /// uncommitted change before the run, or a command rewrote it during the
    /// run (cargo updates a lock its manifest has outgrown unless `--locked`
    /// forbids it, and none of prview's cargo commands pass it).
    DirtyLock,
    /// The reviewed commit moved its cargo project away from the configured
    /// cargo root, so cargo ran in a directory the lockfile questions were not
    /// asked about.
    RelocatedCargoRoot,
    /// The configuration `cargo audit` read, `.cargo/audit.toml` in the cargo
    /// root, is not one both sides share ([`cargo_audit_config_gap`]): the
    /// change edits it, or the scanned tree's copy differs from the target's.
    /// The lockfile may be the target's, but that file decides which advisories
    /// fail, and no audit reads the base's copy of it, so no lockfile
    /// comparison can show a failure predates the change.
    AuditConfigChanged,
    /// A compared commit holds `.cargo/audit.toml` only under another case,
    /// such as `.cargo/Audit.toml`. A checkout on a case-insensitive
    /// filesystem (the default on macOS and Windows) reads that entry, and the
    /// exact-path comparison cannot see it, so nothing shows the configuration
    /// is the one both sides share.
    AuditConfigCaseVariant,
    /// The Cargo home cargo-audit resolves is relative: the `CARGO_HOME` the
    /// checks inherited, or, with that unset, `$HOME/.cargo` under a relative
    /// or empty `HOME`. It then resolves inside the directory `cargo audit` ran
    /// in, so the fallback configuration `audit.toml` and the advisory database
    /// under it are files in the scanned tree, which no comparison of
    /// `.cargo/audit.toml` covers.
    RelativeCargoHome,
    /// The Cargo home cargo-audit resolves (`CARGO_HOME`, else `$HOME/.cargo`)
    /// is absolute but lies inside the repository checkout or the scanned tree,
    /// however it is spelled or linked ([`cargo_home_inside_trees`]). The
    /// fallback configuration and the advisory database are then files there,
    /// which no comparison of `.cargo/audit.toml` covers, just as for a relative
    /// one.
    InTreeCargoHome,
    /// The Cargo home lies outside the checkout and the scanned tree, but what
    /// cargo-audit read through it is not shown to ([`audit_inputs_outside_trees`]):
    /// its `audit.toml` or `advisory-db` is a link into either tree, the
    /// configuration the audit applied names a `[database] path` that is
    /// relative or leads there, or the fallback configuration could not be read.
    /// The advisories that fail are then decided by files a change can edit.
    AuditInputInTree,
    /// The worktree status or the checkout's identity could not be read, so
    /// nothing about the scanned lockfile was established either way.
    UnknownProvenance,
}

impl LockProofGap {
    /// The gap in one clause, for the merge gate's blocking line. Free-form
    /// text by contract: structural consumers read `checks[]`, not this string.
    pub(crate) fn gate_note(self) -> &'static str {
        match self {
            LockProofGap::NoTargetLock => {
                "provenance proof unavailable: no Cargo.lock in the target tree"
            }
            LockProofGap::DirtyLock => {
                "provenance proof unavailable: Cargo.lock dirty or rewritten in the scanned tree"
            }
            LockProofGap::RelocatedCargoRoot => {
                "provenance proof unavailable: the reviewed commit moved the cargo \
                 root away from the configured one"
            }
            LockProofGap::AuditConfigChanged => {
                "provenance proof unavailable: the cargo-audit configuration \
                 (.cargo/audit.toml) changed or is dirty in the scanned tree"
            }
            LockProofGap::AuditConfigCaseVariant => {
                "provenance proof unavailable: .cargo/audit.toml is committed under \
                 another case, which a case-insensitive checkout reads"
            }
            LockProofGap::RelativeCargoHome => {
                "provenance proof unavailable: the Cargo home (CARGO_HOME, else \
                 $HOME/.cargo) is relative, so cargo audit read its fallback \
                 configuration and advisory database inside the scanned tree"
            }
            LockProofGap::InTreeCargoHome => {
                "provenance proof unavailable: the Cargo home (CARGO_HOME, else \
                 $HOME/.cargo) lies inside the checkout or the scanned tree, so cargo \
                 audit read its fallback configuration and advisory database from \
                 files there"
            }
            LockProofGap::AuditInputInTree => {
                "provenance proof unavailable: cargo audit's fallback configuration \
                 or advisory database is not shown to lie outside the checkout and \
                 the scanned tree (a link or a configured database path leads there, \
                 or it could not be read)"
            }
            LockProofGap::UnknownProvenance => {
                "provenance proof unavailable: the scanned tree could not be tied \
                 to the target commit"
            }
        }
    }
}

/// Resolve [`CargoAuditLockProof`] for this run.
///
/// The lockfile half of the proof has TWO premises, and both branches of the
/// checkout shape share the first one. The third premise, the audit
/// configuration, needs the diffs and is applied by [`CleanComparison::resolve`]
/// ([`cargo_audit_config_gap`]) on the same evidence.
///
/// **Premise 1 — the target tree has a lockfile at all.** `cargo audit` does
/// not refuse a crate without `Cargo.lock`; it resolves one from the registry,
/// audits that, and exits non-zero on a hit (measured: cargo-audit 0.22.2).
/// Such a run produces genuine advisories about a lockfile no commit contains,
/// and calling them "pre-existing: Cargo.lock unchanged by this PR" is a claim
/// about a file the target does not have — a false PASS on a security gate, in
/// a repository whose `Cargo.lock` is untracked or ignored and whose PR just
/// added a vulnerable dependency. So the proof asks the target COMMIT for the
/// one lockfile the audit reads — `Cargo.lock` in the cargo root itself, since
/// `cargo audit` never falls back to a workspace root's — and a tree without
/// it, or an unanswerable question, establishes nothing.
///
/// **Premise 2 — the lockfile that was read was that one.** Where the tree
/// starts is only half of it: cargo check, clippy, test and audit run in the
/// same tree one after another, none of them passes `--locked`, and cargo
/// rewrites a lockfile its manifest has outgrown. A target that adds a
/// dependency without regenerating `Cargo.lock` therefore has the lock updated
/// by the first cargo command, and the audit reads that updated lock — while
/// the lock-changed classification still compares the committed, untouched
/// one, so an advisory the manifest change pulled in reads as pre-existing.
/// Both shapes therefore also ask whether the lock changed WHILE the checks ran.
///
/// `target_is_checkout == Some(false)` is the remote/snapshot shape: `cargo
/// audit` runs through `plan_cargo_run`, in a worktree snapshot materialised at
/// the target commit, so the lock it read is the target's unless a command
/// rewrote it. The shared snapshot is observed at every check boundary
/// ([`LockEvidence::snapshot_integrity`]); the proof holds only when no
/// boundary saw the audited lock change, and an unreadable boundary — or no
/// observation at all — establishes nothing. The boundaries are unioned over
/// the whole run, not cut at the audit, so a rewrite by a check that ran after
/// it withholds the proof too: deliberately conservative. This widens the pre-existing
/// downgrade to `cargo_audit` on snapshot runs — a deliberate gate-semantics
/// decision (the case `check_scans_target_snapshot` documents as deliberately
/// deferred), earned here by a proof rather than inherited from a check having
/// moved substrate. It holds only while cargo ran in the cargo root premise 1
/// asked about: a reviewed commit that moved its manifest (`crates/core` →
/// `backend`) has cargo run in the new directory, while premise 1 and the
/// lock-changed classification still read the configured one — possibly a
/// stale lock left behind. That shape is refused before premise 1 is asked.
///
/// `Some(true)` is the local shape: the lock is the target's only while the
/// lockfile carries no uncommitted change, both in the dirty set frozen before
/// the checks ran (R4-19) and after them. The second reading is of the audited
/// lock alone (a diff narrowed to that one path), against the target commit,
/// with untracked files excluded — so the in-repo output and check caches
/// R4-19 guards against cannot reach it, while a lock cargo rewrote mid-run
/// does. An unreadable status (`None` or a
/// failed read) establishes nothing.
fn resolve_cargo_audit_lock_proof(
    config: &Config,
    repo: Option<&crate::git::Repository>,
    resolved_target: &crate::git::ResolvedRef,
    target_is_checkout: Option<bool>,
    evidence: LockEvidence<'_>,
) -> CargoAuditLockProof {
    let Some(target_is_checkout) = target_is_checkout else {
        return CargoAuditLockProof::Unproven(LockProofGap::UnknownProvenance);
    };
    let Some(repo) = repo else {
        return CargoAuditLockProof::Unproven(LockProofGap::UnknownProvenance);
    };
    if !target_is_checkout
        && crate::checks::reviewed_cargo_root_relocated(config, &resolved_target.commit_id)
    {
        return CargoAuditLockProof::Unproven(LockProofGap::RelocatedCargoRoot);
    }
    // Premise 1, shared by both shapes: no lockfile in the target tree, or no
    // readable answer, and there is nothing for premise 2 to be about.
    let lock = match crate::artifacts::audit::cargo_audit_lock_path_in_commit(
        repo,
        &resolved_target.commit_id,
        &config.repo_root,
        config.profile.cargo_root.as_deref(),
    ) {
        Some(Some(lock)) => lock,
        Some(None) => return CargoAuditLockProof::Unproven(LockProofGap::NoTargetLock),
        None => return CargoAuditLockProof::Unproven(LockProofGap::UnknownProvenance),
    };

    if !target_is_checkout {
        return match evidence
            .snapshot_integrity
            .map(|integrity| integrity.tracked_changes())
        {
            Some(Some(changed)) if changed.contains(&lock) => {
                CargoAuditLockProof::Unproven(LockProofGap::DirtyLock)
            }
            Some(Some(_)) => CargoAuditLockProof::TargetLock,
            Some(None) | None => CargoAuditLockProof::Unproven(LockProofGap::UnknownProvenance),
        };
    }
    match evidence.dirty_before_checks {
        None => return CargoAuditLockProof::Unproven(LockProofGap::UnknownProvenance),
        Some(dirty) if dirty.contains(&lock) => {
            return CargoAuditLockProof::Unproven(LockProofGap::DirtyLock);
        }
        Some(_) => {}
    }
    match repo.tracked_path_differs_from_oid(&resolved_target.commit_id, &lock) {
        Ok(true) => CargoAuditLockProof::Unproven(LockProofGap::DirtyLock),
        Ok(false) => CargoAuditLockProof::TargetLock,
        Err(_) => CargoAuditLockProof::Unproven(LockProofGap::UnknownProvenance),
    }
}

/// What the run observed about the lockfile `cargo audit` read — the evidence
/// premise 2 of [`resolve_cargo_audit_lock_proof`] is decided on.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct LockEvidence<'a> {
    /// Operator-checkout paths dirty before the checks ran (R4-19); `None` when
    /// that status could not be read.
    pub(crate) dirty_before_checks: Option<&'a std::collections::BTreeSet<String>>,
    /// The shared snapshot's check-boundary observations: how an off-`HEAD` run
    /// learns whether a command rewrote the tree the audit read. `None` for a
    /// local review, which has no snapshot to observe.
    pub(crate) snapshot_integrity: Option<&'a super::signal::SnapshotIntegrity>,
    /// The shared snapshot's working tree, the one an off-`HEAD` audit ran in,
    /// where an untracked or ignored file it read can still be found. `None`
    /// for a local review.
    pub(crate) snapshot_root: Option<&'a std::path::Path>,
    /// The `CARGO_HOME` the checks inherited, which `cargo audit` resolves
    /// against the directory it runs in when it is relative. `None` when unset.
    pub(crate) cargo_home: Option<&'a std::ffi::OsStr>,
    /// The account home the checks inherited, as the `home` crate reads it
    /// ([`crate::checks::cargo_operator_home`]). With `CARGO_HOME` unset or
    /// empty, cargo-audit's Cargo home is `<this>/.cargo`. `None` when no home
    /// can be found.
    pub(crate) operator_home: Option<&'a std::path::Path>,
}

impl<'a> LockEvidence<'a> {
    /// Evidence carrying only the dirty set frozen before the checks.
    #[cfg(test)]
    pub(crate) fn before_checks(dirty: Option<&'a std::collections::BTreeSet<String>>) -> Self {
        LockEvidence {
            dirty_before_checks: dirty,
            ..LockEvidence::default()
        }
    }
}

/// Per-check clean-comparison signal: whether an all-out-of-diff location set for
/// a given check may be trusted as pre-existing debt and downgraded off the merge
/// gate.
///
/// The downgrade is only sound for a check whose findings came from the analysed
/// *target* tree. Two shapes qualify:
///
/// * **Local target** (`captured head == target`): every baseline-signal check scans the
///   working tree, which IS the target — provided the tree is clean. A dirty
///   worktree can make an uncommitted finding look out-of-diff (R2-9), so a dirty
///   local scan downgrades nothing.
/// * **Remote/snapshot target** (`captured head != target`): only checks listed
///   by `check_scans_target_snapshot` qualify. Operator-scanned checks came from
///   a different tree, even if the checkout moves to the target before publication.
///   Cargo audit separately selects its lock/config proof from the actual
///   snapshot observation, including an explicit same-HEAD target.
///
/// `--current-only` deliberately drops the diff bases to analyse the whole
/// current state, so there is no diff baseline a finding can "predate": the
/// downgrade must never fire regardless of tree shape (R3-14).
///
/// Checkout identity comes from the operator HEAD captured before checks. An
/// unknown captured HEAD grants no downgrade. A later HEAD read only invalidates
/// a moved checkout; it never changes which source produced the findings.
#[derive(Debug, Clone)]
pub(crate) struct CleanComparison {
    /// Whether the captured checkout was the analysed target. `None` means
    /// the operator HEAD was not established or moved; no stable identity is inferred.
    target_is_checkout: Option<bool>,
    /// When the local checkout is the target, whether it is free of staged,
    /// unstaged, and untracked changes. `None` means the status could not be
    /// read: not a licence to trust the tree, so the downgrade stays off.
    worktree_clean: Option<bool>,
    /// `--current-only`: the run has no diff baseline, so no out-of-diff row can
    /// be proven pre-existing and the downgrade is disabled entirely (R3-14).
    current_only: bool,
    /// Whether at least one resolved base differs from the target, i.e. a real
    /// diff baseline exists. When no base resolves (a repo whose configured
    /// trunk is absent, or the only base *is* the target) the diffs are empty
    /// and every location sits out-of-diff trivially — from the missing
    /// changed-file set, not from any proof it predates the target. Without a
    /// baseline the pre-existing downgrade must never fire (R4-20).
    has_base_diff: bool,
    /// check_ids whose OWN config file is part of the diff. A changed
    /// formatter/linter config can make a stricter rule flag previously-clean,
    /// UNCHANGED files, so an out-of-diff finding for that tool is no longer
    /// provably pre-existing. The downgrade is suppressed for these check_ids
    /// (R5-21).
    configs_changed: std::collections::BTreeSet<&'static str>,
    /// Cargo audit's own substrate proof. `cargo_audit` is the one
    /// baseline-signal check whose findings do not live in source files, so the
    /// whole-tree cleanliness rule above is not evidence about it either way —
    /// see [`CargoAuditLockProof`]. Its configuration file is part of that
    /// proof rather than of `configs_changed`, so the gate can name it
    /// ([`LockProofGap::AuditConfigChanged`]).
    cargo_audit_lock: CargoAuditLockProof,
}

impl CleanComparison {
    /// Build the comparison from a `worktree_clean` value **frozen before the
    /// run touched the tree** (R4-19). Cleanliness must be captured once, before
    /// checks run and before any artifact is written, otherwise an in-repo
    /// `--output-dir` or a check that drops an untracked cache makes a clean
    /// source scan look "dirty" and blocks the pre-existing downgrade. See
    /// [`capture_worktree_provenance`].
    pub(crate) fn resolve(
        config: &Config,
        resolved_target: &crate::git::ResolvedRef,
        resolved_bases: &[crate::git::ResolvedRef],
        worktree_clean: Option<bool>,
        lock_evidence: LockEvidence<'_>,
        worktree_head_sha: Option<&str>,
        diffs: &[crate::git::Diff],
    ) -> Self {
        let has_base_diff = has_resolvable_base_diff(resolved_target, resolved_bases);
        let configs_changed = changed_tool_config_owners(diffs);
        let repo = crate::git::Repository::open(&config.repo_root).ok();
        let head = repo.as_ref().and_then(|repo| repo.head_commit_id().ok());
        // A later HEAD can invalidate source stability, never grant a new
        // source identity to results produced earlier in the run.
        let stable_head = worktree_head_sha.filter(|captured| head.as_deref() == Some(*captured));
        let target_is_checkout = stable_head.map(|captured| captured == resolved_target.commit_id);
        // A same-HEAD exact review still audits a materialized snapshot. The
        // snapshot observation, rather than HEAD equality, decides which lock
        // and audit-config evidence describes the bytes cargo-audit read.
        let audit_scans_checkout =
            target_is_checkout.map(|same| same && lock_evidence.snapshot_integrity.is_none());
        CleanComparison {
            target_is_checkout,
            worktree_clean,
            current_only: config.current_only,
            has_base_diff,
            configs_changed,
            cargo_audit_lock: match resolve_cargo_audit_lock_proof(
                config,
                repo.as_ref(),
                resolved_target,
                audit_scans_checkout,
                lock_evidence,
            ) {
                CargoAuditLockProof::TargetLock => cargo_audit_config_gap(
                    config,
                    repo.as_ref(),
                    resolved_target,
                    audit_scans_checkout,
                    lock_evidence,
                    diffs,
                )
                .map_or(
                    CargoAuditLockProof::TargetLock,
                    CargoAuditLockProof::Unproven,
                ),
                proof => proof,
            },
        }
    }

    /// The operator working-tree cleanliness this comparison was built from —
    /// the value frozen before the checks ran, `None` when it could not be read.
    ///
    /// Exposed so the merge gate can hold the per-check substrate rows against
    /// the SAME observation the pre-existing downgrade uses, instead of taking a
    /// second, later reading of the tree.
    pub(crate) fn operator_worktree_clean(&self) -> Option<bool> {
        self.worktree_clean
    }

    /// Cargo audit's lockfile provenance proof, as resolved for this run.
    ///
    /// Exposed so the merge gate can state WHY a cargo audit blocked when the
    /// counts have nothing to add: "unclassified" names the outcome, not the
    /// missing premise.
    pub(crate) fn cargo_audit_lock_proof(&self) -> CargoAuditLockProof {
        self.cargo_audit_lock
    }

    /// Whether the pre-existing downgrade may fire for `check_id`'s findings.
    pub(crate) fn applies_to(&self, check_id: &str) -> bool {
        if self.configs_changed.iter().any(|owner| *owner == check_id) {
            // The tool's own config changed in this diff, so a newly-stricter
            // rule may flag UNCHANGED files: an out-of-diff finding is no longer
            // provably pre-existing. Suppress the downgrade conservatively so the
            // finding stays Unclassified and keeps gating (R5-21).
            return false;
        }
        if !self.has_base_diff {
            // No resolved base differs from the target, so no diff baseline
            // exists: every location is out-of-diff trivially from the empty
            // changed-file set and nothing can be proven pre-existing (R4-20).
            return false;
        }
        if self.current_only {
            // No diff baseline exists, so nothing can be proven pre-existing: the
            // full-scan findings all sit "out of diff" trivially (R3-14).
            return false;
        }
        if check_id == "cargo_audit" {
            // R2-9's dirty-worktree rule is about source bytes that could forge
            // an out-of-diff position. A cargo-audit advisory has no source
            // position to forge — it is `Cargo.lock` × the advisory database —
            // so uncommitted changes elsewhere in the tree are not evidence
            // about it. The proof this check needs is that the lockfile it read
            // was the target's; the checks above (R5-21, R4-20, R3-14) still
            // apply, and R5-23 still governs rows whose origin is unknown.
            return self.cargo_audit_lock == CargoAuditLockProof::TargetLock;
        }
        match self.target_is_checkout {
            // Local checkout was the target: both captured cleanliness and HEAD
            // stability are required. Generated files do not trigger a new status read.
            Some(true) => self.worktree_clean == Some(true),
            // A different operator checkout never licenses operator-scanned findings.
            // Checks known to use the target snapshot retain their own source proof.
            Some(false) => check_scans_target_snapshot(check_id),
            None => false,
        }
    }

    #[cfg(test)]
    pub(crate) fn for_test(target_is_checkout: bool, worktree_clean: bool) -> Self {
        CleanComparison {
            target_is_checkout: Some(target_is_checkout),
            worktree_clean: Some(worktree_clean),
            current_only: false,
            has_base_diff: true,
            configs_changed: std::collections::BTreeSet::new(),
            // The lockfile proof is orthogonal to whole-tree cleanliness, which
            // is the whole point of the split: a test that dirties the tree is
            // not thereby saying the lockfile moved. Tests that mean the lock
            // itself is unproven say so with `for_test_cargo_audit_lock`.
            cargo_audit_lock: CargoAuditLockProof::TargetLock,
        }
    }

    /// A clean local comparison in which only cargo audit's lockfile proof is
    /// varied.
    #[cfg(test)]
    pub(crate) fn for_test_cargo_audit_lock(proof: CargoAuditLockProof) -> Self {
        CleanComparison {
            cargo_audit_lock: proof,
            ..CleanComparison::for_test(true, true)
        }
    }

    #[cfg(test)]
    pub(crate) fn for_test_current_only() -> Self {
        CleanComparison {
            target_is_checkout: Some(true),
            worktree_clean: Some(true),
            current_only: true,
            has_base_diff: true,
            configs_changed: std::collections::BTreeSet::new(),
            cargo_audit_lock: CargoAuditLockProof::TargetLock,
        }
    }

    #[cfg(test)]
    pub(crate) fn for_test_no_base_diff() -> Self {
        CleanComparison {
            target_is_checkout: Some(true),
            worktree_clean: Some(true),
            current_only: false,
            has_base_diff: false,
            configs_changed: std::collections::BTreeSet::new(),
            cargo_audit_lock: CargoAuditLockProof::TargetLock,
        }
    }

    #[cfg(test)]
    pub(crate) fn for_test_config_changed(owners: &[&'static str]) -> Self {
        CleanComparison {
            target_is_checkout: Some(true),
            worktree_clean: Some(true),
            current_only: false,
            has_base_diff: true,
            configs_changed: owners.iter().copied().collect(),
            cargo_audit_lock: CargoAuditLockProof::TargetLock,
        }
    }
}

/// Map a changed config file's basename to the baseline-signal check_id whose
/// out-of-diff findings must NOT be downgraded when that config is in the diff
/// (R5-21). A stricter formatter/linter rule can start flagging files the PR
/// never touched, so those out-of-diff findings are no longer provably
/// pre-existing.
///
/// `Cargo.toml` is deliberately NOT mapped: it carries `[lints]` for
/// rustc/clippy — whole-project gates that are never eligible for the downgrade
/// anyway — not rustfmt config (which lives in `rustfmt.toml`). `pyproject.toml`
/// IS mapped to ruff because it carries the `[tool.ruff]` section; the mapping
/// is deliberately conservative, so an unrelated `pyproject.toml` edit
/// suppressing the ruff downgrade is an accepted false-CONDITIONAL over a
/// false-PASS.
fn config_file_owner(basename: &str) -> Option<&'static str> {
    match basename {
        "rustfmt.toml" | ".rustfmt.toml" => Some("rustfmt"),
        "ruff.toml" | ".ruff.toml" | "pyproject.toml" => Some("ruff"),
        ".eslintrc" | ".eslintrc.js" | ".eslintrc.cjs" | ".eslintrc.json" | ".eslintrc.yaml"
        | ".eslintrc.yml" | "eslint.config.js" | "eslint.config.mjs" | "eslint.config.cjs" => {
            Some("eslint")
        }
        ".stylelintrc"
        | ".stylelintrc.json"
        | ".stylelintrc.js"
        | ".stylelintrc.yaml"
        | ".stylelintrc.yml"
        | "stylelint.config.js"
        | "stylelint.config.cjs" => Some("stylelint"),
        ".prettierrc"
        | ".prettierrc.json"
        | ".prettierrc.js"
        | ".prettierrc.cjs"
        | ".prettierrc.yaml"
        | ".prettierrc.yml"
        | "prettier.config.js"
        | "prettier.config.cjs" => Some("prettier"),
        "semgrep.yml" | "semgrep.yaml" | ".semgrep.yml" | ".semgrep.yaml" => Some("semgrep_scan"),
        _ => None,
    }
}

/// The gap, if any, in the premise that the configuration `cargo audit` reads
/// in the audited Cargo root is one the base and the target share: it differs
/// between the base and the target of a diff, or the scanned tree's copy of it
/// is not the target's.
///
/// That file is part of the substrate the audit verdict is computed on, like
/// the lockfile. Its `ignore` list, `informational_warnings` and
/// `[output] deny` decide which advisories fail the audit. No audit reads the
/// base's configuration: the audit runs in the reviewed tree, and the baseline
/// audit reads the base's lockfile but runs in the repository's checkout, so a
/// changed configuration never shows up in the lockfile comparison. A pull
/// request that only removes an ignored advisory makes the audit fail with the
/// lockfile unchanged and every finding out-of-diff. The lockfile proof alone
/// would then downgrade that failure to pre-existing, a false PASS, so a
/// changed configuration withholds the proof as
/// [`LockProofGap::AuditConfigChanged`].
///
/// The file is compared by blob identity at its exact path in both commits
/// ([`crate::artifacts::audit::cargo_audit_config_path`]). A rename or deletion
/// therefore counts, even though the pack's changed-file rows keep only a
/// rename's new path. Anything the commits cannot answer for also counts as a
/// change: an unreadable tree, a symlink at the path or at a parent, or no
/// repository. The committed comparison speaks only for the commits, so the
/// file the audit actually read must also be the target's
/// ([`scanned_audit_config_is_target`]).
///
/// An exact-path comparison is blind to an entry spelled in another case,
/// such as `.cargo/Audit.toml`, which a checkout on a case-insensitive
/// filesystem reads in place of the path. When the target or either side of a
/// diff holds one ([`crate::git::Repository::case_variant_at_commit`]), the
/// proof is withheld as [`LockProofGap::AuditConfigCaseVariant`]. This is
/// checked first, because it makes every exact-path answer moot. A tree that
/// cannot be searched for one names no case, so it counts as a change like
/// any other unreadable answer.
///
/// cargo-audit falls back to `audit.toml` in its Cargo home when the cargo
/// root has no `.cargo/audit.toml`, and reads its advisory database under that
/// home either way. The home is `CARGO_HOME` when set and non-empty, and
/// otherwise `$HOME/.cargo`, as `home::cargo_home` resolves it. A relative home
/// resolves against the directory the audit ran in, making both files part of
/// the scanned tree that no comparison here covers. The proof is then withheld
/// as [`LockProofGap::RelativeCargoHome`] before anything else is asked. An
/// absolute one is the environment's only while it lies outside the checkout
/// and the scanned tree; inside either, the proof is withheld the same way, as
/// [`LockProofGap::InTreeCargoHome`]. Without any home, cargo-audit reads no
/// fallback, and rustsec cannot place its default database.
///
/// Once the committed comparisons hold, the files cargo-audit read outside
/// them are followed to where they lead ([`audit_inputs_outside_trees`]): the
/// fallback `audit.toml` and the advisory database, which a link under an
/// external home or a configured `[database] path` can place back inside a
/// tree. Either withholds the proof as [`LockProofGap::AuditInputInTree`].
///
/// With a Cargo root outside the repository there is no in-tree file, and no
/// lock proof either.
fn cargo_audit_config_gap(
    config: &Config,
    repo: Option<&crate::git::Repository>,
    resolved_target: &crate::git::ResolvedRef,
    target_is_checkout: Option<bool>,
    evidence: LockEvidence<'_>,
    diffs: &[crate::git::Diff],
) -> Option<LockProofGap> {
    let path = crate::artifacts::audit::cargo_audit_config_path(
        &config.repo_root,
        config.profile.cargo_root.as_deref(),
    )?;
    // The same reading as `home::cargo_home`, which cargo-audit uses: an
    // empty `CARGO_HOME` counts as unset, and then the account home's `.cargo`
    // is the Cargo home. Without either there is no fallback configuration to
    // read, and rustsec cannot place its default database; a database the
    // configuration names is judged with the other inputs below.
    let cargo_home = match evidence.cargo_home.filter(|home| !home.is_empty()) {
        Some(home) => Some(std::path::PathBuf::from(home)),
        None => evidence.operator_home.map(|home| home.join(".cargo")),
    };
    let roots: Vec<&std::path::Path> = std::iter::once(config.repo_root.as_path())
        .chain(evidence.snapshot_root)
        .collect();
    if let Some(home) = &cargo_home {
        if !home.is_absolute() {
            return Some(LockProofGap::RelativeCargoHome);
        }
        if cargo_home_inside_trees(home, roots.iter().copied()) {
            return Some(LockProofGap::InTreeCargoHome);
        }
    }
    let Some(repo) = repo else {
        return Some(LockProofGap::AuditConfigChanged);
    };
    let commits = std::iter::once(resolved_target.commit_id.as_str()).chain(
        diffs
            .iter()
            .flat_map(|diff| [diff.base_commit_id.as_str(), diff.target_commit_id.as_str()]),
    );
    let mut unreadable = false;
    for commit in commits {
        match repo.case_variant_at_commit(commit, &path) {
            Ok(true) => return Some(LockProofGap::AuditConfigCaseVariant),
            Ok(false) => {}
            Err(_) => unreadable = true,
        }
    }
    let committed_change = diffs.iter().any(|diff| {
        match (
            repo.regular_blob_at_commit(&diff.base_commit_id, &path),
            repo.regular_blob_at_commit(&diff.target_commit_id, &path),
        ) {
            (Ok(base), Ok(target)) => base != target,
            _ => true,
        }
    });
    let scanned_is_target =
        scanned_audit_config_is_target(repo, &path, resolved_target, target_is_checkout, evidence);
    if unreadable || committed_change || !scanned_is_target {
        return Some(LockProofGap::AuditConfigChanged);
    }
    let inputs_outside = audit_inputs_outside_trees(
        repo,
        &path,
        &resolved_target.commit_id,
        cargo_home.as_deref(),
        &roots,
    );
    (!inputs_outside).then_some(LockProofGap::AuditInputInTree)
}

/// Whether what cargo-audit read beyond the cargo root's `.cargo/audit.toml`
/// is shown to lie outside the checkout and the scanned tree (`roots`): the
/// fallback configuration, and the advisory database, which decides which
/// advisories exist at all.
///
/// [`cargo_audit_config_gap`] has already placed the Cargo home outside both,
/// yet a file under it can still lead back in. `audit.toml` or `advisory-db`
/// there may be a symbolic link into the checkout, whose target a change edits
/// with the lockfile untouched; [`cargo_home_inside_trees`] resolves such a
/// link where it exists. A dangling one reads as absent, to cargo-audit as
/// here.
///
/// The configuration the audit applied, the target's `.cargo/audit.toml` when
/// it has one (the comparisons before this showed the scanned copy is that
/// file) and otherwise the fallback, can name the database itself:
/// `[database] path`, which cargo-audit opens as written, so a relative one
/// lies under the directory the audit ran in. Such a path takes the place of
/// `advisory-db` in the Cargo home, and it matters even with no home at all,
/// where it is what lets the audit run. `[database] url` is not followed:
/// rustsec's `Repository::fetch` accepts only an `https://` address, and on
/// any other cargo-audit exits without a report, so nothing is downgraded.
///
/// A fallback configuration that exists but cannot be read shows nothing, so
/// it counts as leading in. One that is not a TOML table names no database:
/// cargo-audit refuses to load it, so there is no report to downgrade.
fn audit_inputs_outside_trees(
    repo: &crate::git::Repository,
    path: &str,
    target: &str,
    cargo_home: Option<&std::path::Path>,
    roots: &[&std::path::Path],
) -> bool {
    let inside = |input: &std::path::Path| cargo_home_inside_trees(input, roots.iter().copied());
    let applied = match repo.regular_blob_bytes_at_oid(target, path) {
        Ok(Some(config)) => Some(config),
        Ok(None) => match cargo_home.map(|home| home.join("audit.toml")) {
            None => None,
            Some(fallback) if inside(&fallback) => return false,
            Some(fallback) => match std::fs::read(&fallback) {
                Ok(config) => Some(config),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                Err(_) => return false,
            },
        },
        Err(_) => return false,
    };
    let database = match applied.as_deref().and_then(configured_advisory_database) {
        Some(database) if !database.is_absolute() => return false,
        Some(database) => database,
        None => match cargo_home {
            Some(home) => home.join("advisory-db"),
            None => return true,
        },
    };
    !inside(&database)
}

/// The `[database] path` a cargo-audit configuration sets, as written.
fn configured_advisory_database(config: &[u8]) -> Option<std::path::PathBuf> {
    let config: toml::Table = toml::from_str(std::str::from_utf8(config).ok()?).ok()?;
    config
        .get("database")?
        .get("path")?
        .as_str()
        .map(std::path::PathBuf::from)
}

/// Whether the absolute `home` is, or lies inside, one of `roots`.
///
/// Every path is read twice: lexically, with `.` and `..` resolved, and
/// through the filesystem, with symbolic links resolved up to its deepest
/// existing ancestor, since cargo-audit may create the directory itself. Any
/// pairing of the readings counts, so a home spelled through `..` or reached
/// through a link is where it leads. Components compare without regard to
/// ASCII case, as a case-insensitive filesystem opens the same directory under
/// either spelling. A match that only case produces on a case-sensitive one
/// withholds the proof needlessly, never grants it. Only absolute readings
/// take part: a relative root read lexically would contain every path.
fn cargo_home_inside_trees<'a>(
    home: &std::path::Path,
    roots: impl IntoIterator<Item = &'a std::path::Path>,
) -> bool {
    fn readings(path: &std::path::Path) -> Vec<std::path::PathBuf> {
        let mut readings = vec![crate::paths::clean_path_buf(path)];
        readings.extend(resolved_through_links(path));
        readings.retain(|reading| reading.is_absolute());
        readings
    }
    fn resolved_through_links(path: &std::path::Path) -> Option<std::path::PathBuf> {
        let mut missing = Vec::new();
        let mut existing = path;
        loop {
            if let Ok(real) = existing.canonicalize() {
                return Some(
                    missing
                        .iter()
                        .rev()
                        .fold(real, |real, part| real.join(part)),
                );
            }
            missing.push(existing.file_name()?);
            existing = existing.parent()?;
        }
    }
    fn starts_with_ignoring_ascii_case(path: &std::path::Path, prefix: &std::path::Path) -> bool {
        let mut components = path.components();
        prefix.components().all(|part| {
            components
                .next()
                .is_some_and(|own| own.as_os_str().eq_ignore_ascii_case(part.as_os_str()))
        })
    }
    let homes = readings(home);
    roots.into_iter().any(|root| {
        readings(root).iter().any(|root| {
            homes
                .iter()
                .any(|home| starts_with_ignoring_ascii_case(home, root))
        })
    })
}

/// Whether the `.cargo/audit.toml` the audit read in the scanned tree is the
/// target commit's: the same file, or no file where the target has none.
///
/// A local review audits the checkout, so a staged, unstaged, untracked or
/// ignored configuration there is the one `cargo audit` applied. Such a file
/// can ignore the advisory the change introduced while pre-existing ones still
/// fail. The proof would then downgrade the failure, and the gate would pass a
/// change that its own committed configuration blocks.
///
/// The observations mirror the lockfile's (premise 2 of
/// [`resolve_cargo_audit_lock_proof`]):
/// - the paths dirty before the checks, where a dirty parent such as an
///   untracked symlinked `.cargo` counts, and so does a path spelled in
///   another case ([`path_or_parent_is`]);
/// - after the checks, the tracked path read against the target in the index,
///   in the working tree, and on disk past any skip-worktree flag
///   ([`crate::git::Repository::tracked_path_differs_from_oid`]);
/// - where the target has no configuration, any file at the path. This is how
///   an ignored configuration is caught, since the status read never lists it.
///
/// A snapshot run audits a tree materialised from the target, so only a check
/// can make the file there differ. The check boundaries report a tracked file
/// it rewrote, in the index, the working tree or on disk past a skip flag. They
/// never list an untracked or ignored file, and an earlier check's build
/// script can generate one where the target has no configuration. So, as
/// locally, any file at that path in the snapshot's working tree counts.
///
/// Anything that cannot be read counts as a difference.
fn scanned_audit_config_is_target(
    repo: &crate::git::Repository,
    path: &str,
    resolved_target: &crate::git::ResolvedRef,
    target_is_checkout: Option<bool>,
    evidence: LockEvidence<'_>,
) -> bool {
    let absent_from = |root: &std::path::Path| {
        matches!(
            std::fs::symlink_metadata(root.join(path)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound
        )
    };
    match target_is_checkout {
        Some(false) => {
            let untouched = evidence
                .snapshot_integrity
                .and_then(|integrity| integrity.tracked_changes())
                .is_some_and(|changed| {
                    !changed
                        .iter()
                        .any(|changed| path_or_parent_is(path, changed))
                });
            untouched
                && match repo.regular_blob_at_commit(&resolved_target.commit_id, path) {
                    Ok(Some(_)) => true,
                    Ok(None) => evidence.snapshot_root.is_some_and(absent_from),
                    Err(_) => false,
                }
        }
        Some(true) => {
            let Some(dirty) = evidence.dirty_before_checks else {
                return false;
            };
            if dirty.iter().any(|changed| path_or_parent_is(path, changed)) {
                return false;
            }
            let target = &resolved_target.commit_id;
            if !matches!(repo.tracked_path_differs_from_oid(target, path), Ok(false)) {
                return false;
            }
            match repo.regular_blob_at_commit(target, path) {
                Ok(Some(_)) => true,
                Ok(None) => repo.workdir().is_some_and(absent_from),
                Err(_) => false,
            }
        }
        None => false,
    }
}

/// Whether the repository-relative `changed` names `path` or a directory above
/// it, under ASCII case folding. A status read lists an untracked symlink or
/// directory by its own path, with or without a trailing `/`, and a checkout
/// on a case-insensitive filesystem reads `.cargo/Audit.toml` for
/// `.cargo/audit.toml`. On a case-sensitive one the folding only withholds a
/// proof it could have kept.
fn path_or_parent_is(path: &str, changed: &str) -> bool {
    let changed = changed.trim_end_matches('/');
    path.get(..changed.len())
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case(changed))
        && path[changed.len()..]
            .chars()
            .next()
            .is_none_or(|next| next == '/')
}

/// The set of baseline-signal check_ids whose config file appears in `diffs`.
fn changed_tool_config_owners(
    diffs: &[crate::git::Diff],
) -> std::collections::BTreeSet<&'static str> {
    let mut owners = std::collections::BTreeSet::new();
    for file in diffs.iter().flat_map(|diff| diff.files.iter()) {
        let basename = std::path::Path::new(&file.path)
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(file.path.as_str());
        if let Some(owner) = config_file_owner(basename) {
            owners.insert(owner);
        }
    }
    owners
}

/// Whether any resolved base differs from the target commit — i.e. a real diff
/// baseline exists. An empty base set, or a base whose commit *is* the target,
/// yields no baseline: the diffs are empty and every finding sits out-of-diff
/// trivially, so the pre-existing downgrade must not fire (R4-20).
fn has_resolvable_base_diff(
    resolved_target: &crate::git::ResolvedRef,
    resolved_bases: &[crate::git::ResolvedRef],
) -> bool {
    resolved_bases
        .iter()
        .any(|base| base.commit_id != resolved_target.commit_id)
}

/// Whether a check materialises and scans an ephemeral snapshot of the analysed
/// *target* when that target is a fetched remote ref not checked out locally.
///
/// `semgrep_scan` builds its own detached worktree at the target commit (R2-10).
/// Since A2, the other file-scoped linters (`ruff`, `eslint`, `stylelint`) run
/// through `plan_check_run`, which materialises a worktree snapshot of the target
/// and scans there in `--pr`/`--remote` mode — so their out-of-diff findings also
/// genuinely predate the target diff and may be downgraded.
///
/// `rustfmt` and `cargo_audit` were originally excluded because they ran at the
/// local checkout (R3-16). They now scan the snapshot too, but stay off this
/// list: widening the pre-existing downgrade is a gate-semantics decision, not a
/// side effect of moving a check onto the reviewed substrate. Keeping `rustfmt`
/// out is the conservative side — findings surface instead of being suppressed.
///
/// `cargo_audit` never reaches this function any more: [`CleanComparison::applies_to`]
/// answers it from [`CargoAuditLockProof`] first, which is that deliberate
/// gate-semantics decision taken explicitly and on a stated proof rather than
/// by adding a name to this list.
fn check_scans_target_snapshot(check_id: &str) -> bool {
    matches!(check_id, "semgrep_scan" | "ruff" | "eslint" | "stylelint")
}

/// Operator checkout state frozen at the start of a run: HEAD, cleanliness,
/// and a fingerprint of exactly what was dirty.
///
/// Cleanliness and digest come from ONE status read, so the pack cannot claim a clean
/// tree next to a digest of uncommitted changes. A HEAD change detected across
/// that read invalidates all three fields; this is not an atomic filesystem snapshot.
#[derive(Debug, Clone, Default)]
pub struct WorktreeProvenance {
    /// Operator checkout commit captured before checks. `None` for an unborn
    /// or unreadable HEAD; never substituted with the reviewed target SHA.
    pub head_sha: Option<String>,
    /// No staged, unstaged, or untracked changes at capture time. `None` when
    /// the status could not be read — cleanliness unestablished, never assumed.
    pub clean: Option<bool>,
    /// `sha256:<hex>` over the canonical `XY <path>` rendering of the status
    /// PLUS the current bytes of every dirty path (see
    /// [`render_status_fingerprint`]). `None` when the repository could not be
    /// inspected — an unknown fingerprint stays visibly unknown.
    pub status_digest: Option<String>,
    /// The repository-relative paths that were dirty in that SAME status read.
    ///
    /// `clean` collapses the status to one boolean, which is the right evidence
    /// for a whole-tree scanner but the wrong evidence for a check whose
    /// substrate is a single file. Keeping the path set lets a per-file proof
    /// (today: cargo audit's lockfile, see [`CargoAuditLockProof`]) ask about
    /// the file it actually read without a second, later reading of the whole
    /// tree — which is exactly what R4-19 forbids. (The lockfile proof does read
    /// its one tracked file again after the checks, because cargo itself may
    /// rewrite it; untracked output, R4-19's concern, cannot reach that read.)
    ///
    /// `None` whenever `clean` is `None`: a status nobody could read names no
    /// paths, and an empty set there would read as "nothing was dirty".
    pub dirty_paths: Option<std::collections::BTreeSet<String>>,
}

/// Read the working tree at `repo_root` once and derive both the cleanliness
/// flag and the status digest.
///
/// Called to freeze the value BEFORE any check runs or artifact is written
/// (R4-19). Cleanliness read after the run reflects prview/tool-generated files
/// (an in-repo `--output-dir` or an untracked check cache), not the source state
/// that was scanned — which would wrongly mark a clean run "dirty" and suppress
/// the pre-existing downgrade.
///
/// The two failure modes are NOT the same and must not resolve alike:
///
/// - no git repository at all: nothing can be uncommitted, and a run without a
///   repo has no diff baseline either, so the downgrade is already disabled by
///   `has_base_diff`. `Some(true)` preserves the historical permissive shape;
/// - a repository whose status cannot be read (unreadable or malformed index):
///   cleanliness was NOT established. Reporting `true` there certifies a tree
///   nobody inspected — it reaches `PROVENANCE.json.operator_worktree.clean` as a fact
///   and lets `CleanComparison` downgrade out-of-diff failures to pre-existing.
///   That is the one direction this record exists to prevent, so it stays
///   `None`: unknown, and treated as untrusted.
pub(crate) fn capture_worktree_provenance(repo_root: &std::path::Path) -> WorktreeProvenance {
    capture_worktree_provenance_inner(repo_root, || {})
}

fn capture_worktree_provenance_inner(
    repo_root: &Path,
    after_fingerprint: impl FnOnce(),
) -> WorktreeProvenance {
    use sha2::{Digest, Sha256};

    let Ok(repo) = git2::Repository::discover(repo_root) else {
        return WorktreeProvenance {
            head_sha: None,
            clean: Some(true),
            status_digest: None,
            dirty_paths: Some(std::collections::BTreeSet::new()),
        };
    };
    let read_head = || {
        repo.head()
            .and_then(|head| head.peel_to_commit())
            .ok()
            .map(|commit| commit.id().to_string())
    };
    let head_sha = read_head();
    let mut opts = git2::StatusOptions::new();
    opts.include_untracked(true)
        .recurse_untracked_dirs(true)
        .renames_head_to_index(true);
    let Ok(statuses) = repo.statuses(Some(&mut opts)) else {
        return WorktreeProvenance {
            head_sha,
            clean: None,
            status_digest: None,
            dirty_paths: None,
        };
    };

    let workdir = repo.workdir().map(|dir| dir.to_path_buf());
    let mut budget = FingerprintBudget::new(FINGERPRINT_BYTE_BUDGET);
    let fingerprint = render_status_fingerprint(&statuses, workdir.as_deref(), 0, &mut budget);
    let mut hasher = Sha256::new();
    hasher.update(fingerprint.as_bytes());
    after_fingerprint();

    // A commit/checkout during status or content reads mixes two observations.
    // Discard them instead of certifying the raced checkout clean. This bounds
    // the check; it does not lock the worktree or detect a HEAD change and revert.
    if read_head() != head_sha {
        return WorktreeProvenance::default();
    }

    WorktreeProvenance {
        head_sha,
        clean: Some(statuses.is_empty()),
        status_digest: Some(format!("sha256:{:x}", hasher.finalize())),
        // Derived from the SAME `statuses` the cleanliness flag and the digest
        // come from, so the three can never describe different observations.
        dirty_paths: Some(
            statuses
                .iter()
                .filter_map(|entry| entry.path().map(str::to_string))
                .collect(),
        ),
    }
}

/// The status rendering plus the current CONTENT of every dirty path.
///
/// The status set alone cannot tell two runs apart: editing the same tracked
/// file to different text leaves the same `M <path>` line, so a status-only
/// digest claimed two materially different substrates were the same one. Each
/// entry therefore carries a fingerprint of the bytes on disk right now:
/// `blob:<len>:<sha256>` for a regular file, the hashed target for a symlink,
/// `dir` for an ordinary directory, `gitlink:…` for a nested repository (see
/// [`nested_repo_fingerprint`]), `absent` for a deleted path and `unreadable`
/// when the bytes cannot be read.
///
/// Only the dirty subset is read — a clean tree hashes nothing, and every scan
/// prview runs afterwards (semgrep, loctree, the language checks) reads far more
/// of the tree than this does.
fn render_status_fingerprint(
    statuses: &git2::Statuses<'_>,
    workdir: Option<&Path>,
    depth: usize,
    budget: &mut FingerprintBudget,
) -> String {
    // Sort BEFORE reading. The budget is spent in iteration order, so which
    // entries are hashed and which are stat-fingerprinted must not depend on
    // the order git happens to hand them over: the same tree has to produce the
    // same digest every time. The key is the status pair plus the path, both
    // unique per entry, so the resulting order is the one sorting the rendered
    // lines produced before.
    let mut entries: Vec<(String, Option<std::path::PathBuf>)> = statuses
        .iter()
        .map(|entry| {
            // Git stores names as bytes. `path()` gives up on anything that is
            // not UTF-8, which rendered every such entry as one literal
            // placeholder and looked up its content at a path that does not
            // exist: two runs dirtying different unrepresentable names, or the
            // same one differently, produced the same digest.
            let bytes = entry.path_bytes();
            let key = format!(
                "{} {}",
                status_codes(entry.status()),
                status_path_label(bytes)
            );
            let path = match (workdir, os_relative_path(bytes)) {
                (Some(dir), Some(relative)) => Some(dir.join(relative)),
                _ => None,
            };
            (key, path)
        })
        .collect();
    entries.sort_by(|left, right| left.0.cmp(&right.0));

    entries
        .into_iter()
        .map(|(key, path)| {
            let content = match path {
                Some(path) => content_fingerprint(&path, depth, budget),
                None => "unknown".to_string(),
            };
            format!("{key}\0{content}")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// How many bytes one capture may read to fingerprint dirty content.
///
/// The read used to be unbounded, and `recurse_untracked_dirs` means an
/// untracked directory is expanded entry by entry: one forgotten dataset, model
/// checkpoint or vendored bundle in the working tree and prview hashed gigabytes
/// before the first check even started — and this capture is deliberately taken
/// before any of them run, so nobody is doing anything else meanwhile.
///
/// Measured on this crate's release build, 256 MiB of `sha256` takes ~1 s
/// (0.94 s / 1.55 s / 1.14 s over three passes on a warm cache) and ~15 s in a
/// debug build. A second of ceiling for a step nobody waits on deliberately is
/// the trade: ordinary review-sized dirt (a handful of edited sources) is
/// nowhere near it, so no existing digest changes, and everything past it is
/// described rather than read.
const FINGERPRINT_BYTE_BUDGET: u64 = 256 * 1024 * 1024;

/// The read allowance left in one capture, shared across every dirty entry and
/// every nested repository the walk descends into — the bound is on the whole
/// digest, not on each file.
struct FingerprintBudget {
    remaining: u64,
}

impl FingerprintBudget {
    fn new(bytes: u64) -> Self {
        Self { remaining: bytes }
    }

    /// Reserve `len` bytes, or refuse. A file too large for what is left is
    /// never read *partially*: a half-hashed file would be rendered as a whole
    /// one, which is precisely the collision this digest exists to avoid.
    /// Refusing also leaves the allowance intact, so the small files that follow
    /// a huge one are still fingerprinted by content.
    fn take(&mut self, len: u64) -> bool {
        match self.remaining.checked_sub(len) {
            Some(left) => {
                self.remaining = left;
                true
            }
            None => false,
        }
    }
}

/// How a dirty path is written into the digest.
///
/// UTF-8 names appear as themselves. A name that is not UTF-8 appears as the
/// hash of its bytes, so two different unrepresentable names stay two different
/// lines — the placeholder they used to share made them one.
fn status_path_label(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};

    match std::str::from_utf8(bytes) {
        Ok(path) => path.to_string(),
        Err(_) => {
            let mut hasher = Sha256::new();
            hasher.update(bytes);
            format!("<non-utf8:{:x}>", hasher.finalize())
        }
    }
}

/// A status entry's path as the OS names it, so a name git cannot render as
/// UTF-8 still resolves to the file it points at.
///
/// Windows paths are UTF-16 with no byte-oriented API to rebuild them from, so
/// an unrepresentable name there stays unreadable rather than guessed at — the
/// path still contributes to the digest through its hashed bytes.
fn os_relative_path(bytes: &[u8]) -> Option<std::path::PathBuf> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        Some(std::path::PathBuf::from(std::ffi::OsStr::from_bytes(bytes)))
    }
    #[cfg(not(unix))]
    {
        std::str::from_utf8(bytes)
            .ok()
            .map(std::path::PathBuf::from)
    }
}

/// Fingerprint the bytes currently at `path`, without loading the file whole.
///
/// `budget` is the run-wide allowance for bytes actually read (see
/// [`FingerprintBudget`]); a file that does not fit is described from its
/// metadata instead.
fn content_fingerprint(path: &Path, depth: usize, budget: &mut FingerprintBudget) -> String {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        // The path is gone (a deletion) — that IS its state.
        return "absent".to_string();
    };

    if meta.is_symlink() {
        return symlink_fingerprint(path, budget);
    }
    if meta.is_dir() {
        return nested_repo_fingerprint(path, depth, budget);
    }
    if !meta.is_file() {
        // A fifo, socket or device node in the worktree. Git lists it like any
        // other untracked entry, and opening it is at best meaningless and at
        // worst a permanent block on a reader that never gets a writer.
        return "special".to_string();
    }

    file_fingerprint(path, meta.len(), budget)
}

/// Fingerprint a symlink by both halves of what it is.
///
/// The link's own content — as git stores it — is the target *path*, and a link
/// retargeted at identical bytes is still a different tree. But everything the
/// checks read through it lives at the far end, and hashing the pathname alone
/// let all of that change between two runs under one unchanged digest.
///
/// The target is resolved exactly one logical hop, through the link itself:
/// `metadata` follows the whole chain, and a loop or a dangling link comes back
/// as an error rather than a walk. Only a regular file is read; a directory is
/// recorded as such without descending (an absolute link can leave the repo
/// entirely), and a device or fifo is never opened.
fn symlink_fingerprint(path: &Path, budget: &mut FingerprintBudget) -> String {
    use sha2::{Digest, Sha256};

    let Ok(target) = std::fs::read_link(path) else {
        return "unreadable".to_string();
    };
    let mut hasher = Sha256::new();
    hasher.update(target.as_os_str().as_encoded_bytes());
    let link = format!("{:x}", hasher.finalize());

    let reached = match std::fs::metadata(path) {
        Ok(meta) if meta.is_file() => file_fingerprint(path, meta.len(), budget),
        Ok(meta) if meta.is_dir() => "dir".to_string(),
        Ok(_) => "special".to_string(),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => "absent".to_string(),
        Err(_) => "unreadable".to_string(),
    };

    format!("symlink:{link}:{reached}")
}

/// Hash a regular file's bytes, or describe it from its metadata when reading
/// it would blow the run's [`FingerprintBudget`].
///
/// `stat:` is deliberately a different word from `blob:`: it is not a content
/// hash and must never be read as one. Two runs where an over-budget file
/// changed while keeping both its size and its mtime do collide — a far
/// narrower window than the constant "too big" marker an alternative would
/// have used, which would have made *every* oversized file equal to every
/// other.
fn file_fingerprint(path: &Path, len: u64, budget: &mut FingerprintBudget) -> String {
    use sha2::{Digest, Sha256};

    if !budget.take(len) {
        return stat_fingerprint(path, len);
    }

    let Ok(mut file) = std::fs::File::open(path) else {
        return "unreadable".to_string();
    };
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    let mut read: u64 = 0;
    loop {
        match std::io::Read::read(&mut file, &mut buf) {
            Ok(0) => break,
            Ok(n) => {
                read += n as u64;
                hasher.update(&buf[..n]);
                if read > len {
                    // The file grew under the reader. Stop at what the budget
                    // was granted for rather than following it indefinitely; a
                    // partial hash would be labelled as a whole one.
                    return stat_fingerprint(path, read);
                }
            }
            Err(_) => return "unreadable".to_string(),
        }
    }
    format!("blob:{read}:{:x}", hasher.finalize())
}

/// Describe a file that was not read: its size and modification time.
fn stat_fingerprint(path: &Path, len: u64) -> String {
    let mtime = std::fs::metadata(path)
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or_else(|| "unknown".to_string(), |age| age.as_nanos().to_string());
    format!("stat:{len}:{mtime}")
}

/// Fingerprint a directory that the status walk did not descend into.
///
/// Git never recurses into another repository: a submodule (or an embedded
/// checkout) is ONE status entry, so the digest used to record nothing but "a
/// directory is there". A submodule sitting on a different commit, or carrying
/// uncommitted work, is a materially different tree for every scan that follows
/// — and it fingerprinted identically, the collision this digest exists to
/// prevent.
///
/// Its own repository answers both questions cheaply: `HEAD` names the commit,
/// and one status walk says whether anything is uncommitted. That walk is only
/// ever run for a directory the superproject ALREADY reported as dirty, so it
/// costs nothing on a clean tree. Anything unreadable degrades to a coarser
/// marker rather than a false match.
///
/// A dirty nested repository is fingerprinted the same way the superproject is,
/// recursively: `HEAD` plus a clean/dirty flag still collides, because a
/// submodule parked on one commit with two different sets of uncommitted edits
/// is two different trees for every scan that follows. The recursion reads only
/// the nested repository's own dirty subset, so the bound is the same one that
/// makes the top-level walk affordable, and it stops at
/// [`NESTED_REPO_MAX_DEPTH`] — beyond that the coarse `dirty` marker returns,
/// which is exactly what this function reported before, never something weaker.
fn nested_repo_fingerprint(path: &Path, depth: usize, budget: &mut FingerprintBudget) -> String {
    use sha2::{Digest, Sha256};

    // `open`, not `discover`: a plain directory must not resolve to the
    // superproject and report ITS head as the directory's content.
    let Ok(repo) = git2::Repository::open(path) else {
        // Not a repository — an ordinary directory carries no content of its
        // own, and the status walk lists whatever is inside it separately.
        return "dir".to_string();
    };

    let head = repo
        .head()
        .ok()
        .and_then(|head| head.target())
        .map_or_else(|| "unborn".to_string(), |oid| oid.to_string());

    let mut opts = git2::StatusOptions::new();
    // Untracked directories ARE expanded, as at the top level: the entries are
    // what gets hashed, so an unexpanded directory would hide exactly the
    // difference this fingerprint is here to catch.
    opts.include_untracked(true).recurse_untracked_dirs(true);
    let statuses = match repo.statuses(Some(&mut opts)) {
        Ok(statuses) => statuses,
        Err(_) => return format!("gitlink:{head}:unknown"),
    };
    if statuses.is_empty() {
        return format!("gitlink:{head}:clean");
    }
    if depth >= NESTED_REPO_MAX_DEPTH {
        return format!("gitlink:{head}:dirty");
    }

    // The nested walk spends the SAME allowance as the top level: the bound is
    // on one capture's total reading, not on each repository it descends into.
    let inner = render_status_fingerprint(&statuses, repo.workdir(), depth + 1, budget);
    let mut hasher = Sha256::new();
    hasher.update(inner.as_bytes());
    format!("gitlink:{head}:dirty:{:x}", hasher.finalize())
}

/// How many levels of nested repository the digest descends into.
///
/// A submodule holding a submodule is ordinary; a chain deep enough to matter
/// for cost is not, and a cap keeps a pathological (or symlink-looped) nesting
/// from turning one status read into an unbounded walk.
const NESTED_REPO_MAX_DEPTH: usize = 3;

/// The `XY` status pair in the shape of `git status --porcelain`: index status
/// first, worktree status second, an untracked entry as `??`.
///
/// This is a canonical *rendering*, not a capture of the CLI's stdout — it is
/// part of a fingerprint, so it must be stable across git versions and locales
/// rather than byte-identical to any one `git status` invocation.
fn status_codes(status: git2::Status) -> String {
    use git2::Status;

    if status.contains(Status::WT_NEW) && !status.intersects(Status::INDEX_NEW) {
        return "??".to_string();
    }
    let index = if status.contains(Status::INDEX_NEW) {
        'A'
    } else if status.contains(Status::INDEX_MODIFIED) {
        'M'
    } else if status.contains(Status::INDEX_DELETED) {
        'D'
    } else if status.contains(Status::INDEX_RENAMED) {
        'R'
    } else if status.contains(Status::INDEX_TYPECHANGE) {
        'T'
    } else {
        ' '
    };
    let worktree = if status.contains(Status::WT_NEW) {
        'A'
    } else if status.contains(Status::WT_MODIFIED) {
        'M'
    } else if status.contains(Status::WT_DELETED) {
        'D'
    } else if status.contains(Status::WT_RENAMED) {
        'R'
    } else if status.contains(Status::WT_TYPECHANGE) {
        'T'
    } else {
        ' '
    };
    format!("{index}{worktree}")
}

pub(crate) fn classify_quality_failure(
    check_id: &str,
    dashboard_findings: &[DashboardFinding],
    clean_comparison: bool,
) -> QualityFailureClass {
    let mut saw_in_diff = false;
    let mut saw_out_of_diff = false;
    let mut saw_unlocated = false;

    for finding in dashboard_findings
        .iter()
        .filter(|finding| finding.check_id == check_id)
    {
        match finding.in_diff {
            Some(true) => saw_in_diff = true,
            Some(false) => saw_out_of_diff = true,
            // An unlocated finding (`in_diff == None`) could not be resolved to a
            // changed-file decision, so its causation is unknown. It must NOT be
            // silently ignored: a check that mixes an unlocated row with
            // out-of-diff rows cannot be proven purely pre-existing (R5-23).
            None => saw_unlocated = true,
        }
    }

    match (saw_in_diff, saw_out_of_diff) {
        (true, true) => QualityFailureClass::Mixed,
        (true, false) => QualityFailureClass::Introduced,
        // An all-out-of-diff location set may only be downgraded to pre-existing
        // when the scan was a clean comparison (R2-9) AND the check's locations
        // are an exhaustive baseline signal. A dirty local scan can make a
        // working-tree finding look out-of-diff; and for whole-project gates
        // (build/test/typecheck) an out-of-diff location never proves the
        // failure predates the diff. Either way it stays an unclassified failure
        // that still counts against the gate (`has_new_failures`).
        //
        // `!saw_unlocated` is the R5-23 guard: the downgrade requires EVERY
        // finding for this check to be located and out-of-diff. A single
        // unlocated row (a parse-blind or otherwise unclassifiable finding)
        // means the pre-existing proof is incomplete, so the check stays
        // Unclassified and keeps gating rather than approving on partial
        // evidence.
        (false, true)
            if !saw_unlocated && clean_comparison && check_id_is_baseline_signal(check_id) =>
        {
            QualityFailureClass::Preexisting
        }
        (false, true) => QualityFailureClass::Unclassified,
        (false, false) => QualityFailureClass::Unclassified,
    }
}

pub(crate) fn push_quality_failure(
    summary: &mut QualityFailureSummary,
    name: String,
    classification: QualityFailureClass,
    origin: QualityFailureOrigin,
) {
    summary.quality_failures.push(name.clone());
    summary.details.push(QualityFailureDetail {
        name: name.clone(),
        classification,
        origin,
    });

    match classification {
        QualityFailureClass::Introduced => summary.introduced_quality_failures.push(name),
        QualityFailureClass::Preexisting => summary.preexisting_quality_failures.push(name),
        QualityFailureClass::Mixed => summary.mixed_quality_failures.push(name),
        QualityFailureClass::Unclassified => summary.unclassified_quality_failures.push(name),
    }
}

pub(crate) fn build_quality_failure_summary(
    checks: &[CheckResult],
    dashboard_findings: &[DashboardFinding],
    clean_comparison: &CleanComparison,
) -> QualityFailureSummary {
    let mut summary = QualityFailureSummary::default();

    for check in checks
        .iter()
        .filter(|check| quality_downgrade_eligible(check))
    {
        let check_id = check_id_from_name(&check.name);
        // The clean-comparison gate is per-check: only checks that scanned the
        // analysed target may have out-of-diff findings downgraded (R3-16).
        let classification = classify_quality_failure(
            &check_id,
            dashboard_findings,
            clean_comparison.applies_to(&check_id),
        );
        // The origin is recorded alongside the classification: warning-level
        // entries take part in the pre-existing downgrade (that is why they are
        // admitted at all) but never fail the gate — see `has_new_failures`.
        let origin = if check.is_failure() {
            QualityFailureOrigin::Failure
        } else {
            QualityFailureOrigin::Warning
        };
        push_quality_failure(&mut summary, check.name.clone(), classification, origin);
    }

    summary
}

/// Whether a check is eligible for the pre-existing downgrade computation.
///
/// Failures (`Failed`/`Error`) always are. Warning-level baseline-signal checks
/// are too (R2-13): a formatter like Rustfmt reporting `cargo fmt --check`
/// deltas surfaces as `Warnings`, and when every reported location lies outside
/// the diff it is purely pre-existing debt that should get the same
/// preexisting-only downgrade as a failure — otherwise the verdict stays
/// CONDITIONAL instead of PASS-with-caveat.
///
/// Eligibility is NOT the same thing as gating (R2-13 re-adjudicated). Entering
/// the summary is what lets a warning be classified and downgraded; it never
/// makes the warning a failure. The origin recorded in
/// [`QualityFailureDetail::origin`] keeps every `Warnings` entry out of
/// [`QualityFailureSummary::has_new_failures`], whatever it classifies as — so
/// an in-diff warning is still reported as `Introduced` and keeps its review
/// weight through the policy engine, and a warning that produced no locatable
/// finding at all (`Unclassified`) no longer counterfeits "N quality checks
/// failed" and no longer flips `quality_pass` to false.
fn quality_downgrade_eligible(check: &CheckResult) -> bool {
    check.is_failure()
        || (matches!(check.status, crate::checks::CheckStatus::Warnings)
            && check_id_is_baseline_signal(&check_id_from_name(&check.name)))
}

pub(crate) fn quality_failure_reason_text(
    quality_failures: &[String],
    quality_failure_details: &[QualityFailureDetail],
) -> Option<String> {
    if quality_failures.is_empty() {
        return None;
    }

    if quality_failure_details.is_empty() {
        return None;
    }

    // Failures and warnings get SEPARATE sentences: only a check that actually
    // failed may be described with the word "failed". A warning-level baseline
    // signal is reported as what it is — a warning signal — so the gate text can
    // no longer manufacture "N quality checks failed" out of advisory output.
    let mut sentences = Vec::new();
    if let Some(breakdown) = classification_breakdown(quality_failure_details, |detail| {
        detail.origin == QualityFailureOrigin::Failure
    }) {
        sentences.push(format!(
            "{} quality check{} failed ({})",
            breakdown.count,
            if breakdown.count == 1 { "" } else { "s" },
            breakdown.parts.join(", ")
        ));
    }
    if let Some(breakdown) = classification_breakdown(quality_failure_details, |detail| {
        detail.origin == QualityFailureOrigin::Warning
    }) {
        sentences.push(format!(
            "{} warning signal{}: {}",
            breakdown.count,
            if breakdown.count == 1 { "" } else { "s" },
            breakdown.parts.join(", ")
        ));
    }

    if sentences.is_empty() {
        return None;
    }

    Some(sentences.join("; "))
}

struct ClassificationBreakdown {
    count: usize,
    parts: Vec<String>,
}

/// Count the selected details per classification, rendering the same
/// `N introduced, M pre-existing, …` breakdown used by both sentences.
fn classification_breakdown(
    quality_failure_details: &[QualityFailureDetail],
    select: impl Fn(&QualityFailureDetail) -> bool,
) -> Option<ClassificationBreakdown> {
    let mut introduced = 0usize;
    let mut preexisting = 0usize;
    let mut mixed = 0usize;
    let mut unclassified = 0usize;
    let mut count = 0usize;

    for detail in quality_failure_details
        .iter()
        .filter(|detail| select(detail))
    {
        count += 1;
        match detail.classification {
            QualityFailureClass::Introduced => introduced += 1,
            QualityFailureClass::Preexisting => preexisting += 1,
            QualityFailureClass::Mixed => mixed += 1,
            QualityFailureClass::Unclassified => unclassified += 1,
        }
    }

    if count == 0 {
        return None;
    }

    let mut parts = Vec::new();
    if introduced > 0 {
        parts.push(format!("{} introduced", introduced));
    }
    if preexisting > 0 {
        parts.push(format!("{} pre-existing", preexisting));
    }
    if mixed > 0 {
        parts.push(format!("{} mixed", mixed));
    }
    if unclassified > 0 {
        parts.push(format!("{} unclassified", unclassified));
    }

    Some(ClassificationBreakdown { count, parts })
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::artifacts::signal::BreakingRisk;

    fn removed_symbol_finding() -> BreakingFinding {
        BreakingFinding {
            file: "src/lib.rs".to_string(),
            kind: BreakingKind::RemovedSymbol {
                symbol_type: "fn".to_string(),
            },
            line: "pub fn old_api()".to_string(),
            risk_level: BreakingRisk::High,
        }
    }

    fn relocated_symbol_finding() -> BreakingFinding {
        BreakingFinding {
            file: "src/lib.rs".to_string(),
            kind: BreakingKind::RelocatedSymbol {
                symbol_type: "fn".to_string(),
            },
            line: "pub fn moved_api()".to_string(),
            risk_level: BreakingRisk::Low,
        }
    }

    #[test]
    fn breaking_escalation_raises_approve_to_review_required() {
        use crate::policy::engine::MergeRecommendation;

        let mut axis = MergeRecommendation::Approve;
        let reason = apply_breaking_escalation(true, &[removed_symbol_finding()], &mut axis);
        assert_eq!(axis, MergeRecommendation::ReviewRequired);
        assert_eq!(
            reason.as_deref(),
            Some("breaking API change detected: 1 finding")
        );
    }

    #[test]
    fn breaking_escalation_never_produces_block() {
        use crate::policy::engine::MergeRecommendation;

        // Two real breaking findings — escalation still tops out at ReviewRequired.
        let mut axis = MergeRecommendation::Approve;
        let reason = apply_breaking_escalation(
            true,
            &[removed_symbol_finding(), removed_symbol_finding()],
            &mut axis,
        );
        assert_eq!(axis, MergeRecommendation::ReviewRequired);
        assert_eq!(
            reason.as_deref(),
            Some("breaking API change detected: 2 findings")
        );
    }

    #[test]
    fn breaking_escalation_disabled_is_noop() {
        use crate::policy::engine::MergeRecommendation;

        let mut axis = MergeRecommendation::Approve;
        let reason = apply_breaking_escalation(false, &[removed_symbol_finding()], &mut axis);
        assert_eq!(axis, MergeRecommendation::Approve);
        assert!(reason.is_none());
    }

    #[test]
    fn breaking_escalation_ignores_relocations_only() {
        use crate::policy::engine::MergeRecommendation;

        // Relocated/re-exported symbols are non-breaking: no escalation.
        let mut axis = MergeRecommendation::Approve;
        let reason = apply_breaking_escalation(true, &[relocated_symbol_finding()], &mut axis);
        assert_eq!(axis, MergeRecommendation::Approve);
        assert!(reason.is_none());
    }

    #[test]
    fn breaking_escalation_does_not_downgrade_a_higher_axis() {
        use crate::policy::engine::MergeRecommendation;

        // Already Block for another reason — escalation must not lower it.
        let mut axis = MergeRecommendation::Block;
        let reason = apply_breaking_escalation(true, &[removed_symbol_finding()], &mut axis);
        assert_eq!(axis, MergeRecommendation::Block);
        assert!(reason.is_some());
    }

    #[test]
    fn advisory_only_review_required_is_allow_with_review_not_hold() {
        // policy allows merge, nothing failed, only non-blocking review signals.
        // This must read as "mergeable with advisories", not a stop-sign HOLD.
        let view = build_merge_decision_view(
            true,  // policy_allow_merge
            true,  // quality_pass
            false, // recommended_merge (review_required)
            &[],   // quality_failures
            &[],   // quality_failure_details
            &[],   // blocking_issues
            vec!["3 inline findings".to_string()],
        );
        assert_eq!(view.state, MergeDecisionState::AllowWithReview);
        assert_eq!(view.state.gate_label(), "MERGE WITH REVIEW");
        assert_eq!(view.state.card_label(), "GO WITH REVIEW");
    }

    #[test]
    fn new_failure_review_required_stays_hold() {
        // A failing check that belongs to this change keeps a true HOLD.
        let view = build_merge_decision_view(
            true,
            true,
            false,
            &["clippy".to_string()],
            &[QualityFailureDetail {
                name: "clippy".to_string(),
                classification: QualityFailureClass::Introduced,
                origin: QualityFailureOrigin::Failure,
            }],
            &[],
            vec!["clippy returned warnings".to_string()],
        );
        assert_eq!(view.state, MergeDecisionState::Hold);
        assert_eq!(view.state.gate_label(), "HOLD");
    }

    #[test]
    fn preexisting_only_failure_is_allow_with_review_not_hold() {
        let view = build_merge_decision_view(
            true,
            true,
            false,
            &["Semgrep scan".to_string()],
            &[QualityFailureDetail {
                name: "Semgrep scan".to_string(),
                classification: QualityFailureClass::Preexisting,
                origin: QualityFailureOrigin::Failure,
            }],
            &[],
            vec!["Pre-existing quality failures (not from this diff): Semgrep scan".to_string()],
        );

        assert_eq!(view.state, MergeDecisionState::AllowWithReview);
        assert_eq!(view.state.gate_label(), "MERGE WITH REVIEW");
    }

    #[test]
    fn blocking_policy_violation_is_block() {
        let view = build_merge_decision_view(
            false,
            false,
            false,
            &["semgrep".to_string()],
            &[],
            &["secret leak".to_string()],
            vec![],
        );
        assert_eq!(view.state, MergeDecisionState::Block);
    }

    #[test]
    fn clean_approve_is_allow() {
        let view = build_merge_decision_view(true, true, true, &[], &[], &[], vec![]);
        assert_eq!(view.state, MergeDecisionState::Allow);
        assert_eq!(view.state.gate_label(), "MERGE");
    }

    /// Property: for every combination of the two authoritative axes plus
    /// `quality_pass`, the derived scalar decision fields are mutually coherent.
    /// This is the invariant that makes `allow_merge: true` beside a
    /// `CONDITIONAL`/`BLOCK` verdict unrepresentable (PV-03).
    #[test]
    fn derived_decision_fields_are_always_coherent() {
        use crate::policy::engine::{AnalysisStatus, MergeRecommendation};

        let statuses = [
            AnalysisStatus::Complete,
            AnalysisStatus::Degraded,
            AnalysisStatus::Incomplete,
        ];
        let recs = [
            MergeRecommendation::Approve,
            MergeRecommendation::ReviewRequired,
            MergeRecommendation::Block,
        ];

        for status in statuses {
            for rec in recs {
                for quality_pass in [true, false] {
                    let d = derive_decision(status, rec, quality_pass);

                    // Vocabulary is exactly the unified set — no stray HOLD.
                    assert!(
                        matches!(d.verdict, "PASS" | "CONDITIONAL" | "BLOCK"),
                        "unexpected verdict {:?}",
                        d.verdict
                    );

                    // allow_merge is true iff the verdict is a clean PASS.
                    assert_eq!(d.allow_merge, d.verdict == "PASS");

                    // A permissive allow_merge can never coexist with a
                    // non-PASS verdict or a non-approve recommendation.
                    if d.allow_merge {
                        assert_eq!(d.verdict, "PASS");
                        assert_eq!(rec, MergeRecommendation::Approve);
                        assert!(
                            d.recommended_merge,
                            "PASS implies an approve recommendation"
                        );
                        assert_eq!(status, AnalysisStatus::Complete);
                        assert!(quality_pass, "PASS implies quality passed");
                    }

                    // BLOCK verdict iff the recommendation blocks.
                    assert_eq!(d.verdict == "BLOCK", rec == MergeRecommendation::Block);

                    // recommended_merge tracks the approve recommendation only.
                    assert_eq!(d.recommended_merge, rec == MergeRecommendation::Approve);
                }
            }
        }
    }

    fn out_of_diff_finding(check_id: &str) -> DashboardFinding {
        DashboardFinding {
            file: None,
            line: None,
            level: "error",
            check_name: check_id.to_string(),
            check_id: check_id.to_string(),
            message: "finding".to_string(),
            in_diff: Some(false),
        }
    }

    fn in_diff_finding(check_id: &str) -> DashboardFinding {
        DashboardFinding {
            file: None,
            line: None,
            level: "error",
            check_name: check_id.to_string(),
            check_id: check_id.to_string(),
            message: "finding".to_string(),
            in_diff: Some(true),
        }
    }

    fn failed_check(name: &str) -> CheckResult {
        CheckResult {
            name: name.to_string(),
            status: crate::checks::CheckStatus::Failed,
            duration: std::time::Duration::from_millis(1),
            output: String::new(),
            cached: false,
            provenance: None,
        }
    }

    fn warning_check(name: &str) -> CheckResult {
        CheckResult {
            name: name.to_string(),
            status: crate::checks::CheckStatus::Warnings,
            duration: std::time::Duration::from_millis(1),
            output: String::new(),
            cached: false,
            provenance: None,
        }
    }

    #[test]
    fn semgrep_out_of_diff_findings_are_preexisting() {
        // A scanner whose locations are an exhaustive baseline signal: all
        // findings outside the diff really means pre-existing debt.
        let findings = [out_of_diff_finding("semgrep_scan")];
        assert_eq!(
            classify_quality_failure("semgrep_scan", &findings, true),
            QualityFailureClass::Preexisting
        );
    }

    #[test]
    fn dirty_scan_out_of_diff_baseline_signal_is_not_preexisting() {
        // R2-9: on a dirty scan an out-of-diff location may be an uncommitted
        // working-tree finding, so even a baseline-signal check must NOT be
        // downgraded to pre-existing.
        let findings = [out_of_diff_finding("semgrep_scan")];
        assert_eq!(
            classify_quality_failure("semgrep_scan", &findings, false),
            QualityFailureClass::Unclassified
        );
    }

    fn unlocated_finding(check_id: &str) -> DashboardFinding {
        DashboardFinding {
            file: None,
            line: None,
            level: "error",
            check_name: check_id.to_string(),
            check_id: check_id.to_string(),
            message: "unlocated finding".to_string(),
            in_diff: None,
        }
    }

    #[test]
    fn unlocated_plus_out_of_diff_baseline_signal_is_not_preexisting() {
        // R5-23: one finding could not be located (in_diff == None) while the
        // rest sit out-of-diff. The unlocated row must block the pre-existing
        // downgrade — its causation is unknown, so the check cannot be proven
        // purely pre-existing and stays Unclassified (keeps gating).
        let findings = [
            unlocated_finding("semgrep_scan"),
            out_of_diff_finding("semgrep_scan"),
        ];
        assert_eq!(
            classify_quality_failure("semgrep_scan", &findings, true),
            QualityFailureClass::Unclassified
        );
    }

    #[test]
    fn all_out_of_diff_baseline_signal_is_preexisting() {
        // R5-23 control: with every row located and out-of-diff (no unlocated
        // rows) the downgrade to pre-existing still fires as before.
        let findings = [
            out_of_diff_finding("semgrep_scan"),
            out_of_diff_finding("semgrep_scan"),
        ];
        assert_eq!(
            classify_quality_failure("semgrep_scan", &findings, true),
            QualityFailureClass::Preexisting
        );
    }

    #[test]
    fn cargo_test_out_of_diff_findings_are_not_preexisting() {
        // A whole-project gate: an API change in this PR can break a test in an
        // unchanged file. The out-of-diff location does NOT prove the failure
        // predates the diff, so it must not be downgraded to pre-existing.
        let findings = [out_of_diff_finding("cargo_test")];
        assert_eq!(
            classify_quality_failure("cargo_test", &findings, true),
            QualityFailureClass::Unclassified
        );
    }

    #[test]
    fn baseline_signal_membership_excludes_build_test_typecheck_gates() {
        for id in [
            "semgrep_scan",
            "eslint",
            "stylelint",
            "ruff",
            "prettier",
            "rustfmt",
            "cargo_audit",
        ] {
            assert!(
                check_id_is_baseline_signal(id),
                "{id} should be baseline signal"
            );
        }
        // clippy is a whole-project compile gate (`cargo clippy -- -D warnings`),
        // not a per-location formatter, so it must NOT be a baseline signal.
        for id in [
            "cargo_test",
            "cargo",
            "clippy",
            "tsc",
            "tests",
            "pytest",
            "mypy",
        ] {
            assert!(
                !check_id_is_baseline_signal(id),
                "{id} is a whole-project gate, not a baseline signal"
            );
        }
    }

    #[test]
    fn failed_cargo_test_out_of_diff_still_counts_as_new_failure() {
        // The end-to-end guarantee for THREAD 4: a failed whole-project gate
        // whose findings all sit outside the diff is NOT silently downgraded —
        // it stays a new failure that fails `has_new_failures`.
        let findings = [out_of_diff_finding("cargo_test")];
        let summary = build_quality_failure_summary(
            &[failed_check("cargo test")],
            &findings,
            &CleanComparison::for_test(true, true),
        );
        assert!(summary.preexisting_quality_failures.is_empty());
        assert_eq!(summary.unclassified_quality_failures, vec!["cargo test"]);
        assert!(summary.has_new_failures());
    }

    #[test]
    fn failed_semgrep_out_of_diff_is_preexisting_and_not_new() {
        let findings = [out_of_diff_finding("semgrep_scan")];
        let summary = build_quality_failure_summary(
            &[failed_check("Semgrep scan")],
            &findings,
            &CleanComparison::for_test(true, true),
        );
        assert_eq!(summary.preexisting_quality_failures, vec!["Semgrep scan"]);
        assert!(!summary.has_new_failures());
    }

    #[test]
    fn dirty_scan_keeps_out_of_diff_semgrep_failure_as_new() {
        // R2-9 end-to-end: with a dirty scan the out-of-diff semgrep failure is
        // not downgraded, so it stays a new failure that fails the gate.
        let findings = [out_of_diff_finding("semgrep_scan")];
        let summary = build_quality_failure_summary(
            &[failed_check("Semgrep scan")],
            &findings,
            &CleanComparison::for_test(true, false),
        );
        assert!(summary.preexisting_quality_failures.is_empty());
        assert_eq!(summary.unclassified_quality_failures, vec!["Semgrep scan"]);
        assert!(summary.has_new_failures());
    }

    #[test]
    fn remote_target_downgrades_only_snapshot_scanned_checks() {
        // R3-16: on a remote/snapshot target (head != target) the snapshot-backed
        // checks (semgrep + the plan_check_run linters ruff/eslint/stylelint)
        // scanned the target snapshot, so their out-of-diff rows may downgrade.
        // rustfmt scanned the local checkout — a different tree — so its
        // out-of-diff rows must NOT be downgraded to pre-existing.
        //
        // `cargo_audit` is deliberately NOT judged by this list any more: it
        // runs in the target snapshot via `plan_cargo_run`, so the lockfile it
        // read IS the target's and `CargoAuditLockProof::TargetLock` holds. Its
        // membership is asserted below as the proof it now is, not as an
        // inherited property of the rustfmt case.
        //
        // The class this list used to hold for cargo audit — "no proof, no
        // downgrade" — did not move out of the suite when it moved out of the
        // list. `for_test` hands this test a proof that already holds, so the
        // assertion below is about the widening; the premises themselves are
        // guarded through the real resolver by
        // `cargo_audit_lock_proof_reads_the_lockfile_not_the_tree` (dirty lock,
        // unreadable status) and `a_target_without_a_lockfile_proves_nothing`
        // (no lockfile in the target, in BOTH checkout shapes).
        let clean = CleanComparison::for_test(false, true);
        assert!(
            clean.applies_to("semgrep_scan"),
            "semgrep scans the target snapshot, downgrade applies"
        );
        assert!(
            clean.applies_to("ruff"),
            "ruff scans the target snapshot via plan_check_run, downgrade applies"
        );
        assert!(
            clean.applies_to("eslint"),
            "eslint scans the target snapshot via plan_check_run, downgrade applies"
        );
        assert!(
            clean.applies_to("stylelint"),
            "stylelint scans the target snapshot via plan_check_run, downgrade applies"
        );
        assert!(
            !clean.applies_to("rustfmt"),
            "rustfmt scanned the local checkout, downgrade must not apply"
        );
        assert!(
            clean.applies_to("cargo_audit"),
            "cargo audit reads the target snapshot's lockfile, so the proof holds"
        );
        assert!(
            !CleanComparison::for_test_cargo_audit_lock(CargoAuditLockProof::Unproven(
                LockProofGap::DirtyLock,
            ))
            .applies_to("cargo_audit"),
            "without lockfile provenance cargo audit must not downgrade"
        );

        let findings = [out_of_diff_finding("rustfmt")];
        let summary = build_quality_failure_summary(
            &[warning_check("Rustfmt")],
            &findings,
            &CleanComparison::for_test(false, true),
        );
        assert!(
            summary.preexisting_quality_failures.is_empty(),
            "a local-checkout rustfmt finding is not pre-existing on a remote target"
        );
        assert_eq!(summary.unclassified_quality_failures, vec!["Rustfmt"]);

        let semgrep_findings = [out_of_diff_finding("semgrep_scan")];
        let semgrep_summary = build_quality_failure_summary(
            &[failed_check("Semgrep scan")],
            &semgrep_findings,
            &CleanComparison::for_test(false, true),
        );
        assert_eq!(
            semgrep_summary.preexisting_quality_failures,
            vec!["Semgrep scan"]
        );
    }

    /// The defect this proof exists for: a pack that knows `new=0,
    /// pre-existing=2` and blocks anyway, because R2-9 asked a question about
    /// source files of a check whose findings are not source files.
    #[test]
    fn unrelated_worktree_dirt_no_longer_gates_cargo_audit() {
        // Lock untouched by the diff (every advisory out-of-diff), tree dirty in
        // something that has nothing to do with dependencies.
        let dirty_tree = CleanComparison::for_test(true, false);
        assert!(
            dirty_tree.applies_to("cargo_audit"),
            "uncommitted source cannot move an advisory that lives in Cargo.lock"
        );
        let summary = build_quality_failure_summary(
            &[failed_check("Cargo audit")],
            &[out_of_diff_finding("cargo_audit")],
            &dirty_tree,
        );
        assert_eq!(summary.preexisting_quality_failures, vec!["Cargo audit"]);
        assert!(
            !summary.has_new_failures(),
            "a pre-existing-only audit must not fail the quality gate"
        );

        // R2-9 is narrowed, not broken: the same dirty tree still gates every
        // check whose findings ARE source locations.
        for id in ["rustfmt", "semgrep_scan", "eslint", "ruff"] {
            assert!(
                !dirty_tree.applies_to(id),
                "{id} findings are source locations; dirty tree still gates them"
            );
        }
    }

    /// The narrowing has a floor: dirt in the lockfile itself is exactly the
    /// evidence that the audited lock was not the target's.
    #[test]
    fn a_dirty_lockfile_revokes_the_cargo_audit_downgrade() {
        let unproven = CleanComparison::for_test_cargo_audit_lock(CargoAuditLockProof::Unproven(
            LockProofGap::DirtyLock,
        ));
        assert!(!unproven.applies_to("cargo_audit"));
        let summary = build_quality_failure_summary(
            &[failed_check("Cargo audit")],
            &[out_of_diff_finding("cargo_audit")],
            &unproven,
        );
        assert!(summary.preexisting_quality_failures.is_empty());
        assert_eq!(summary.unclassified_quality_failures, vec!["Cargo audit"]);
        assert!(
            summary.has_new_failures(),
            "an unproven lockfile must keep the audit gating"
        );
    }

    /// The proof licenses the downgrade; it never manufactures one. An advisory
    /// the diff introduced stays introduced, and a row with no base comparison
    /// (R5-23) stays unclassified, whatever the lockfile provenance says.
    #[test]
    fn cargo_audit_lock_proof_never_launders_new_or_unknown_advisories() {
        let proven = CleanComparison::for_test(true, true);

        let introduced = build_quality_failure_summary(
            &[failed_check("Cargo audit")],
            &[in_diff_finding("cargo_audit")],
            &proven,
        );
        assert_eq!(introduced.introduced_quality_failures, vec!["Cargo audit"]);
        assert!(introduced.has_new_failures());

        let unknown_base = build_quality_failure_summary(
            &[failed_check("Cargo audit")],
            &[unlocated_finding("cargo_audit")],
            &proven,
        );
        assert!(unknown_base.preexisting_quality_failures.is_empty());
        assert_eq!(
            unknown_base.unclassified_quality_failures,
            vec!["Cargo audit"]
        );
        assert!(
            unknown_base.has_new_failures(),
            "no base audit means no proof, whatever the lockfile says"
        );
    }

    /// R3-14 and R4-20 sit upstream of the lockfile proof and keep their veto:
    /// with no diff baseline at all, "out of diff" is an artefact of an empty
    /// changed-file set, not evidence about a lockfile.
    #[test]
    fn cargo_audit_lock_proof_does_not_outrank_a_missing_baseline() {
        assert!(
            !CleanComparison::for_test_current_only().applies_to("cargo_audit"),
            "--current-only has no baseline to predate (R3-14)"
        );
        assert!(
            !CleanComparison::for_test_no_base_diff().applies_to("cargo_audit"),
            "no resolvable base diff proves nothing pre-existing (R4-20)"
        );
    }

    /// The proof asks about the lockfile alone: a dirty source file in the status
    /// frozen before the checks ran (R4-19) says nothing about it, while a dirty
    /// lock breaks it. The one later reading is of that same tracked lockfile,
    /// never of the tree as it stands at artifact time.
    #[test]
    fn cargo_audit_lock_proof_reads_the_lockfile_not_the_tree() {
        let (tmp, _repo, first, target) = comparison_repo_with_lock();
        let config = crate::config::test_config_builder()
            .repo_root(tmp.path())
            .build();
        let resolve = |dirty: &[&str]| {
            let dirty: std::collections::BTreeSet<String> =
                dirty.iter().map(|path| path.to_string()).collect();
            CleanComparison::resolve(
                &config,
                &resolved_ref(&first.to_string()),
                &[resolved_ref(&target.to_string())],
                Some(dirty.is_empty()),
                LockEvidence::before_checks(Some(&dirty)),
                Some(&first.to_string()),
                &[],
            )
        };

        assert!(
            resolve(&["src/unrelated.rs"]).applies_to("cargo_audit"),
            "a dirty source file says nothing about the lockfile"
        );
        assert!(
            !resolve(&["src/unrelated.rs"]).applies_to("rustfmt"),
            "the same dirt still gates a source-file scanner"
        );
        assert!(
            !resolve(&["Cargo.lock"]).applies_to("cargo_audit"),
            "a dirty lockfile is the one edit that breaks the proof"
        );
        assert!(
            !CleanComparison::resolve(
                &config,
                &resolved_ref(&first.to_string()),
                &[resolved_ref(&target.to_string())],
                None,
                LockEvidence::before_checks(None),
                Some(&first.to_string()),
                &[],
            )
            .applies_to("cargo_audit"),
            "an unreadable status establishes nothing"
        );
    }

    /// P1: the proof never asserted that the lockfile it vouches for EXISTS.
    ///
    /// `cargo audit` does not refuse a crate without `Cargo.lock` — it resolves
    /// one from the registry, audits that, and exits non-zero on a hit
    /// (measured: cargo-audit 0.22.2 generates the file in place). Granting
    /// `TargetLock` there let the gate downgrade a real security failure to
    /// "pre-existing: Cargo.lock unchanged by this PR" in a repository that has
    /// no `Cargo.lock` at all — a false PASS, in both the local and the snapshot
    /// shape, since neither branch consulted the target tree.
    #[test]
    fn a_target_without_a_lockfile_proves_nothing() {
        let (tmp, _repo, first, target) = comparison_repo();
        let config = crate::config::test_config_builder()
            .repo_root(tmp.path())
            .build();
        let clean: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();

        // Local shape: head == target, tree spotless, and still no proof —
        // the missing premise is the file, not the dirt.
        let local = CleanComparison::resolve(
            &config,
            &resolved_ref(&first.to_string()),
            &[resolved_ref(&target.to_string())],
            Some(true),
            LockEvidence::before_checks(Some(&clean)),
            Some(&first.to_string()),
            &[],
        );
        assert!(
            !local.applies_to("cargo_audit"),
            "a clean tree with no Cargo.lock vouches for no lockfile"
        );
        assert_eq!(
            local.cargo_audit_lock_proof(),
            CargoAuditLockProof::Unproven(LockProofGap::NoTargetLock),
            "the gap is named so the merge gate can state it"
        );

        // Snapshot shape: the snapshot is materialised at the target commit, so
        // "the lock it read is the target's" is vacuous when the target has none.
        let snapshot = CleanComparison::resolve(
            &config,
            &resolved_ref(&target.to_string()),
            &[resolved_ref(&first.to_string())],
            Some(true),
            LockEvidence::before_checks(Some(&clean)),
            Some(&first.to_string()),
            &[],
        );
        assert!(
            !snapshot.applies_to("cargo_audit"),
            "a snapshot of a lock-less target audits a lockfile no commit carries"
        );
        assert_eq!(
            snapshot.cargo_audit_lock_proof(),
            CargoAuditLockProof::Unproven(LockProofGap::NoTargetLock)
        );

        // The control: the same two shapes over a target that DOES carry a
        // committed lockfile keep the proof, so this is the file premise and
        // nothing else.
        let (lock_tmp, _lock_repo, lock_first, lock_target) = comparison_repo_with_lock();
        let lock_config = crate::config::test_config_builder()
            .repo_root(lock_tmp.path())
            .build();
        for (target_ref, base_ref) in [(&lock_first, &lock_target), (&lock_target, &lock_first)] {
            let untouched = snapshot_observed(&target_ref.to_string(), &[]);
            let proven = CleanComparison::resolve(
                &lock_config,
                &resolved_ref(&target_ref.to_string()),
                &[resolved_ref(&base_ref.to_string())],
                Some(true),
                LockEvidence {
                    dirty_before_checks: Some(&clean),
                    snapshot_integrity: Some(&untouched),
                    snapshot_root: Some(lock_tmp.path()),
                    ..LockEvidence::default()
                },
                Some(&lock_first.to_string()),
                &[],
            );
            assert_eq!(
                proven.cargo_audit_lock_proof(),
                CargoAuditLockProof::TargetLock
            );
            assert!(proven.applies_to("cargo_audit"));
        }
    }

    #[test]
    fn same_head_snapshot_uses_snapshot_lock_observation() {
        let (tmp, _repo, head, base) = comparison_repo_with_lock();
        let config = crate::config::test_config_builder()
            .repo_root(tmp.path())
            .build();
        let clean = std::collections::BTreeSet::new();
        let head = head.to_string();
        let base = base.to_string();
        let dirty = snapshot_observed(&head, &["Cargo.lock"]);
        let comparison = CleanComparison::resolve(
            &config,
            &resolved_ref(&head),
            &[resolved_ref(&base)],
            Some(true),
            LockEvidence {
                dirty_before_checks: Some(&clean),
                snapshot_integrity: Some(&dirty),
                snapshot_root: Some(tmp.path()),
                ..LockEvidence::default()
            },
            Some(&head),
            &[],
        );
        assert_eq!(
            comparison.cargo_audit_lock_proof(),
            CargoAuditLockProof::Unproven(LockProofGap::DirtyLock),
            "HEAD equality cannot override a rewritten snapshot lock"
        );

        let untouched = snapshot_observed(&head, &[]);
        let comparison = CleanComparison::resolve(
            &config,
            &resolved_ref(&head),
            &[resolved_ref(&base)],
            Some(true),
            LockEvidence {
                dirty_before_checks: Some(&clean),
                snapshot_integrity: Some(&untouched),
                snapshot_root: Some(tmp.path()),
                ..LockEvidence::default()
            },
            Some(&head),
            &[],
        );
        assert_eq!(
            comparison.cargo_audit_lock_proof(),
            CargoAuditLockProof::TargetLock
        );
    }

    const CORE_MANIFEST: &str = "[package]\nname = \"core\"\nversion = \"0.1.0\"\n";
    const CORE_LOCK: &str = "version = 3\n\n[[package]]\nname = \"core\"\n";

    /// What a [`lock_proof_repo`] commit holds at one path.
    enum LockProofEntry {
        Blob(&'static str),
        /// A symlink with this target — committed as a link, not as content.
        #[cfg(unix)]
        Link(&'static str),
    }

    /// A two-commit repository for the cargo-root cases of the lockfile proof.
    /// `HEAD` holds `head_files` and stays checked out with a clean tree;
    /// `target` holds `target_files` and has `HEAD` as its parent.
    fn lock_proof_repo(
        head_files: &[(&str, LockProofEntry)],
        target_files: &[(&str, LockProofEntry)],
    ) -> (tempfile::TempDir, String, String) {
        fn write_tree(
            repo: &git2::Repository,
            root: &std::path::Path,
            files: &[(&str, LockProofEntry)],
        ) -> git2::Oid {
            let mut index = repo.index().unwrap();
            index.clear().unwrap();
            for (path, entry) in files {
                let on_disk = root.join(path);
                std::fs::create_dir_all(on_disk.parent().unwrap()).unwrap();
                match entry {
                    LockProofEntry::Blob(content) => std::fs::write(&on_disk, content).unwrap(),
                    #[cfg(unix)]
                    LockProofEntry::Link(to) => std::os::unix::fs::symlink(to, &on_disk).unwrap(),
                }
                index.add_path(std::path::Path::new(path)).unwrap();
            }
            index.write().unwrap();
            index.write_tree().unwrap()
        }

        let tmp = tempfile::tempdir().unwrap();
        let repo = git2::Repository::init(tmp.path()).unwrap();
        let signature = git2::Signature::now("Test", "test@example.com").unwrap();
        let target_tree = write_tree(&repo, tmp.path(), target_files);
        for (path, _) in target_files {
            let _ = std::fs::remove_file(tmp.path().join(path));
        }
        let head_tree = write_tree(&repo, tmp.path(), head_files);
        let head_tree = repo.find_tree(head_tree).unwrap();
        let head = repo
            .commit(
                Some("HEAD"),
                &signature,
                &signature,
                "operator",
                &head_tree,
                &[],
            )
            .unwrap();
        let target_tree = repo.find_tree(target_tree).unwrap();
        let parent = repo.find_commit(head).unwrap();
        let target = repo
            .commit(
                None,
                &signature,
                &signature,
                "target",
                &target_tree,
                &[&parent],
            )
            .unwrap();
        (tmp, head.to_string(), target.to_string())
    }

    /// The shared snapshot as its check-boundary observations saw it: `changed`
    /// are the only tracked paths any boundary found differing from `target`.
    fn snapshot_observed(
        target: &str,
        changed: &[&str],
    ) -> super::super::signal::SnapshotIntegrity {
        use crate::checks::snapshot_integrity::{SnapshotIntegrityStatus, SnapshotObservation};
        super::super::signal::SnapshotIntegrity::from_observations(
            SnapshotObservation {
                expected_target_sha: target.to_string(),
                observed_head_sha: Some(target.to_string()),
                status: if changed.is_empty() {
                    SnapshotIntegrityStatus::Clean
                } else {
                    SnapshotIntegrityStatus::Modified
                },
                changed_paths: Some(changed.iter().map(|path| path.to_string()).collect()),
                error: None,
                phase: "after-checks",
                check_name: None,
            },
            Vec::new(),
        )
    }

    fn member_config(tmp: &tempfile::TempDir) -> Config {
        let mut config = crate::config::test_config_builder()
            .repo_root(tmp.path())
            .build();
        config.profile.cargo_root = Some(tmp.path().join("crates/core"));
        config
    }

    /// The reviewed commit moved its crate (`crates/core` → `backend`), so the
    /// snapshot run executed cargo in `backend/` — while a stale
    /// `crates/core/Cargo.lock` left behind is exactly the file premise 1 and
    /// the lock-changed classification would have asked about. Proving that
    /// lock proves nothing about the one the audit read, so the proof is
    /// withheld and says why.
    #[test]
    fn a_relocated_cargo_root_withholds_the_lock_proof() {
        use LockProofEntry::Blob;
        let clean: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        let configured = || {
            [
                ("crates/core/Cargo.toml", Blob(CORE_MANIFEST)),
                ("crates/core/Cargo.lock", Blob(CORE_LOCK)),
            ]
        };
        let (tmp, head, target) = lock_proof_repo(
            &configured(),
            &[
                ("backend/Cargo.toml", Blob(CORE_MANIFEST)),
                ("backend/Cargo.lock", Blob(CORE_LOCK)),
                ("crates/core/Cargo.lock", Blob(CORE_LOCK)),
            ],
        );
        let config = member_config(&tmp);
        assert!(
            crate::checks::reviewed_cargo_root_relocated(&config, &target),
            "fixture precondition: cargo resolves the moved crate in backend/"
        );
        let comparison = CleanComparison::resolve(
            &config,
            &resolved_ref(&target),
            &[resolved_ref(&head)],
            Some(true),
            LockEvidence::before_checks(Some(&clean)),
            Some(&head),
            &[],
        );
        assert_eq!(
            comparison.cargo_audit_lock_proof(),
            CargoAuditLockProof::Unproven(LockProofGap::RelocatedCargoRoot)
        );
        assert!(!comparison.applies_to("cargo_audit"));

        // The control: a target that keeps the crate where it was configured.
        let (tmp, head, target) = lock_proof_repo(&configured(), &configured());
        let config = member_config(&tmp);
        let relocated = crate::checks::reviewed_cargo_root_relocated(&config, &target);
        assert!(!relocated, "a crate that stayed put is not relocated");
        let untouched = snapshot_observed(&target, &[]);
        let kept = CleanComparison::resolve(
            &config,
            &resolved_ref(&target),
            &[resolved_ref(&head)],
            Some(true),
            LockEvidence {
                dirty_before_checks: Some(&clean),
                snapshot_integrity: Some(&untouched),
                snapshot_root: Some(tmp.path()),
                ..LockEvidence::default()
            },
            Some(&head),
            &[],
        );
        assert_eq!(
            kept.cargo_audit_lock_proof(),
            CargoAuditLockProof::TargetLock
        );
    }

    /// cargo-audit's configuration is substrate, like the lockfile. No audit
    /// reads the base's `.cargo/audit.toml`, so a pull request that only drops
    /// an ignored advisory fails the audit with the lockfile unchanged. Every
    /// finding then sits out-of-diff, and the lockfile premises, which all still
    /// hold, must not license the downgrade on their own: the proof is withheld
    /// and names the configuration as its gap.
    ///
    /// The file is compared at the one path cargo-audit reads, so a rename away
    /// counts even though the changed-file row keeps only the new path. A
    /// member's audit never reads the repository root's file. A symlinked
    /// `.cargo` is never vouched for, and neither is a configuration committed
    /// under another case, which a case-insensitive checkout reads in its place.
    #[test]
    fn a_changed_cargo_audit_config_withholds_the_lock_proof() {
        use LockProofEntry::Blob;
        const IGNORING: &str = "[advisories]\nignore = [\"RUSTSEC-2020-0001\"]\n";
        const STRICT: &str = "[advisories]\nignore = []\n";
        const KEPT: CargoAuditLockProof = CargoAuditLockProof::TargetLock;
        const WITHHELD: CargoAuditLockProof =
            CargoAuditLockProof::Unproven(LockProofGap::AuditConfigChanged);

        /// The proof for a snapshot review of `target_files` against
        /// `head_files`, whose lockfile is the same committed file on both sides.
        fn proof(
            head_files: &[(&str, LockProofEntry)],
            target_files: &[(&str, LockProofEntry)],
            member: bool,
        ) -> CargoAuditLockProof {
            let (tmp, head, target) = lock_proof_repo(head_files, target_files);
            let config = if member {
                member_config(&tmp)
            } else {
                crate::config::test_config_builder()
                    .repo_root(tmp.path())
                    .build()
            };
            let diff = crate::git::Repository::open(tmp.path())
                .unwrap()
                .diff_refs(&resolved_ref(&head), &resolved_ref(&target))
                .unwrap();
            let clean = std::collections::BTreeSet::new();
            let untouched = snapshot_observed(&target, &[]);
            let comparison = CleanComparison::resolve(
                &config,
                &resolved_ref(&target),
                &[resolved_ref(&head)],
                Some(true),
                LockEvidence {
                    dirty_before_checks: Some(&clean),
                    snapshot_integrity: Some(&untouched),
                    snapshot_root: Some(tmp.path()),
                    ..LockEvidence::default()
                },
                Some(&head),
                &[diff],
            );
            let proof = comparison.cargo_audit_lock_proof();
            assert_eq!(
                comparison.applies_to("cargo_audit"),
                proof == CargoAuditLockProof::TargetLock,
                "the downgrade follows the proof"
            );
            proof
        }

        let root = |config: Option<&'static str>| {
            let mut files = vec![
                ("Cargo.toml", Blob(CORE_MANIFEST)),
                ("Cargo.lock", Blob(CORE_LOCK)),
            ];
            if let Some(config) = config {
                files.push((".cargo/audit.toml", Blob(config)));
            }
            files
        };

        assert_eq!(
            proof(&root(None), &root(None), false),
            KEPT,
            "no configuration on either side keeps the proof"
        );
        assert_eq!(
            proof(&root(Some(IGNORING)), &root(Some(IGNORING)), false),
            KEPT,
            "an unchanged configuration keeps the proof"
        );
        assert_eq!(
            proof(&root(Some(IGNORING)), &root(Some(STRICT)), false),
            WITHHELD,
            "a dropped ignore can fail the audit with the lockfile unchanged"
        );
        assert_eq!(
            proof(&root(None), &root(Some(STRICT)), false),
            WITHHELD,
            "an added project configuration replaces the user's"
        );
        assert_eq!(
            proof(&root(Some(IGNORING)), &root(None), false),
            WITHHELD,
            "a deleted configuration drops its ignores"
        );
        let mut renamed = root(None);
        renamed.push((".cargo/audit.toml.off", Blob(IGNORING)));
        assert_eq!(
            proof(&root(Some(IGNORING)), &renamed, false),
            WITHHELD,
            "a rename away counts, though the changed-file row names only the new path"
        );

        const CASED: CargoAuditLockProof =
            CargoAuditLockProof::Unproven(LockProofGap::AuditConfigCaseVariant);
        let cased = |config: &'static str| {
            let mut files = root(None);
            files.push((".cargo/Audit.toml", Blob(config)));
            files
        };
        assert_eq!(
            proof(&cased(IGNORING), &cased(IGNORING), false),
            CASED,
            "a case-insensitive checkout reads .cargo/Audit.toml, which no exact path compares"
        );
        assert_eq!(
            proof(&cased(IGNORING), &root(Some(IGNORING)), false),
            CASED,
            "a variant on the base side of a diff counts too"
        );

        let member = |root_config: &'static str, own_config: &'static str| {
            vec![
                ("crates/core/Cargo.toml", Blob(CORE_MANIFEST)),
                ("crates/core/Cargo.lock", Blob(CORE_LOCK)),
                (".cargo/audit.toml", Blob(root_config)),
                ("crates/core/.cargo/audit.toml", Blob(own_config)),
            ]
        };
        assert_eq!(
            proof(&member(IGNORING, IGNORING), &member(STRICT, IGNORING), true),
            KEPT,
            "a member's audit never reads the repository root's configuration"
        );
        assert_eq!(
            proof(&member(IGNORING, IGNORING), &member(IGNORING, STRICT), true),
            WITHHELD,
            "the member's own configuration is the one the audit read"
        );

        #[cfg(unix)]
        {
            let linked = || {
                let mut files = root(None);
                files.push(("config/audit.toml", Blob(IGNORING)));
                files.push((".cargo", LockProofEntry::Link("config")));
                files
            };
            assert_eq!(
                proof(&linked(), &linked(), false),
                WITHHELD,
                "a symlinked .cargo resolves outside what the tree describes"
            );
        }
    }

    /// P1 (review): the committed comparison speaks only for the commits, and a
    /// local review audits the checkout. A `.cargo/audit.toml` edited there,
    /// whether staged, unstaged, untracked or ignored, is the configuration
    /// `cargo audit` applied. It can ignore the advisory the change introduced
    /// while pre-existing ones still fail, and the downgrade would then pass
    /// the change. The proof is withheld whenever the scanned tree's copy is
    /// not the target's, however the edit reached it.
    #[test]
    fn a_scanned_cargo_audit_config_that_is_not_the_targets_withholds_the_lock_proof() {
        use LockProofEntry::Blob;
        const COMMITTED: &str = "[advisories]\nignore = []\n";
        const LOCAL: &str = "[advisories]\nignore = [\"RUSTSEC-2020-0001\"]\n";
        const CONFIG: &str = ".cargo/audit.toml";
        const KEPT: CargoAuditLockProof = CargoAuditLockProof::TargetLock;
        const WITHHELD: CargoAuditLockProof =
            CargoAuditLockProof::Unproven(LockProofGap::AuditConfigChanged);

        fn files(config: Option<&'static str>) -> Vec<(&'static str, LockProofEntry)> {
            let mut files = vec![
                ("Cargo.toml", Blob(CORE_MANIFEST)),
                ("Cargo.lock", Blob(CORE_LOCK)),
            ];
            if let Some(config) = config {
                files.push((CONFIG, Blob(config)));
            }
            files
        }

        /// The proof for a local review of the checked-out target. `before`
        /// edits the checkout ahead of the status read frozen before the
        /// checks, and `during` edits it afterwards, as a check would. Also
        /// returns whether that status read listed the configuration or a
        /// parent of it.
        fn local(
            committed: Option<&'static str>,
            before: fn(&std::path::Path),
            during: fn(&std::path::Path),
        ) -> (CargoAuditLockProof, bool) {
            let (tmp, head, target) = lock_proof_repo(&files(committed), &files(committed));
            git2::Repository::open(tmp.path())
                .unwrap()
                .set_head_detached(git2::Oid::from_str(&target).unwrap())
                .unwrap();
            before(tmp.path());
            let provenance = capture_worktree_provenance(tmp.path());
            during(tmp.path());
            let config = crate::config::test_config_builder()
                .repo_root(tmp.path())
                .build();
            let diff = crate::git::Repository::open(tmp.path())
                .unwrap()
                .diff_refs(&resolved_ref(&head), &resolved_ref(&target))
                .unwrap();
            let comparison = CleanComparison::resolve(
                &config,
                &resolved_ref(&target),
                &[resolved_ref(&head)],
                provenance.clean,
                LockEvidence::before_checks(provenance.dirty_paths.as_ref()),
                provenance.head_sha.as_deref(),
                &[diff],
            );
            let proof = comparison.cargo_audit_lock_proof();
            assert_eq!(
                comparison.applies_to("cargo_audit"),
                proof == KEPT,
                "the downgrade follows the proof"
            );
            let listed = provenance
                .dirty_paths
                .expect("the status was read")
                .iter()
                .any(|changed| path_or_parent_is(CONFIG, changed));
            (proof, listed)
        }

        fn nothing(_: &std::path::Path) {}
        fn unrelated(root: &std::path::Path) {
            std::fs::write(root.join("notes.txt"), "scratch\n").unwrap();
        }
        fn edit(root: &std::path::Path) {
            std::fs::create_dir_all(root.join(".cargo")).unwrap();
            std::fs::write(root.join(CONFIG), LOCAL).unwrap();
        }
        fn stage(root: &std::path::Path) {
            edit(root);
            let repo = git2::Repository::open(root).unwrap();
            let mut index = repo.index().unwrap();
            index.add_path(std::path::Path::new(CONFIG)).unwrap();
            index.write().unwrap();
        }
        fn delete(root: &std::path::Path) {
            std::fs::remove_file(root.join(CONFIG)).unwrap();
        }
        fn ignore_and_edit(root: &std::path::Path) {
            std::fs::create_dir_all(root.join(".git/info")).unwrap();
            std::fs::write(root.join(".git/info/exclude"), ".cargo/\n").unwrap();
            edit(root);
        }

        assert_eq!(
            local(Some(COMMITTED), nothing, nothing).0,
            KEPT,
            "a checkout that is the target keeps the proof"
        );
        assert_eq!(
            local(None, nothing, nothing).0,
            KEPT,
            "no configuration in the target or the checkout keeps the proof"
        );
        assert_eq!(
            local(Some(COMMITTED), unrelated, nothing).0,
            KEPT,
            "an unrelated dirty file says nothing about the configuration"
        );
        assert_eq!(
            local(Some(COMMITTED), edit, nothing),
            (WITHHELD, true),
            "an unstaged edit is the configuration the audit applied"
        );
        assert_eq!(
            local(Some(COMMITTED), stage, nothing),
            (WITHHELD, true),
            "so is a staged one"
        );
        assert_eq!(
            local(Some(COMMITTED), delete, nothing),
            (WITHHELD, true),
            "a deleted configuration drops what the target's applies"
        );
        assert_eq!(
            local(None, edit, nothing),
            (WITHHELD, true),
            "an untracked configuration applies where the target has none"
        );
        assert_eq!(
            local(None, ignore_and_edit, nothing),
            (WITHHELD, false),
            "an ignored configuration never reaches the status read and is still found"
        );
        assert_eq!(
            local(Some(COMMITTED), nothing, edit),
            (WITHHELD, false),
            "an edit after the status read is found by the read after the checks"
        );

        #[cfg(unix)]
        {
            fn link(root: &std::path::Path) {
                std::fs::create_dir_all(root.join("local")).unwrap();
                std::fs::write(root.join("local/audit.toml"), LOCAL).unwrap();
                std::os::unix::fs::symlink("local", root.join(".cargo")).unwrap();
            }
            assert_eq!(
                local(None, link, nothing),
                (WITHHELD, true),
                "an untracked symlinked .cargo is listed by its own path"
            );
        }

        // Snapshot shape: the audit read a tree materialised from the target,
        // so only a check makes the file differ: a boundary that saw the
        // tracked file rewritten, or, where the target has none, a file a
        // check left at the path, which no boundary lists.
        enum SnapshotTree {
            Unreadable,
            Materialised,
            Generated,
        }
        fn snapshot(
            committed: Option<&'static str>,
            changed: &[&str],
            tree: SnapshotTree,
        ) -> CargoAuditLockProof {
            let (tmp, head, target) = lock_proof_repo(&files(committed), &files(committed));
            let config = crate::config::test_config_builder()
                .repo_root(tmp.path())
                .build();
            let diff = crate::git::Repository::open(tmp.path())
                .unwrap()
                .diff_refs(&resolved_ref(&head), &resolved_ref(&target))
                .unwrap();
            // The snapshot's own working tree, apart from the operator's
            // checkout.
            let worktree = tempfile::tempdir().unwrap();
            if let SnapshotTree::Generated = tree {
                edit(worktree.path());
            }
            let clean = std::collections::BTreeSet::new();
            let observed = snapshot_observed(&target, changed);
            CleanComparison::resolve(
                &config,
                &resolved_ref(&target),
                &[resolved_ref(&head)],
                Some(true),
                LockEvidence {
                    dirty_before_checks: Some(&clean),
                    snapshot_integrity: Some(&observed),
                    snapshot_root: match tree {
                        SnapshotTree::Unreadable => None,
                        _ => Some(worktree.path()),
                    },
                    ..LockEvidence::default()
                },
                Some(&head),
                std::slice::from_ref(&diff),
            )
            .cargo_audit_lock_proof()
        }
        use SnapshotTree::{Generated, Materialised, Unreadable};
        assert_eq!(
            snapshot(Some(COMMITTED), &[], Materialised),
            KEPT,
            "an untouched snapshot keeps the proof"
        );
        assert_eq!(
            snapshot(Some(COMMITTED), &["notes.txt"], Materialised),
            KEPT,
            "an unrelated rewrite says nothing about the configuration"
        );
        assert_eq!(
            snapshot(Some(COMMITTED), &[CONFIG], Materialised),
            WITHHELD,
            "a check that rewrote the configuration in the snapshot"
        );
        assert_eq!(
            snapshot(None, &[], Materialised),
            KEPT,
            "no configuration in the target or the snapshot keeps the proof"
        );
        assert_eq!(
            snapshot(None, &[], Generated),
            WITHHELD,
            "a configuration a check generated applies where the target has none, \
             though no boundary lists an untracked file"
        );
        assert_eq!(
            snapshot(None, &[], Unreadable),
            WITHHELD,
            "a snapshot tree nobody can look at vouches for no absence"
        );
    }

    /// cargo-audit falls back to `$CARGO_HOME/audit.toml` where the cargo root
    /// has no configuration, and reads its advisory database under
    /// `$CARGO_HOME` either way. A relative `CARGO_HOME` resolves against the
    /// directory the audit ran in, so both are files in the scanned tree, and
    /// the change can edit them with the lockfile untouched.
    #[test]
    fn a_relative_cargo_home_withholds_the_lock_proof() {
        use LockProofEntry::Blob;
        let files = || {
            [
                ("Cargo.toml", Blob(CORE_MANIFEST)),
                ("Cargo.lock", Blob(CORE_LOCK)),
            ]
        };
        let (tmp, head, target) = lock_proof_repo(&files(), &files());
        let config = crate::config::test_config_builder()
            .repo_root(tmp.path())
            .build();
        let clean = std::collections::BTreeSet::new();
        let untouched = snapshot_observed(&target, &[]);
        let proof = |cargo_home: Option<&std::ffi::OsStr>| {
            CleanComparison::resolve(
                &config,
                &resolved_ref(&target),
                &[resolved_ref(&head)],
                Some(true),
                LockEvidence {
                    dirty_before_checks: Some(&clean),
                    snapshot_integrity: Some(&untouched),
                    snapshot_root: Some(tmp.path()),
                    cargo_home,
                    operator_home: None,
                },
                Some(&head),
                &[],
            )
            .cargo_audit_lock_proof()
        };
        let absolute = std::env::temp_dir().join("cargo-home");
        assert_eq!(proof(None), CargoAuditLockProof::TargetLock, "unset");
        assert_eq!(
            proof(Some("".as_ref())),
            CargoAuditLockProof::TargetLock,
            "an empty CARGO_HOME counts as unset"
        );
        assert_eq!(
            proof(Some(absolute.as_os_str())),
            CargoAuditLockProof::TargetLock,
            "an absolute CARGO_HOME lies outside what the change edits"
        );
        for relative in [".cargo-home", "../cargo-home", "."] {
            assert_eq!(
                proof(Some(relative.as_ref())),
                CargoAuditLockProof::Unproven(LockProofGap::RelativeCargoHome),
                "CARGO_HOME={relative}"
            );
        }
    }

    /// An absolute `CARGO_HOME` is the environment's only while it lies
    /// outside the checkout and the scanned tree. Inside either, however it is
    /// spelled or linked, its `audit.toml` and advisory database are files the
    /// change can edit with the lockfile untouched.
    #[test]
    fn an_absolute_cargo_home_inside_the_tree_withholds_the_lock_proof() {
        use LockProofEntry::Blob;
        let files = || {
            [
                ("Cargo.toml", Blob(CORE_MANIFEST)),
                ("Cargo.lock", Blob(CORE_LOCK)),
            ]
        };
        let (tmp, head, target) = lock_proof_repo(&files(), &files());
        let config = crate::config::test_config_builder()
            .repo_root(tmp.path())
            .build();
        let parent = tmp.path().parent().unwrap();
        let snapshot = tempfile::tempdir_in(parent).unwrap();
        let outside = tempfile::tempdir_in(parent).unwrap();
        let clean = std::collections::BTreeSet::new();
        let untouched = snapshot_observed(&target, &[]);
        let proof = |cargo_home: &std::path::Path| {
            CleanComparison::resolve(
                &config,
                &resolved_ref(&target),
                &[resolved_ref(&head)],
                Some(true),
                LockEvidence {
                    dirty_before_checks: Some(&clean),
                    snapshot_integrity: Some(&untouched),
                    snapshot_root: Some(snapshot.path()),
                    cargo_home: Some(cargo_home.as_os_str()),
                    operator_home: None,
                },
                Some(&head),
                &[],
            )
            .cargo_audit_lock_proof()
        };
        let repo_name = tmp.path().file_name().unwrap();
        assert_eq!(
            proof(&outside.path().join("cargo-home")),
            CargoAuditLockProof::TargetLock,
            "a sibling of both trees is the environment's"
        );
        let inside = [
            (tmp.path().join(".cargo-home"), "in the checkout"),
            (tmp.path().to_path_buf(), "the checkout itself"),
            (snapshot.path().join("home"), "in the scanned tree"),
            (
                outside
                    .path()
                    .join("..")
                    .join(repo_name)
                    .join(".cargo-home"),
                "spelled through `..`",
            ),
            (
                parent
                    .join(repo_name.to_ascii_uppercase())
                    .join(".cargo-home"),
                "spelled in another case",
            ),
        ];
        for (home, why) in inside {
            assert_eq!(
                proof(&home),
                CargoAuditLockProof::Unproven(LockProofGap::InTreeCargoHome),
                "{why}: CARGO_HOME={}",
                home.display()
            );
        }
        #[cfg(unix)]
        {
            let link = outside.path().join("link");
            std::os::unix::fs::symlink(tmp.path(), &link).unwrap();
            assert_eq!(
                proof(&link.join(".cargo-home")),
                CargoAuditLockProof::Unproven(LockProofGap::InTreeCargoHome),
                "reached through a link, not yet created"
            );
        }
    }

    /// With `CARGO_HOME` unset or empty, cargo-audit's Cargo home is
    /// `$HOME/.cargo`. A `HOME` inside the checkout or the snapshot puts that
    /// home's `audit.toml` in the tree, where, for a member cargo root with no
    /// configuration of its own, it is the repository root's
    /// `.cargo/audit.toml`.
    #[test]
    fn a_home_derived_cargo_home_is_judged_like_an_inherited_one() {
        use LockProofEntry::Blob;
        let files = || {
            [
                ("Cargo.toml", Blob(CORE_MANIFEST)),
                ("Cargo.lock", Blob(CORE_LOCK)),
            ]
        };
        let (tmp, head, target) = lock_proof_repo(&files(), &files());
        let config = crate::config::test_config_builder()
            .repo_root(tmp.path())
            .build();
        let parent = tmp.path().parent().unwrap();
        let snapshot = tempfile::tempdir_in(parent).unwrap();
        let outside = tempfile::tempdir_in(parent).unwrap();
        let clean = std::collections::BTreeSet::new();
        let untouched = snapshot_observed(&target, &[]);
        let proof = |cargo_home: Option<&str>, operator_home: Option<&std::path::Path>| {
            CleanComparison::resolve(
                &config,
                &resolved_ref(&target),
                &[resolved_ref(&head)],
                Some(true),
                LockEvidence {
                    dirty_before_checks: Some(&clean),
                    snapshot_integrity: Some(&untouched),
                    snapshot_root: Some(snapshot.path()),
                    cargo_home: cargo_home.map(std::ffi::OsStr::new),
                    operator_home,
                },
                Some(&head),
                &[],
            )
            .cargo_audit_lock_proof()
        };
        let in_tree = CargoAuditLockProof::Unproven(LockProofGap::InTreeCargoHome);
        assert_eq!(
            proof(None, Some(outside.path())),
            CargoAuditLockProof::TargetLock,
            "a HOME outside both trees is the environment's"
        );
        assert_eq!(
            proof(None, Some(tmp.path())),
            in_tree,
            "HOME at the checkout root"
        );
        assert_eq!(
            proof(None, Some(snapshot.path())),
            in_tree,
            "HOME at the snapshot root"
        );
        assert_eq!(
            proof(Some(""), Some(tmp.path())),
            in_tree,
            "an empty CARGO_HOME falls back to HOME"
        );
        let external = outside.path().join("cargo-home");
        assert_eq!(
            proof(external.to_str(), Some(tmp.path())),
            CargoAuditLockProof::TargetLock,
            "a set CARGO_HOME wins over HOME"
        );
        for relative in ["", "home"] {
            assert_eq!(
                proof(None, Some(std::path::Path::new(relative))),
                CargoAuditLockProof::Unproven(LockProofGap::RelativeCargoHome),
                "HOME={relative:?}"
            );
        }
        assert_eq!(
            proof(None, None),
            CargoAuditLockProof::TargetLock,
            "with no home at all cargo-audit reads no fallback configuration"
        );
    }

    /// An external Cargo home is the environment's only while what cargo-audit
    /// reads through it stays outside too. A link at its `audit.toml` or
    /// `advisory-db`, or a configured `[database] path` that is relative or
    /// leads into a tree, puts the audit's policy in files the change can
    /// edit. A committed configuration can name such a database as well, and
    /// then it counts even with no home at all.
    #[test]
    fn what_an_external_cargo_home_leads_to_is_judged_where_it_leads() {
        use LockProofEntry::Blob;
        let files = || {
            [
                ("Cargo.toml", Blob(CORE_MANIFEST)),
                ("Cargo.lock", Blob(CORE_LOCK)),
            ]
        };
        let (tmp, head, target) = lock_proof_repo(&files(), &files());
        let config = crate::config::test_config_builder()
            .repo_root(tmp.path())
            .build();
        let parent = tmp.path().parent().unwrap();
        let snapshot = tempfile::tempdir_in(parent).unwrap();
        let outside = tempfile::tempdir_in(parent).unwrap();
        let clean = std::collections::BTreeSet::new();
        let untouched = snapshot_observed(&target, &[]);
        let proof = |cargo_home: &std::path::Path| {
            CleanComparison::resolve(
                &config,
                &resolved_ref(&target),
                &[resolved_ref(&head)],
                Some(true),
                LockEvidence {
                    dirty_before_checks: Some(&clean),
                    snapshot_integrity: Some(&untouched),
                    snapshot_root: Some(snapshot.path()),
                    cargo_home: Some(cargo_home.as_os_str()),
                    operator_home: None,
                },
                Some(&head),
                &[],
            )
            .cargo_audit_lock_proof()
        };
        let home_with = |audit_toml: Option<String>| {
            let home = tempfile::tempdir_in(parent).unwrap();
            if let Some(audit_toml) = audit_toml {
                std::fs::write(home.path().join("audit.toml"), audit_toml).unwrap();
            }
            home
        };
        let database =
            |path: &std::path::Path| format!("[database]\npath = '{}'\n", path.to_str().unwrap());
        let led_in = CargoAuditLockProof::Unproven(LockProofGap::AuditInputInTree);

        for (home, why) in [
            (home_with(None), "no fallback configuration"),
            (
                home_with(Some("[advisories]\nignore = []\n".into())),
                "a fallback configuration naming no database",
            ),
            (
                home_with(Some(database(&outside.path().join("advisory-db")))),
                "a database outside both trees",
            ),
        ] {
            assert_eq!(
                proof(home.path()),
                CargoAuditLockProof::TargetLock,
                "{why} is the environment's"
            );
        }
        for (home, why) in [
            (
                home_with(Some("[database]\npath = 'advisory-db'\n".into())),
                "a relative database path, opened where the audit ran",
            ),
            (
                home_with(Some(database(&tmp.path().join("vendor/advisory-db")))),
                "a database in the checkout",
            ),
            (
                home_with(Some(database(&snapshot.path().join("advisory-db")))),
                "a database in the scanned tree",
            ),
        ] {
            assert_eq!(proof(home.path()), led_in, "{why}");
        }
        let unreadable = home_with(None);
        std::fs::create_dir(unreadable.path().join("audit.toml")).unwrap();
        assert_eq!(
            proof(unreadable.path()),
            led_in,
            "a fallback configuration that cannot be read shows nothing"
        );
        #[cfg(unix)]
        {
            let policy = tmp.path().join("policy.toml");
            std::fs::write(&policy, "[advisories]\nignore = []\n").unwrap();
            let linked_config = home_with(None);
            std::os::unix::fs::symlink(&policy, linked_config.path().join("audit.toml")).unwrap();
            assert_eq!(
                proof(linked_config.path()),
                led_in,
                "a fallback configuration linked into the checkout"
            );
            let advisories = tmp.path().join("advisory-db");
            std::fs::create_dir(&advisories).unwrap();
            let linked_database = home_with(None);
            std::os::unix::fs::symlink(&advisories, linked_database.path().join("advisory-db"))
                .unwrap();
            assert_eq!(
                proof(linked_database.path()),
                led_in,
                "an advisory database linked into the checkout"
            );
        }

        // A committed configuration is the one applied, fallback or not.
        let committed = |audit_toml: &'static str| {
            [
                ("Cargo.toml", Blob(CORE_MANIFEST)),
                ("Cargo.lock", Blob(CORE_LOCK)),
                (".cargo/audit.toml", Blob(audit_toml)),
            ]
        };
        for (audit_toml, expected, why) in [
            (
                "[advisories]\nignore = []\n",
                CargoAuditLockProof::TargetLock,
                "a committed configuration naming no database",
            ),
            (
                "[database]\npath = 'vendor/advisory-db'\nfetch = false\n",
                led_in,
                "a committed configuration naming an in-tree database",
            ),
        ] {
            let (tmp, head, target) =
                lock_proof_repo(&committed(audit_toml), &committed(audit_toml));
            let config = crate::config::test_config_builder()
                .repo_root(tmp.path())
                .build();
            let untouched = snapshot_observed(&target, &[]);
            for cargo_home in [None, Some(outside.path().as_os_str())] {
                assert_eq!(
                    CleanComparison::resolve(
                        &config,
                        &resolved_ref(&target),
                        &[resolved_ref(&head)],
                        Some(true),
                        LockEvidence {
                            dirty_before_checks: Some(&clean),
                            snapshot_integrity: Some(&untouched),
                            snapshot_root: None,
                            cargo_home,
                            operator_home: None,
                        },
                        Some(&head),
                        &[],
                    )
                    .cargo_audit_lock_proof(),
                    expected,
                    "{why}, CARGO_HOME={cargo_home:?}"
                );
            }
        }
    }

    #[test]
    fn path_or_parent_is_matches_whole_components() {
        assert!(path_or_parent_is(".cargo/audit.toml", ".cargo/audit.toml"));
        assert!(path_or_parent_is(".cargo/audit.toml", ".cargo"));
        assert!(path_or_parent_is(".cargo/audit.toml", ".cargo/"));
        assert!(path_or_parent_is("crates/core/.cargo/audit.toml", "crates"));
        assert!(path_or_parent_is(".cargo/audit.toml", ".cargo/Audit.toml"));
        assert!(path_or_parent_is(".cargo/audit.toml", ".CARGO"));
        assert!(!path_or_parent_is(".cargo/audit.toml", ""));
        assert!(!path_or_parent_is(
            ".cargo/audit.toml",
            ".cargo/audit.toml.off"
        ));
        assert!(!path_or_parent_is(".cargo/audit.toml", ".car"));
        assert!(!path_or_parent_is(".cargo/audit.toml", ".cargo/audit"));
        assert!(!path_or_parent_is(
            ".cargo/audit.toml",
            ".cargo/audit.toml/x"
        ));
    }

    /// `cargo audit` reads `Cargo.lock` in the directory it runs in and never a
    /// workspace root's, so a root lock beside a lock-less member vouches for a
    /// file the audit did not read — in both checkout shapes.
    #[test]
    fn a_workspace_root_lock_does_not_vouch_for_a_lockless_member() {
        use LockProofEntry::Blob;
        let clean: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        let files = || {
            [
                ("crates/core/Cargo.toml", Blob(CORE_MANIFEST)),
                ("Cargo.lock", Blob(CORE_LOCK)),
            ]
        };
        let (tmp, head, target) = lock_proof_repo(&files(), &files());
        let config = member_config(&tmp);
        for (reviewed, base) in [(&head, &target), (&target, &head)] {
            let comparison = CleanComparison::resolve(
                &config,
                &resolved_ref(reviewed),
                &[resolved_ref(base)],
                Some(true),
                LockEvidence::before_checks(Some(&clean)),
                Some(&head),
                &[],
            );
            assert_eq!(
                comparison.cargo_audit_lock_proof(),
                CargoAuditLockProof::Unproven(LockProofGap::NoTargetLock),
                "reviewing {reviewed}: the root lock is not the member's"
            );
        }
    }

    /// A member lock committed as a symlink is not a committed lockfile, and
    /// the root lock it points at does not stand in for it.
    #[cfg(unix)]
    #[test]
    fn a_symlinked_member_lock_is_not_a_target_lock() {
        use LockProofEntry::{Blob, Link};
        let clean: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        let files = || {
            [
                ("crates/core/Cargo.toml", Blob(CORE_MANIFEST)),
                ("crates/core/Cargo.lock", Link("../../Cargo.lock")),
                ("Cargo.lock", Blob(CORE_LOCK)),
            ]
        };
        let (tmp, head, target) = lock_proof_repo(&files(), &files());
        let comparison = CleanComparison::resolve(
            &member_config(&tmp),
            &resolved_ref(&target),
            &[resolved_ref(&head)],
            Some(true),
            LockEvidence::before_checks(Some(&clean)),
            Some(&head),
            &[],
        );
        assert_eq!(
            comparison.cargo_audit_lock_proof(),
            CargoAuditLockProof::Unproven(LockProofGap::NoTargetLock)
        );
    }

    /// The local dirty check asks about the lockfile the audit read — the
    /// member's — and nothing else.
    #[test]
    fn the_local_dirty_check_asks_about_the_audited_lock() {
        use LockProofEntry::Blob;
        let files = || {
            [
                ("crates/core/Cargo.toml", Blob(CORE_MANIFEST)),
                ("crates/core/Cargo.lock", Blob(CORE_LOCK)),
                ("Cargo.lock", Blob(CORE_LOCK)),
            ]
        };
        let (tmp, head, target) = lock_proof_repo(&files(), &files());
        let config = member_config(&tmp);
        let local = |dirty: &[&str]| {
            let dirty: std::collections::BTreeSet<String> =
                dirty.iter().map(|path| path.to_string()).collect();
            CleanComparison::resolve(
                &config,
                &resolved_ref(&head),
                &[resolved_ref(&target)],
                Some(dirty.is_empty()),
                LockEvidence::before_checks(Some(&dirty)),
                Some(&head),
                &[],
            )
            .cargo_audit_lock_proof()
        };
        assert_eq!(
            local(&["crates/core/Cargo.lock"]),
            CargoAuditLockProof::Unproven(LockProofGap::DirtyLock)
        );
        assert_eq!(
            local(&["Cargo.lock"]),
            CargoAuditLockProof::TargetLock,
            "a root lock the member's audit never reads cannot revoke its proof"
        );
    }

    /// P1: a snapshot is materialised at the target commit, but cargo does not
    /// keep it that way. A target that adds a dependency without regenerating
    /// `Cargo.lock` has the lock rewritten by the first cargo command (none
    /// passes `--locked`), the audit reads the rewritten lock, and the committed
    /// lock the classification compares is untouched — so an advisory the
    /// manifest change pulled in reads as pre-existing. The boundary
    /// observations of the shared snapshot are what shows the rewrite.
    #[test]
    fn a_lock_rewritten_in_the_snapshot_withholds_the_proof() {
        let (tmp, _repo, first, target) = comparison_repo_with_lock();
        let config = crate::config::test_config_builder()
            .repo_root(tmp.path())
            .build();
        let clean: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        let target = target.to_string();
        let proof = |integrity: Option<&super::super::signal::SnapshotIntegrity>| {
            CleanComparison::resolve(
                &config,
                &resolved_ref(&target),
                &[resolved_ref(&first.to_string())],
                Some(true),
                LockEvidence {
                    dirty_before_checks: Some(&clean),
                    snapshot_integrity: integrity,
                    snapshot_root: Some(tmp.path()),
                    ..LockEvidence::default()
                },
                Some(&first.to_string()),
                &[],
            )
            .cargo_audit_lock_proof()
        };

        assert_eq!(
            proof(Some(&snapshot_observed(&target, &["Cargo.lock"]))),
            CargoAuditLockProof::Unproven(LockProofGap::DirtyLock),
            "the audit read a lock a check rewrote, not the committed one"
        );
        assert_eq!(
            proof(Some(&snapshot_observed(&target, &["src/generated.rs"]))),
            CargoAuditLockProof::TargetLock,
            "a rewrite of some other tracked file says nothing about the lock"
        );
        let unreadable = super::super::signal::SnapshotIntegrity::from_observations(
            crate::checks::snapshot_integrity::SnapshotObservation {
                expected_target_sha: target.clone(),
                observed_head_sha: None,
                status: crate::checks::snapshot_integrity::SnapshotIntegrityStatus::Unknown,
                changed_paths: None,
                error: Some("open shared snapshot: gone".to_string()),
                phase: "after-checks",
                check_name: None,
            },
            Vec::new(),
        );
        assert_eq!(
            proof(Some(&unreadable)),
            CargoAuditLockProof::Unproven(LockProofGap::UnknownProvenance),
            "an unreadable boundary establishes nothing"
        );
        assert_eq!(
            proof(None),
            CargoAuditLockProof::Unproven(LockProofGap::UnknownProvenance),
            "a snapshot nobody observed establishes nothing"
        );

        // The member's audit reads the member's lock, so only a rewrite of THAT
        // path revokes its proof.
        use LockProofEntry::Blob;
        let files = || {
            [
                ("crates/core/Cargo.toml", Blob(CORE_MANIFEST)),
                ("crates/core/Cargo.lock", Blob(CORE_LOCK)),
                ("Cargo.lock", Blob(CORE_LOCK)),
            ]
        };
        let (member_tmp, head, member_target) = lock_proof_repo(&files(), &files());
        let member = member_config(&member_tmp);
        let member_proof = |changed: &[&str]| {
            let observed = snapshot_observed(&member_target, changed);
            CleanComparison::resolve(
                &member,
                &resolved_ref(&member_target),
                &[resolved_ref(&head)],
                Some(true),
                LockEvidence {
                    dirty_before_checks: Some(&clean),
                    snapshot_integrity: Some(&observed),
                    snapshot_root: Some(member_tmp.path()),
                    ..LockEvidence::default()
                },
                Some(&head),
                &[],
            )
            .cargo_audit_lock_proof()
        };
        assert_eq!(
            member_proof(&["crates/core/Cargo.lock"]),
            CargoAuditLockProof::Unproven(LockProofGap::DirtyLock)
        );
        assert_eq!(
            member_proof(&["Cargo.lock"]),
            CargoAuditLockProof::TargetLock,
            "a root lock the member's audit never reads cannot revoke its proof"
        );
    }

    /// The local half of the same P1: the dirty set is frozen before the checks
    /// (R4-19), so a lock cargo rewrites mid-run is invisible to it. The audited
    /// lock is read again after the checks — that one tracked file only, so the
    /// untracked output R4-19 guards against still cannot revoke the proof.
    #[test]
    fn a_lock_rewritten_in_the_local_checkout_withholds_the_proof() {
        let (tmp, _repo, first, target) = comparison_repo_with_lock();
        let config = crate::config::test_config_builder()
            .repo_root(tmp.path())
            .build();
        let clean: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        let proof = || {
            CleanComparison::resolve(
                &config,
                &resolved_ref(&first.to_string()),
                &[resolved_ref(&target.to_string())],
                Some(true),
                LockEvidence::before_checks(Some(&clean)),
                Some(&first.to_string()),
                &[],
            )
            .cargo_audit_lock_proof()
        };
        let committed = std::fs::read_to_string(tmp.path().join("Cargo.lock")).unwrap();

        std::fs::create_dir_all(tmp.path().join("target")).unwrap();
        std::fs::write(tmp.path().join("target/cache.bin"), "check output").unwrap();
        assert_eq!(
            proof(),
            CargoAuditLockProof::TargetLock,
            "untracked check output does not touch the lockfile"
        );

        std::fs::write(
            tmp.path().join("Cargo.lock"),
            format!("{committed}\n[[package]]\nname = \"vulnerable\"\nversion = \"0.1.0\"\n"),
        )
        .unwrap();
        assert_eq!(
            proof(),
            CargoAuditLockProof::Unproven(LockProofGap::DirtyLock),
            "a lock rewritten after the dirty set was frozen is not the committed one"
        );

        std::fs::write(tmp.path().join("Cargo.lock"), committed).unwrap();
        assert_eq!(
            proof(),
            CargoAuditLockProof::TargetLock,
            "the committed bytes restored are the committed lock again"
        );
    }

    #[test]
    fn local_target_downgrades_all_baseline_signals_when_clean() {
        // head == target: the local checkout IS the target for every check, so a
        // clean worktree downgrades any baseline-signal out-of-diff finding.
        let clean = CleanComparison::for_test(true, true);
        for id in ["semgrep_scan", "rustfmt", "ruff", "eslint"] {
            assert!(
                clean.applies_to(id),
                "{id} should downgrade on a clean local target"
            );
        }
        let dirty = CleanComparison::for_test(true, false);
        for id in ["semgrep_scan", "rustfmt", "ruff", "eslint"] {
            assert!(
                !dirty.applies_to(id),
                "{id} must not downgrade on a dirty local target"
            );
        }
    }

    fn config_diff(path: &str) -> crate::git::Diff {
        crate::git::Diff {
            base: "main".to_string(),
            target: "feature".to_string(),
            base_commit_id: "base".to_string(),
            target_commit_id: "target".to_string(),
            files: vec![crate::git::FileChange {
                path: path.to_string(),
                status: crate::git::FileStatus::Modified,
                additions: 1,
                deletions: 0,
            }],
            stats: crate::git::DiffStats {
                files_changed: 1,
                additions: 1,
                deletions: 0,
                copied: 0,
            },
            commits: vec![],
        }
    }

    #[test]
    fn config_file_owner_maps_known_config_basenames() {
        assert_eq!(config_file_owner("rustfmt.toml"), Some("rustfmt"));
        assert_eq!(config_file_owner(".rustfmt.toml"), Some("rustfmt"));
        assert_eq!(config_file_owner("pyproject.toml"), Some("ruff"));
        assert_eq!(config_file_owner("ruff.toml"), Some("ruff"));
        assert_eq!(config_file_owner(".eslintrc.json"), Some("eslint"));
        assert_eq!(config_file_owner("eslint.config.mjs"), Some("eslint"));
        assert_eq!(config_file_owner(".stylelintrc"), Some("stylelint"));
        assert_eq!(config_file_owner(".prettierrc"), Some("prettier"));
        assert_eq!(config_file_owner("semgrep.yml"), Some("semgrep_scan"));
        // Cargo.toml is intentionally unmapped: it configures whole-project
        // gates (clippy/rustc lints), which are never eligible for the downgrade.
        assert_eq!(config_file_owner("Cargo.toml"), None);
        assert_eq!(config_file_owner("src/main.rs"), None);
    }

    #[test]
    fn changed_tool_config_owners_matches_nested_config_path() {
        // A config file anywhere in the tree is matched by basename.
        let diffs = [config_diff("crates/foo/rustfmt.toml")];
        let owners = changed_tool_config_owners(&diffs);
        assert!(owners.contains("rustfmt"));
        assert_eq!(owners.len(), 1);
        // A plain source change owns no config.
        assert!(changed_tool_config_owners(&[config_diff("src/lib.rs")]).is_empty());
    }

    #[test]
    fn changed_tool_config_suppresses_only_that_tools_downgrade() {
        // R5-21: rustfmt.toml is in the diff, so a stricter format rule may flag
        // unchanged files — rustfmt's out-of-diff findings must NOT be downgraded.
        let clean = CleanComparison::for_test_config_changed(&["rustfmt"]);
        assert!(
            !clean.applies_to("rustfmt"),
            "a changed rustfmt config suppresses the rustfmt downgrade"
        );
        // Tools whose config did not change keep the downgrade.
        assert!(clean.applies_to("ruff"));
        assert!(clean.applies_to("semgrep_scan"));

        // End-to-end: the out-of-diff rustfmt warning stays Unclassified.
        let findings = [out_of_diff_finding("rustfmt")];
        let summary = build_quality_failure_summary(
            &[warning_check("Rustfmt")],
            &findings,
            &CleanComparison::for_test_config_changed(&["rustfmt"]),
        );
        assert!(summary.preexisting_quality_failures.is_empty());
        assert_eq!(summary.unclassified_quality_failures, vec!["Rustfmt"]);
        // Deliberately updated with the origin split (re-adjudicates R2-13).
        // What R5-21 protects is the CLASSIFICATION: a changed rustfmt config
        // means the out-of-diff rows cannot be proven pre-existing, so they stay
        // Unclassified and keep their review weight through the policy engine
        // (Warnings → Advisory → ReviewRequired → CONDITIONAL). It never
        // protected calling a formatter warning a *failed quality check* —
        // Rustfmt reported `Warnings`, not `Failed`. The suppression is intact
        // above; only the failure claim is gone.
        assert!(
            !summary.has_new_failures(),
            "an unclassified WARNING is still not a failed quality check"
        );

        // The same shape from a check that genuinely failed still gates.
        let failed_summary = build_quality_failure_summary(
            &[failed_check("Rustfmt")],
            &findings,
            &CleanComparison::for_test_config_changed(&["rustfmt"]),
        );
        assert!(
            failed_summary.has_new_failures(),
            "a failed check with the same unclassified rows still fails the gate"
        );
    }

    #[test]
    fn unlocated_warning_is_not_a_failed_quality_check() {
        // P0 "warning→failure": a baseline-signal check that reports `Warnings`
        // without producing a single locatable finding (cargo audit raising an
        // unmaintained-crate advisory) classifies as Unclassified — there is
        // nothing to place inside or outside the diff. It must NOT make
        // `quality_pass` false, and the gate text must not say "failed".
        let summary = build_quality_failure_summary(
            &[warning_check("Cargo audit")],
            &[],
            &CleanComparison::for_test(true, true),
        );
        assert_eq!(summary.unclassified_quality_failures, vec!["Cargo audit"]);
        assert!(
            !summary.has_new_failures(),
            "a warning that produced no finding is not a new failure"
        );

        let reason = quality_failure_reason_text(&summary.quality_failures, &summary.details)
            .expect("warning signals are still described");
        assert!(
            !reason.contains("failed"),
            "warning-only reason must not use the word 'failed': {reason}"
        );
        assert_eq!(reason, "1 warning signal: 1 unclassified");
    }

    #[test]
    fn unlocated_failure_still_fails_the_gate() {
        // Fail-closed control for the test above: the SAME unlocated shape from
        // a check that actually failed keeps gating.
        let summary = build_quality_failure_summary(
            &[failed_check("Cargo audit")],
            &[],
            &CleanComparison::for_test(true, true),
        );
        assert!(summary.has_new_failures());
        assert_eq!(
            quality_failure_reason_text(&summary.quality_failures, &summary.details).as_deref(),
            Some("1 quality check failed (1 unclassified)")
        );
    }

    #[test]
    fn in_diff_warning_is_introduced_but_still_not_a_failure() {
        // An introduced warning keeps its classification (and its review weight
        // through the policy engine) but is still not a failed quality check.
        let findings = [in_diff_finding("rustfmt")];
        let summary = build_quality_failure_summary(
            &[warning_check("Rustfmt")],
            &findings,
            &CleanComparison::for_test(true, true),
        );
        assert_eq!(summary.introduced_quality_failures, vec!["Rustfmt"]);
        assert!(!summary.has_new_failures());
        assert_eq!(
            quality_failure_reason_text(&summary.quality_failures, &summary.details).as_deref(),
            Some("1 warning signal: 1 introduced")
        );
    }

    #[test]
    fn failure_and_warning_get_separate_sentences() {
        let findings = [
            in_diff_finding("cargo_test"),
            out_of_diff_finding("rustfmt"),
        ];
        let summary = build_quality_failure_summary(
            &[failed_check("cargo test"), warning_check("Rustfmt")],
            &findings,
            &CleanComparison::for_test(true, true),
        );
        assert!(summary.has_new_failures());
        assert_eq!(
            quality_failure_reason_text(&summary.quality_failures, &summary.details).as_deref(),
            Some("1 quality check failed (1 introduced); 1 warning signal: 1 pre-existing")
        );
    }

    #[test]
    fn warning_only_summary_is_allow_with_review_not_hold() {
        // Blast radius of the origin split on the hero label: with no real
        // failure left, a warnings-only run is "mergeable with advisories".
        let view = build_merge_decision_view(
            true,  // policy_allow_merge
            true,  // quality_pass (no longer broken by the warning)
            false, // recommended_merge — policy still says review required
            &["Cargo audit".to_string()],
            &[QualityFailureDetail {
                name: "Cargo audit".to_string(),
                classification: QualityFailureClass::Unclassified,
                origin: QualityFailureOrigin::Warning,
            }],
            &[],
            vec!["Cargo audit note: 1 informational advisory".to_string()],
        );
        assert_eq!(view.state, MergeDecisionState::AllowWithReview);
        assert!(
            !view.reason.contains("failed"),
            "decision reason must not claim a failure: {}",
            view.reason
        );
    }

    #[test]
    fn without_config_change_downgrade_still_fires() {
        // R5-21 control: no config file in the diff, so the rustfmt out-of-diff
        // downgrade to pre-existing works exactly as before.
        let findings = [out_of_diff_finding("rustfmt")];
        let summary = build_quality_failure_summary(
            &[warning_check("Rustfmt")],
            &findings,
            &CleanComparison::for_test(true, true),
        );
        assert_eq!(summary.preexisting_quality_failures, vec!["Rustfmt"]);
        assert!(!summary.has_new_failures());
    }

    fn resolved_ref(commit_id: &str) -> crate::git::ResolvedRef {
        crate::git::ResolvedRef {
            name: commit_id.to_string(),
            commit_id: commit_id.to_string(),
            is_remote: false,
        }
    }

    /// A two-commit repo whose tree carries a real, committed `Cargo.lock`.
    ///
    /// The lockfile is not decoration: cargo audit's provenance proof asks the
    /// target COMMIT whether a lockfile exists there, because a run in a
    /// lock-less tree audits one `cargo audit` generated from the registry. A
    /// fixture without the file can only assert about a world the proof refuses.
    fn comparison_repo_with_lock() -> (tempfile::TempDir, git2::Repository, git2::Oid, git2::Oid) {
        comparison_repo_inner(true)
    }

    fn comparison_repo() -> (tempfile::TempDir, git2::Repository, git2::Oid, git2::Oid) {
        comparison_repo_inner(false)
    }

    fn comparison_repo_inner(
        with_lock: bool,
    ) -> (tempfile::TempDir, git2::Repository, git2::Oid, git2::Oid) {
        let tmp = tempfile::tempdir().unwrap();
        let repo = git2::Repository::init(tmp.path()).unwrap();
        let signature = git2::Signature::now("Test", "test@example.com").unwrap();
        if with_lock {
            std::fs::write(
                tmp.path().join("Cargo.lock"),
                "version = 3\n\n[[package]]\nname = \"demo\"\nversion = \"1.2.3\"\n",
            )
            .unwrap();
            let mut index = repo.index().unwrap();
            index.add_path(std::path::Path::new("Cargo.lock")).unwrap();
            index.write().unwrap();
        }
        let tree_id = repo.index().unwrap().write_tree().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        let first = repo
            .commit(Some("HEAD"), &signature, &signature, "operator", &tree, &[])
            .unwrap();
        let parent = repo.find_commit(first).unwrap();
        let target = repo
            .commit(None, &signature, &signature, "target", &tree, &[&parent])
            .unwrap();
        drop(parent);
        drop(tree);
        (tmp, repo, first, target)
    }

    #[test]
    fn clean_comparison_rejects_a_late_checkout_to_the_target() {
        let (tmp, repo, first, target) = comparison_repo();
        let captured = capture_worktree_provenance(tmp.path());
        assert_eq!(captured.head_sha, Some(first.to_string()));
        assert_eq!(captured.clean, Some(true));
        let config = crate::config::test_config_builder()
            .repo_root(tmp.path())
            .build();
        let stable = CleanComparison::resolve(
            &config,
            &resolved_ref(&target.to_string()),
            &[resolved_ref(&first.to_string())],
            captured.clean,
            LockEvidence::before_checks(captured.dirty_paths.as_ref()),
            captured.head_sha.as_deref(),
            &[],
        );
        assert!(stable.applies_to("semgrep_scan"));
        assert!(!stable.applies_to("rustfmt"));
        repo.set_head_detached(target).unwrap();
        let comparison = CleanComparison::resolve(
            &config,
            &resolved_ref(&target.to_string()),
            &[resolved_ref(&first.to_string())],
            captured.clean,
            LockEvidence::before_checks(captured.dirty_paths.as_ref()),
            captured.head_sha.as_deref(),
            &[],
        );
        assert!(
            !comparison.applies_to("rustfmt"),
            "a late checkout cannot change the source of earlier findings"
        );
        let summary = build_quality_failure_summary(
            &[failed_check("Rustfmt")],
            &[out_of_diff_finding("rustfmt")],
            &comparison,
        );
        assert!(summary.preexisting_quality_failures.is_empty());
        assert_eq!(summary.unclassified_quality_failures, vec!["Rustfmt"]);
        assert!(summary.has_new_failures());
        assert!(
            !comparison.applies_to("semgrep_scan"),
            "a moved HEAD disables the downgrade for the run"
        );
    }

    #[test]
    fn clean_comparison_invalidates_a_target_checkout_that_moved() {
        let (tmp, repo, first, other) = comparison_repo();
        let captured = capture_worktree_provenance(tmp.path());
        let config = crate::config::test_config_builder()
            .repo_root(tmp.path())
            .build();
        let resolve = || {
            CleanComparison::resolve(
                &config,
                &resolved_ref(&first.to_string()),
                &[resolved_ref(&other.to_string())],
                captured.clean,
                LockEvidence::before_checks(captured.dirty_paths.as_ref()),
                captured.head_sha.as_deref(),
                &[],
            )
        };
        // Check output must not replace the status observed before checks.
        fs::write(tmp.path().join("check-output.txt"), "generated").unwrap();
        assert!(resolve().applies_to("rustfmt"));
        repo.set_head_detached(other).unwrap();
        assert!(!resolve().applies_to("rustfmt"));
        repo.set_head("refs/heads/missing").unwrap();
        assert!(
            !resolve().applies_to("rustfmt"),
            "unreadable live HEAD cannot certify stability"
        );
    }

    #[test]
    fn clean_comparison_never_infers_a_missing_captured_head() {
        let (tmp, _repo, first, other) = comparison_repo();
        let config = crate::config::test_config_builder()
            .repo_root(tmp.path())
            .build();
        let comparison = CleanComparison::resolve(
            &config,
            &resolved_ref(&first.to_string()),
            &[resolved_ref(&other.to_string())],
            Some(true),
            LockEvidence::before_checks(Some(&std::collections::BTreeSet::new())),
            None,
            &[],
        );
        for check in ["rustfmt", "semgrep_scan", "ruff"] {
            assert!(
                !comparison.applies_to(check),
                "{check}: current HEAD cannot fill an unknown captured HEAD"
            );
        }
    }

    #[test]
    fn no_resolvable_base_diff_when_bases_empty_or_equal_to_target() {
        // R4-20: an empty base set (a repo whose configured trunk never resolves)
        // and a base whose commit IS the target both yield no diff baseline.
        let target = resolved_ref("aaa111");
        assert!(
            !has_resolvable_base_diff(&target, &[]),
            "no base resolved → no baseline"
        );
        assert!(
            !has_resolvable_base_diff(&target, &[resolved_ref("aaa111")]),
            "base == target → no baseline"
        );
        assert!(
            has_resolvable_base_diff(&target, &[resolved_ref("bbb222")]),
            "a base distinct from the target IS a real baseline"
        );
    }

    #[test]
    fn no_base_diff_blocks_downgrade_even_on_clean_local_checkout() {
        // R4-20: without a resolved base different from the target the full scan
        // has no diff to predate, so a clean local checkout must NOT downgrade —
        // otherwise a baseless run would PASS on unproven "out-of-diff" rows.
        let clean = CleanComparison::for_test_no_base_diff();
        for id in ["semgrep_scan", "rustfmt", "ruff", "eslint"] {
            assert!(
                !clean.applies_to(id),
                "{id} must not downgrade without a diff baseline"
            );
        }

        let findings = [out_of_diff_finding("semgrep_scan")];
        let summary = build_quality_failure_summary(
            &[failed_check("Semgrep scan")],
            &findings,
            &CleanComparison::for_test_no_base_diff(),
        );
        assert!(
            summary.preexisting_quality_failures.is_empty(),
            "no baseline means nothing can be proven pre-existing"
        );
        assert_eq!(summary.unclassified_quality_failures, vec!["Semgrep scan"]);
        assert!(summary.has_new_failures());
    }

    #[test]
    fn current_only_never_downgrades_to_preexisting() {
        // R3-14: `--current-only` drops the diff bases, so every full-scan finding
        // is trivially "out of diff". Without a baseline nothing can be proven
        // pre-existing, so a failed semgrep must stay a new failure that gates —
        // not a silent PASS-downgrade.
        let clean = CleanComparison::for_test_current_only();
        assert!(
            !clean.applies_to("semgrep_scan"),
            "current-only must never downgrade even a baseline signal"
        );
        assert!(!clean.applies_to("rustfmt"));

        let findings = [out_of_diff_finding("semgrep_scan")];
        let summary = build_quality_failure_summary(
            &[failed_check("Semgrep scan")],
            &findings,
            &CleanComparison::for_test_current_only(),
        );
        assert!(
            summary.preexisting_quality_failures.is_empty(),
            "current-only has no baseline to prove a finding pre-existing"
        );
        assert_eq!(summary.unclassified_quality_failures, vec!["Semgrep scan"]);
        assert!(summary.has_new_failures());
    }

    #[test]
    fn introduced_findings_stay_introduced_for_any_check() {
        let findings = [in_diff_finding("cargo_test")];
        assert_eq!(
            classify_quality_failure("cargo_test", &findings, true),
            QualityFailureClass::Introduced
        );
    }

    #[test]
    fn capture_worktree_provenance_rejects_a_head_change_during_capture() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = git2::Repository::init(tmp.path()).unwrap();
        let signature = git2::Signature::now("Test", "test@example.com").unwrap();
        let tree_id = repo.index().unwrap().write_tree().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        let first = repo
            .commit(Some("HEAD"), &signature, &signature, "first", &tree, &[])
            .unwrap();
        let parent = repo.find_commit(first).unwrap();
        let second = repo
            .commit(None, &signature, &signature, "second", &tree, &[&parent])
            .unwrap();

        let capture = capture_worktree_provenance_inner(tmp.path(), || {
            repo.set_head_detached(second).unwrap();
        });
        assert_eq!(repo.head().unwrap().target(), Some(second));
        assert!(
            capture.head_sha.is_none(),
            "a moving HEAD has no coherent capture"
        );
        assert!(
            capture.clean.is_none(),
            "do not certify a raced checkout clean"
        );
        assert!(
            capture.status_digest.is_none(),
            "discard the mixed observation"
        );

        let stable = capture_worktree_provenance(tmp.path());
        assert_eq!(stable.head_sha, Some(second.to_string()));
        assert_eq!(stable.clean, Some(true));
        assert!(stable.status_digest.is_some());
    }

    #[test]
    fn capture_worktree_provenance_reflects_tree_at_capture_time() {
        // R4-19: cleanliness is read from the live tree, so capturing BEFORE
        // tool output (clean) and AFTER it (an untracked artifact/cache) yield
        // different answers — the reason the value must be frozen up front
        // rather than re-read once artifacts 10/20/30 have been written.
        let tmp = tempfile::tempdir().expect("tempdir");
        git2::Repository::init(tmp.path()).expect("init repo");
        assert_eq!(
            capture_worktree_provenance(tmp.path()).clean,
            Some(true),
            "a freshly initialised repo has a clean tree"
        );

        // A file dropped after capture (an in-repo --output-dir or a check
        // cache) makes a *later* read dirty; the frozen early value must not.
        std::fs::write(tmp.path().join("prview-output.txt"), b"artifact").expect("write");
        assert_eq!(
            capture_worktree_provenance(tmp.path()).clean,
            Some(false),
            "an untracked file makes a fresh read dirty"
        );
    }

    /// Git names files in bytes. A name that is not UTF-8 used to render as one
    /// literal placeholder whose content lookup resolved to `absent`, so two
    /// runs dirtying different such names — or the same name with different
    /// bytes in it — claimed the same substrate.
    #[test]
    #[cfg(unix)]
    fn non_utf8_dirty_paths_are_told_apart() {
        use std::os::unix::ffi::OsStrExt;

        // Labels are told apart everywhere, including on filesystems that
        // refuse such names outright (APFS rejects them with EILSEQ).
        assert_ne!(
            status_path_label(b"bad-\xff.txt"),
            status_path_label(b"bad-\xfe.txt"),
            "two different unrepresentable names must not share one line",
        );
        assert_eq!(
            status_path_label(b"src/main.rs"),
            "src/main.rs",
            "a representable path is written as itself",
        );

        let digest_for = |name: &[u8], body: &[u8]| -> Option<String> {
            let tmp = tempfile::tempdir().expect("tempdir");
            git2::Repository::init(tmp.path()).expect("init repo");
            let path = tmp.path().join(std::ffi::OsStr::from_bytes(name));
            // The filesystem may reject the name (APFS enforces UTF-8); the
            // digest question only exists where it does not.
            std::fs::write(&path, body).ok()?;
            capture_worktree_provenance(tmp.path()).status_digest
        };

        let (Some(first), Some(second), Some(recontented)) = (
            digest_for(b"bad-\xff.txt", b"one"),
            digest_for(b"bad-\xfe.txt", b"one"),
            digest_for(b"bad-\xff.txt", b"two"),
        ) else {
            return;
        };
        assert_ne!(
            first, second,
            "two different unrepresentable names are two different substrates",
        );
        assert_ne!(
            first, recontented,
            "the same unrepresentable name with different content is a different substrate",
        );
    }

    /// A dirty symlink used to be fingerprinted by the *pathname* it points at,
    /// so everything the checks would actually read through it — the bytes at
    /// the far end — could change between two runs while the digest stayed
    /// identical. Two materially different substrates, one fingerprint: exactly
    /// the collision this digest exists to prevent.
    #[test]
    #[cfg(unix)]
    fn a_symlink_is_fingerprinted_by_what_it_reaches() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let target = tmp.path().join("target.txt");
        let link = tmp.path().join("link.txt");
        std::fs::write(&target, b"one").expect("write target");
        std::os::unix::fs::symlink(&target, &link).expect("symlink");

        let mut budget = FingerprintBudget::new(FINGERPRINT_BYTE_BUDGET);
        let before = content_fingerprint(&link, 0, &mut budget);
        std::fs::write(&target, b"two").expect("rewrite target");
        let after = content_fingerprint(&link, 0, &mut budget);
        assert_ne!(
            before, after,
            "content reached through the link is part of the substrate",
        );

        // The link's own identity still counts: a link of the same name
        // pointing somewhere else is a different tree even when the two targets
        // happen to hold the same bytes.
        let other = tmp.path().join("other.txt");
        std::fs::write(&other, b"two").expect("write other");
        std::fs::remove_file(&link).expect("drop link");
        std::os::unix::fs::symlink(&other, &link).expect("relink");
        assert_ne!(
            after,
            content_fingerprint(&link, 0, &mut budget),
            "a link retargeted at identical bytes is still a different link",
        );

        // A dangling link is a state of its own, and nothing is read for it.
        std::fs::remove_file(&other).expect("drop other");
        assert!(
            content_fingerprint(&link, 0, &mut budget).ends_with("absent"),
            "a link with nothing at the far end must say so",
        );
    }

    /// A symlink's fingerprint used to be the hash of the path it names, so
    /// the bytes a scan actually reads through it could change completely while
    /// the digest swore the substrate was identical.
    #[test]
    #[cfg(unix)]
    fn a_dirty_symlinks_content_is_part_of_the_substrate() {
        let outside = tempfile::tempdir().expect("target tempdir");
        let target = outside.path().join("payload.txt");
        let tmp = tempfile::tempdir().expect("repo tempdir");
        git2::Repository::init(tmp.path()).expect("init repo");

        std::fs::write(&target, b"one").expect("write target");
        std::os::unix::fs::symlink(&target, tmp.path().join("link")).expect("symlink");
        let first = capture_worktree_provenance(tmp.path())
            .status_digest
            .expect("digest");

        // Same link, same name, same length — only the bytes behind it differ.
        std::fs::write(&target, b"two").expect("rewrite target");
        let second = capture_worktree_provenance(tmp.path())
            .status_digest
            .expect("digest");

        assert_ne!(
            first, second,
            "the content reached through a dirty symlink is what the checks read",
        );
    }

    /// The digest is taken before any check runs, so its reading is the review's
    /// own latency. It used to be unbounded: one untracked dataset or vendored
    /// bundle in the dirty subset and prview hashed it whole before starting.
    #[test]
    fn fingerprinting_stops_reading_once_the_budget_is_spent() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let big = tmp.path().join("big.bin");
        std::fs::write(&big, vec![7u8; 4096]).expect("write big");

        // Budget below the file's size: it is described, not read.
        let mut spent = FingerprintBudget::new(1024);
        let described = content_fingerprint(&big, 0, &mut spent);
        assert!(
            described.starts_with("stat:4096:"),
            "an over-budget file is described from its metadata, got {described}",
        );
        assert_eq!(
            spent.remaining, 1024,
            "a refused read must leave the allowance intact",
        );

        // A small file after it still gets a real content hash — the refusal
        // bounds the reading, it does not blind the rest of the capture.
        let small = tmp.path().join("small.txt");
        std::fs::write(&small, b"hello").expect("write small");
        assert!(
            content_fingerprint(&small, 0, &mut spent).starts_with("blob:5:"),
            "the entries after an oversized one are still hashed",
        );

        // With room, the same file is hashed as before — no existing digest
        // changes because of the bound.
        let mut ample = FingerprintBudget::new(FINGERPRINT_BYTE_BUDGET);
        assert!(
            content_fingerprint(&big, 0, &mut ample).starts_with("blob:4096:"),
            "a file that fits the budget is still fingerprinted by content",
        );
        assert_eq!(
            ample.remaining,
            FINGERPRINT_BYTE_BUDGET - 4096,
            "a granted read must be charged to the allowance",
        );

        // `stat:` is not a constant marker: two oversized files of different
        // sizes stay distinguishable, where one "too big" token would have made
        // every large file equal to every other.
        let bigger = tmp.path().join("bigger.bin");
        std::fs::write(&bigger, vec![7u8; 8192]).expect("write bigger");
        assert_ne!(
            described,
            content_fingerprint(&bigger, 0, &mut spent),
            "two different oversized files must not collapse into one line",
        );
    }

    /// The budget is spent entry by entry, so which files get hashed and which
    /// get described depends on the order they are visited — and the digest of
    /// one unchanged tree must not depend on the order git happens to report.
    #[test]
    fn one_tree_digests_the_same_way_on_every_capture() {
        let tmp = tempfile::tempdir().expect("tempdir");
        git2::Repository::init(tmp.path()).expect("init repo");
        for name in ["a.bin", "b.bin", "c.bin", "d.bin"] {
            std::fs::write(tmp.path().join(name), vec![1u8; 4096]).expect("write");
        }

        let first = capture_worktree_provenance(tmp.path()).status_digest;
        let second = capture_worktree_provenance(tmp.path()).status_digest;
        assert!(first.is_some());
        assert_eq!(
            first, second,
            "the same tree must fingerprint identically on every capture",
        );
    }

    #[test]
    fn an_unreadable_worktree_status_is_never_certified_clean() {
        // A repository whose index cannot be parsed answers NOTHING about
        // cleanliness. Reporting `clean: true` there put a fact in
        // PROVENANCE.json that nobody established, and unlocked the pre-existing
        // downgrade on a tree that was never inspected.
        let tmp = tempfile::tempdir().expect("tempdir");
        git2::Repository::init(tmp.path()).expect("init repo");
        std::fs::write(tmp.path().join(".git/index"), b"definitely not an index")
            .expect("corrupt the index");

        let provenance = capture_worktree_provenance(tmp.path());
        assert_eq!(
            provenance.clean, None,
            "an unreadable status is unknown, not clean",
        );
        assert!(
            provenance.status_digest.is_none(),
            "no status was read, so there is nothing to fingerprint",
        );

        let unknown = CleanComparison {
            target_is_checkout: Some(true),
            worktree_clean: None,
            current_only: false,
            has_base_diff: true,
            configs_changed: std::collections::BTreeSet::new(),
            cargo_audit_lock: CargoAuditLockProof::Unproven(LockProofGap::UnknownProvenance),
        };
        assert!(
            !unknown.applies_to("clippy"),
            "an unverified tree must not downgrade out-of-diff failures to pre-existing",
        );
    }

    #[test]
    fn frozen_clean_value_keeps_downgrade_after_later_writes() {
        // R4-19: the downgrade uses the cleanliness frozen before the run, not a
        // live re-read. Captured clean, the local-target downgrade stays enabled
        // even though tool output later dirties the tree — whereas a late read
        // would have seen the untracked artifact and suppressed it.
        let frozen_clean = CleanComparison::for_test(true, true);
        assert!(frozen_clean.applies_to("rustfmt"));
        assert!(frozen_clean.applies_to("semgrep_scan"));

        let read_late_dirty = CleanComparison::for_test(true, false);
        assert!(
            !read_late_dirty.applies_to("rustfmt"),
            "a late dirty read would wrongly kill the downgrade"
        );
    }
}
