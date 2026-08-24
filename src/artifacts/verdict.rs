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
            MergeDecisionState::Allow | MergeDecisionState::AllowWithReview => "merge-allow",
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

pub(super) fn cargo_audit_baseline_review_caveats(inline: &InlineFindingsSummary) -> Vec<String> {
    inline
        .dashboard_findings
        .iter()
        .find(|finding| finding.check_id == "cargo_audit_baseline")
        .map(|finding| vec![finding.message.clone()])
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

/// Per-check clean-comparison signal: whether an all-out-of-diff location set for
/// a given check may be trusted as pre-existing debt and downgraded off the merge
/// gate.
///
/// The downgrade is only sound for a check whose findings came from the analysed
/// *target* tree. Two shapes qualify:
///
/// * **Local target** (`head == target`): every baseline-signal check scans the
///   working tree, which IS the target — provided the tree is clean. A dirty
///   worktree can make an uncommitted finding look out-of-diff (R2-9), so a dirty
///   local scan downgrades nothing.
/// * **Remote/snapshot target** (`head != target`): only semgrep materialises and
///   scans an ephemeral snapshot of the target (R2-10). Every other baseline
///   signal (rustfmt/ruff/eslint/…) still scans `config.repo_root` — the local
///   checkout, a *different* tree than the target — so its out-of-diff rows prove
///   nothing about the target diff and must NOT be downgraded (R3-16).
///
/// `--current-only` deliberately drops the diff bases to analyse the whole
/// current state, so there is no diff baseline a finding can "predate": the
/// downgrade must never fire regardless of tree shape (R3-14).
///
/// On any inability to inspect the repo we default to the permissive "local
/// checkout is the target" shape, preserving the historical downgrade behaviour
/// rather than distrusting a repo we cannot read. That default does NOT extend to
/// cleanliness: a tree whose status could not be read is unknown, not clean, and
/// an unknown tree never unlocks the downgrade.
#[derive(Debug, Clone)]
pub(crate) struct CleanComparison {
    /// `head == target`: the local working tree IS the analysed target, so every
    /// check scanned the target directly.
    target_is_checkout: bool,
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
        diffs: &[crate::git::Diff],
    ) -> Self {
        let has_base_diff = has_resolvable_base_diff(resolved_target, resolved_bases);
        let configs_changed = changed_tool_config_owners(diffs);
        let head = crate::git::Repository::open(&config.repo_root)
            .ok()
            .and_then(|repo| repo.head_commit_id().ok());
        match head {
            Some(head) => CleanComparison {
                target_is_checkout: head == resolved_target.commit_id,
                worktree_clean,
                current_only: config.current_only,
                has_base_diff,
                configs_changed,
            },
            // Repo unreadable: preserve the historical downgrade by treating the
            // target as the local checkout. Whether the downgrade actually fires
            // is then decided by `worktree_clean`, which is `Some(true)` only for
            // a tree whose status was really read.
            None => CleanComparison {
                target_is_checkout: true,
                worktree_clean,
                current_only: config.current_only,
                has_base_diff,
                configs_changed,
            },
        }
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
        if self.target_is_checkout {
            // Local checkout is the target for every check; only a tree PROVEN
            // clean can be trusted (R2-9). An unread status is not proof.
            self.worktree_clean == Some(true)
        } else {
            // Remote target: only checks that scanned the target snapshot qualify;
            // everything else scanned the local checkout, a different tree (R3-16).
            check_scans_target_snapshot(check_id)
        }
    }

    #[cfg(test)]
    pub(crate) fn for_test(target_is_checkout: bool, worktree_clean: bool) -> Self {
        CleanComparison {
            target_is_checkout,
            worktree_clean: Some(worktree_clean),
            current_only: false,
            has_base_diff: true,
            configs_changed: std::collections::BTreeSet::new(),
        }
    }

    #[cfg(test)]
    pub(crate) fn for_test_current_only() -> Self {
        CleanComparison {
            target_is_checkout: true,
            worktree_clean: Some(true),
            current_only: true,
            has_base_diff: true,
            configs_changed: std::collections::BTreeSet::new(),
        }
    }

    #[cfg(test)]
    pub(crate) fn for_test_no_base_diff() -> Self {
        CleanComparison {
            target_is_checkout: true,
            worktree_clean: Some(true),
            current_only: false,
            has_base_diff: false,
            configs_changed: std::collections::BTreeSet::new(),
        }
    }

    #[cfg(test)]
    pub(crate) fn for_test_config_changed(owners: &[&'static str]) -> Self {
        CleanComparison {
            target_is_checkout: true,
            worktree_clean: Some(true),
            current_only: false,
            has_base_diff: true,
            configs_changed: owners.iter().copied().collect(),
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
/// side effect of moving a check onto the reviewed substrate. Keeping them out
/// is the conservative side — findings surface instead of being suppressed.
fn check_scans_target_snapshot(check_id: &str) -> bool {
    matches!(check_id, "semgrep_scan" | "ruff" | "eslint" | "stylelint")
}

/// Working-tree state frozen at the start of a run: whether the tree was clean,
/// and a fingerprint of exactly what was dirty.
///
/// Both halves come from ONE status read, so the pack can never claim a clean
/// tree next to a digest of uncommitted changes.
#[derive(Debug, Clone, Default)]
pub struct WorktreeProvenance {
    /// No staged, unstaged, or untracked changes at capture time. `None` when
    /// the status could not be read — cleanliness unestablished, never assumed.
    pub clean: Option<bool>,
    /// `sha256:<hex>` over the canonical `XY <path>` rendering of the status
    /// PLUS the current bytes of every dirty path (see
    /// [`render_status_fingerprint`]). `None` when the repository could not be
    /// inspected — an unknown fingerprint stays visibly unknown.
    pub status_digest: Option<String>,
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
///   nobody inspected — it reaches `PROVENANCE.json.worktree.clean` as a fact
///   and lets `CleanComparison` downgrade out-of-diff failures to pre-existing.
///   That is the one direction this record exists to prevent, so it stays
///   `None`: unknown, and treated as untrusted.
pub(crate) fn capture_worktree_provenance(repo_root: &std::path::Path) -> WorktreeProvenance {
    use sha2::{Digest, Sha256};

    let Ok(repo) = git2::Repository::discover(repo_root) else {
        return WorktreeProvenance {
            clean: Some(true),
            status_digest: None,
        };
    };
    let mut opts = git2::StatusOptions::new();
    opts.include_untracked(true)
        .recurse_untracked_dirs(true)
        .renames_head_to_index(true);
    let Ok(statuses) = repo.statuses(Some(&mut opts)) else {
        return WorktreeProvenance {
            clean: None,
            status_digest: None,
        };
    };

    let workdir = repo.workdir().map(|dir| dir.to_path_buf());
    let mut budget = FingerprintBudget::new(FINGERPRINT_BYTE_BUDGET);
    let fingerprint = render_status_fingerprint(&statuses, workdir.as_deref(), 0, &mut budget);
    let mut hasher = Sha256::new();
    hasher.update(fingerprint.as_bytes());

    WorktreeProvenance {
        clean: Some(statuses.is_empty()),
        status_digest: Some(format!("sha256:{:x}", hasher.finalize())),
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
            level: "error",
            check_name: check_id.to_string(),
            check_id: check_id.to_string(),
            message: "finding".to_string(),
            in_diff: Some(false),
        }
    }

    fn in_diff_finding(check_id: &str) -> DashboardFinding {
        DashboardFinding {
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
        // rustfmt/cargo_audit scanned the local checkout — a different tree — so
        // their out-of-diff rows must NOT be downgraded to pre-existing.
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
            !clean.applies_to("cargo_audit"),
            "cargo_audit scanned the local checkout, downgrade must not apply"
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
            target_is_checkout: true,
            worktree_clean: None,
            current_only: false,
            has_base_diff: true,
            configs_changed: std::collections::BTreeSet::new(),
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
