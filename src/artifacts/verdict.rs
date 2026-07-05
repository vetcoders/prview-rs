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

#[derive(Debug, Clone)]
pub(crate) struct QualityFailureDetail {
    pub name: String,
    pub classification: QualityFailureClass,
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
    /// Returns true when there are failures that are new or indeterminate.
    ///
    /// Purely pre-existing failures do NOT count — they existed before this
    /// diff and should not block the gate.  Introduced, mixed, and
    /// unclassified failures are all considered "new" because they either
    /// definitely or possibly originate from the current change.
    pub(crate) fn has_new_failures(&self) -> bool {
        !self.introduced_quality_failures.is_empty()
            || !self.mixed_quality_failures.is_empty()
            || !self.unclassified_quality_failures.is_empty()
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

    if coverage.total_source > 0 && coverage.pct < 80 {
        let mut coverage_caveat = format!("{}% coverage heuristic", coverage.pct);
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

pub(crate) fn build_merge_decision_view(
    policy_allow_merge: bool,
    quality_pass: bool,
    recommended_merge: bool,
    quality_failures: &[String],
    quality_failure_details: &[QualityFailureDetail],
    blocking_issues: &[String],
    review_caveats: Vec<String>,
) -> MergeDecisionView {
    let has_new_quality_failures = quality_failure_details
        .iter()
        .any(|detail| !matches!(detail.classification, QualityFailureClass::Preexisting))
        || (quality_failure_details.is_empty() && !quality_failures.is_empty());

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
/// On any inability to inspect the repo we default to the permissive "clean local
/// checkout" shape, preserving the historical downgrade behaviour rather than
/// distrusting a repo we cannot read.
#[derive(Debug, Clone, Copy)]
pub(crate) struct CleanComparison {
    /// `head == target`: the local working tree IS the analysed target, so every
    /// check scanned the target directly.
    target_is_checkout: bool,
    /// When the local checkout is the target, whether it is free of staged,
    /// unstaged, and untracked changes.
    worktree_clean: bool,
}

impl CleanComparison {
    pub(crate) fn resolve(config: &Config, resolved_target: &crate::git::ResolvedRef) -> Self {
        let head = crate::git::Repository::open(&config.repo_root)
            .ok()
            .and_then(|repo| repo.head_commit_id().ok());
        match head {
            Some(head) => CleanComparison {
                target_is_checkout: head == resolved_target.commit_id,
                worktree_clean: !worktree_has_uncommitted_changes(&config.repo_root),
            },
            // Repo unreadable: preserve the historical downgrade by treating it as
            // a clean local checkout.
            None => CleanComparison {
                target_is_checkout: true,
                worktree_clean: true,
            },
        }
    }

    /// Whether the pre-existing downgrade may fire for `check_id`'s findings.
    pub(crate) fn applies_to(&self, check_id: &str) -> bool {
        if self.target_is_checkout {
            // Local checkout is the target for every check; only a clean tree can
            // be trusted (R2-9).
            self.worktree_clean
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
            worktree_clean,
        }
    }
}

/// Whether a check materialises and scans an ephemeral snapshot of the analysed
/// *target* when that target is a fetched remote ref not checked out locally.
///
/// Only `semgrep_scan` does this (R2-10): it builds a detached worktree at the
/// target commit and scans that clean tree, so its out-of-diff findings genuinely
/// predate the target diff. Every other baseline-signal check scans the local
/// checkout, which in a `--pr`/`--remote` run is a different tree than the target.
fn check_scans_target_snapshot(check_id: &str) -> bool {
    matches!(check_id, "semgrep_scan")
}

/// True when the working tree at `repo_root` has staged, unstaged, or untracked
/// changes. Errors resolve to `false` (treat as clean) to preserve the existing
/// downgrade behaviour when the repo cannot be inspected.
fn worktree_has_uncommitted_changes(repo_root: &std::path::Path) -> bool {
    let Ok(repo) = git2::Repository::discover(repo_root) else {
        return false;
    };
    let mut opts = git2::StatusOptions::new();
    opts.include_untracked(true)
        .recurse_untracked_dirs(true)
        .renames_head_to_index(true);
    repo.statuses(Some(&mut opts))
        .map(|statuses| !statuses.is_empty())
        .unwrap_or(false)
}

pub(crate) fn classify_quality_failure(
    check_id: &str,
    dashboard_findings: &[DashboardFinding],
    clean_comparison: bool,
) -> QualityFailureClass {
    let mut saw_in_diff = false;
    let mut saw_out_of_diff = false;

    for finding in dashboard_findings
        .iter()
        .filter(|finding| finding.check_id == check_id)
    {
        match finding.in_diff {
            Some(true) => saw_in_diff = true,
            Some(false) => saw_out_of_diff = true,
            None => {}
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
        (false, true) if clean_comparison && check_id_is_baseline_signal(check_id) => {
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
) {
    summary.quality_failures.push(name.clone());
    summary.details.push(QualityFailureDetail {
        name: name.clone(),
        classification,
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
        push_quality_failure(&mut summary, check.name.clone(), classification);
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
/// CONDITIONAL instead of PASS-with-caveat. An in-diff warning is classified
/// `Introduced` and keeps its review weight (no downgrade).
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

    let mut introduced = 0usize;
    let mut preexisting = 0usize;
    let mut mixed = 0usize;
    let mut unclassified = 0usize;

    for detail in quality_failure_details {
        match detail.classification {
            QualityFailureClass::Introduced => introduced += 1,
            QualityFailureClass::Preexisting => preexisting += 1,
            QualityFailureClass::Mixed => mixed += 1,
            QualityFailureClass::Unclassified => unclassified += 1,
        }
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

    if parts.is_empty() {
        return None;
    }

    Some(format!(
        "{} quality check{} failed ({})",
        quality_failures.len(),
        if quality_failures.len() == 1 { "" } else { "s" },
        parts.join(", ")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

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
        // R3-16: on a remote/snapshot target (head != target) only semgrep scanned
        // the target snapshot. rustfmt scanned the local checkout — a different
        // tree — so its out-of-diff rows must NOT be downgraded to pre-existing,
        // while semgrep's still are.
        let clean = CleanComparison::for_test(false, true);
        assert!(
            clean.applies_to("semgrep_scan"),
            "semgrep scans the target snapshot, downgrade applies"
        );
        assert!(
            !clean.applies_to("rustfmt"),
            "rustfmt scanned the local checkout, downgrade must not apply"
        );
        assert!(!clean.applies_to("ruff"));
        assert!(!clean.applies_to("eslint"));

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

    #[test]
    fn introduced_findings_stay_introduced_for_any_check() {
        let findings = [in_diff_finding("cargo_test")];
        assert_eq!(
            classify_quality_failure("cargo_test", &findings, true),
            QualityFailureClass::Introduced
        );
    }
}
