use serde::{Deserialize, Serialize};

use crate::check_id::check_id_from_name;
use crate::checks::{CheckResult, CheckStatus, SkippedCheck};
use crate::config::Config;
use crate::policy::{GateClass, PolicySeverity};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolOutcome {
    Passed,
    FindingsFailed,
    FindingsWarning,
    SystemError,
    Skipped,
    Unavailable,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyConclusion {
    Satisfied,
    Advisory,
    Blocked,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnalysisStatus {
    Complete,
    Degraded,
    Incomplete,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MergeRecommendation {
    Approve,
    ReviewRequired,
    Block,
}

/// Typed reason class used by every process-exit adapter.
///
/// The merge verdict deliberately stays on the stable
/// `PASS`/`CONDITIONAL`/`BLOCK` vocabulary. A `CONDITIONAL` alone cannot say
/// whether the run only warned or whether a breaking/degraded fact requires
/// strict enforcement, so the artifact emitter records that distinction once
/// from typed facts and readers consume it without parsing prose caveats.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EnforcementDisposition {
    Clean,
    WarningsOnly,
    ReviewRequired,
    Block,
}

/// Invocation lane for the shared enforcement table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnforcementMode {
    /// Only a policy block is process-fatal.
    Advisory,
    /// Historical top-level `--ci`: block/quality failures are fatal, while a
    /// review-only conditional remains advisory.
    Ci,
    /// Historical CI plus the warnings-clean opt-in.
    CiFailOnWarnings,
    /// `prview gate --strict`: typed review requirements are fatal;
    /// warnings-only is accepted.
    GateStrict,
    /// Strict gate plus the warnings-clean opt-in.
    GateFailOnWarnings,
}

/// Result of looking up a disposition in the enforcement table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnforcementAction {
    Accept,
    Reject,
    Block,
}

impl EnforcementMode {
    pub const fn from_ci_flags(ci: bool, fail_on_warnings: bool) -> Self {
        if ci && fail_on_warnings {
            Self::CiFailOnWarnings
        } else if ci {
            Self::Ci
        } else {
            Self::Advisory
        }
    }

    pub const fn from_gate_flags(strict: bool, fail_on_warnings: bool) -> Self {
        if strict && fail_on_warnings {
            Self::GateFailOnWarnings
        } else if strict {
            Self::GateStrict
        } else {
            Self::Advisory
        }
    }

    pub const fn is_strict(self) -> bool {
        !matches!(self, Self::Advisory)
    }

    pub const fn fails_on_warnings(self) -> bool {
        matches!(self, Self::CiFailOnWarnings | Self::GateFailOnWarnings)
    }

    pub const fn is_ci(self) -> bool {
        matches!(self, Self::Ci | Self::CiFailOnWarnings)
    }
}

impl EnforcementDisposition {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Clean => "clean",
            Self::WarningsOnly => "warnings_only",
            Self::ReviewRequired => "review_required",
            Self::Block => "block",
        }
    }

    /// One monotonic enforcement table shared by gate and CI exit adapters.
    pub const fn action(self, mode: EnforcementMode) -> EnforcementAction {
        match (self, mode) {
            (Self::Block, _) => EnforcementAction::Block,
            (
                Self::ReviewRequired,
                EnforcementMode::GateStrict | EnforcementMode::GateFailOnWarnings,
            )
            | (
                Self::WarningsOnly,
                EnforcementMode::CiFailOnWarnings | EnforcementMode::GateFailOnWarnings,
            ) => EnforcementAction::Reject,
            _ => EnforcementAction::Accept,
        }
    }

    /// Ratchet to the more conservative typed disposition.
    pub fn raise_to(&mut self, other: Self) {
        *self = (*self).max(other);
    }

    /// Classify effective policy evaluations before artifact-only signals add
    /// their own typed ratchets. A plain warning is the only review-required
    /// evaluation that strict mode may accept; loss of confidence, an executed
    /// failure/error/skip, and a hard block remain enforceable.
    pub fn from_evaluations(evaluations: &[CheckEvaluation]) -> Self {
        evaluations
            .iter()
            .fold(Self::Clean, |mut disposition, eval| {
                let current = if eval.merge_impact == MergeRecommendation::Block {
                    Self::Block
                } else if eval.confidence_impact != AnalysisStatus::Complete {
                    Self::ReviewRequired
                } else if eval.outcome == ToolOutcome::FindingsWarning {
                    // The warning remains a real pack fact even when baseline or
                    // policy downgrading makes its effective merge impact Approve.
                    // This is what lets PASS-with-warnings stay PASS while the
                    // explicit warnings-clean lane still sees the warning.
                    Self::WarningsOnly
                } else if eval.merge_impact == MergeRecommendation::ReviewRequired {
                    Self::ReviewRequired
                } else {
                    Self::Clean
                };
                disposition.raise_to(current);
                disposition
            })
    }
}

impl MergeRecommendation {
    /// Legacy single-field verdict, unified to the machine vocabulary
    /// `PASS`/`CONDITIONAL`/`BLOCK` (PV-03/04). `CONDITIONAL` replaces the former
    /// `HOLD` synonym so there is one word for "review required / degraded" on
    /// every decision surface. Old `HOLD` runs are still tolerated by the MCP
    /// adapter and the schema validator during read-back.
    pub fn legacy_verdict(
        self,
        analysis_status: AnalysisStatus,
        quality_pass: bool,
    ) -> &'static str {
        match self {
            MergeRecommendation::Block => "BLOCK",
            MergeRecommendation::ReviewRequired => "CONDITIONAL",
            MergeRecommendation::Approve => {
                if analysis_status == AnalysisStatus::Complete && quality_pass {
                    "PASS"
                } else {
                    "CONDITIONAL"
                }
            }
        }
    }

    pub fn legacy_recommended_merge(self) -> bool {
        self == MergeRecommendation::Approve
    }

    pub fn machine_status(
        self,
        analysis_status: AnalysisStatus,
        quality_pass: bool,
    ) -> &'static str {
        if self == MergeRecommendation::Approve
            && analysis_status == AnalysisStatus::Complete
            && quality_pass
        {
            "ok"
        } else {
            "fail"
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckExecutionState {
    Executed,
    Skipped,
    Unavailable,
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckEvaluation {
    pub check_id: String,
    pub name: String,
    pub raw_status: String,
    pub execution_state: CheckExecutionState,
    pub gate_class: GateClass,
    pub severity: PolicySeverity,
    pub outcome: ToolOutcome,
    pub conclusion: PolicyConclusion,
    pub confidence_impact: AnalysisStatus,
    pub merge_impact: MergeRecommendation,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PolicyRunSummary {
    pub analysis_status: AnalysisStatus,
    pub merge_recommendation: MergeRecommendation,
    pub evaluations: Vec<CheckEvaluation>,
    pub blocking_issues: Vec<String>,
    pub review_caveats: Vec<String>,
}

pub struct PolicyEngine<'a> {
    config: &'a Config,
}

impl<'a> PolicyEngine<'a> {
    pub fn new(config: &'a Config) -> Self {
        Self { config }
    }

    pub fn evaluate_run(&self, result: &CheckResult) -> CheckEvaluation {
        let id = check_id_from_name(&result.name);
        let severity = self.config.policy.severity_for(&id);

        // A check that RAN but returned Skipped (e.g. a required tool that
        // spawned then failed and was downgraded to Skipped, or a degenerate
        // no-op) must route through the SAME policy logic as a pre-run skip.
        // Otherwise a check REQUIRED by policy that vanishes at runtime is
        // scored as a silent Satisfied/Approve and the gate goes falsely green
        // — the fail-open class at the core of the merge-gate doctrine
        // (PR #12 review #1).
        if result.status == CheckStatus::Skipped {
            let reason = result.output.to_lowercase();
            // Only a check that OWNS a test scope, and whose own provenance
            // agrees, may reach the empty-selection branch below.
            let empty_test_scope = proved_empty_test_scope(result, &reason);
            let execution_state = classify_skip_execution_state(&reason, empty_test_scope);
            let (conclusion, confidence_impact, merge_impact) =
                self.skip_policy_outcome(severity, &reason, empty_test_scope);
            return CheckEvaluation {
                check_id: id,
                name: result.name.clone(),
                raw_status: result.status.as_str().to_string(),
                execution_state,
                gate_class: GateClass::Skip,
                severity,
                outcome: skip_outcome_for(execution_state),
                conclusion,
                confidence_impact,
                merge_impact,
                reason: (!result.output.is_empty()).then(|| result.output.clone()),
            };
        }

        let outcome = match result.status {
            CheckStatus::Passed => ToolOutcome::Passed,
            CheckStatus::Failed => ToolOutcome::FindingsFailed,
            CheckStatus::Warnings => ToolOutcome::FindingsWarning,
            CheckStatus::Error => ToolOutcome::SystemError,
            CheckStatus::Skipped => ToolOutcome::Skipped,
        };

        let class = match result.status {
            CheckStatus::Passed => GateClass::Pass,
            CheckStatus::Failed | CheckStatus::Error => GateClass::Fail,
            CheckStatus::Warnings => GateClass::Info,
            CheckStatus::Skipped => GateClass::Skip,
        };
        let is_blocking = self.config.policy.is_blocking(severity, class);

        let conclusion = if is_blocking {
            PolicyConclusion::Blocked
        } else if class == GateClass::Fail || class == GateClass::Info {
            PolicyConclusion::Advisory
        } else {
            PolicyConclusion::Satisfied
        };

        let confidence_impact = if result.status == CheckStatus::Error {
            AnalysisStatus::Incomplete
        } else if semgrep_scan_is_degraded(&id, result) {
            // A semgrep scan that reports parse errors analysed the target only
            // partially, so its finding set is incomplete: an introduced finding
            // may hide in the spans semgrep could not parse. Mark the analysis
            // Degraded so a later pre-existing downgrade (which neutralises the
            // finding impact) cannot launder a partial scan into a clean PASS —
            // the completeness signal survives independently of the findings
            // (R5-24).
            AnalysisStatus::Degraded
        } else {
            AnalysisStatus::Complete
        };

        let merge_impact = if conclusion == PolicyConclusion::Blocked {
            MergeRecommendation::Block
        } else if conclusion == PolicyConclusion::Advisory {
            MergeRecommendation::ReviewRequired
        } else {
            MergeRecommendation::Approve
        };

        let has_hard_fails = result
            .provenance
            .as_ref()
            .is_some_and(|p| !p.hard_fail_signatures.is_empty());
        let final_merge_impact = if has_hard_fails && merge_impact != MergeRecommendation::Block {
            MergeRecommendation::ReviewRequired
        } else {
            merge_impact
        };

        CheckEvaluation {
            check_id: id,
            name: result.name.clone(),
            raw_status: result.status.as_str().to_string(),
            execution_state: CheckExecutionState::Executed,
            gate_class: class,
            severity,
            outcome,
            conclusion,
            confidence_impact,
            merge_impact: final_merge_impact,
            reason: None,
        }
    }

    pub fn evaluate_skip(&self, skipped: &SkippedCheck) -> CheckEvaluation {
        let id = check_id_from_name(&skipped.name);
        let reason = skipped.reason.to_lowercase();
        let severity = self.config.policy.severity_for(&id);
        // A pre-flight skip decided before anything ran: there is no execution
        // to have produced an empty selection, so the empty-selection branch is
        // not available here no matter what the reason says.
        let execution_state = classify_skip_execution_state(&reason, false);
        let (conclusion, confidence_impact, merge_impact) =
            self.skip_policy_outcome(severity, &reason, false);

        CheckEvaluation {
            check_id: id,
            name: skipped.name.clone(),
            raw_status: CheckStatus::Skipped.as_str().to_string(),
            execution_state,
            gate_class: GateClass::Skip,
            severity,
            outcome: skip_outcome_for(execution_state),
            conclusion,
            confidence_impact,
            merge_impact,
            reason: Some(skipped.reason.clone()),
        }
    }

    /// Policy outcome for a check that produced no usable signal (skipped —
    /// either pre-run via `evaluate_skip`, or downgraded at runtime via the
    /// Skipped branch of `evaluate_run`). Shared so a check REQUIRED by policy
    /// cannot silently pass the gate just because it vanished at runtime
    /// (fail-open). `reason` must already be lowercased.
    fn skip_policy_outcome(
        &self,
        severity: PolicySeverity,
        reason: &str,
        empty_test_scope: bool,
    ) -> (PolicyConclusion, AnalysisStatus, MergeRecommendation) {
        if reason.starts_with("profile") {
            // Profile mismatches (e.g. running a rust check on a JS repo) are
            // totally fine — the check simply does not apply here.
            (
                PolicyConclusion::Satisfied,
                AnalysisStatus::Complete,
                MergeRecommendation::Approve,
            )
        } else if empty_test_scope {
            // Sibling of the profile branch, and deliberately as narrow. The
            // check applies to this REPOSITORY but not to this CHANGE, and that
            // is not an assumption: the scope decision proved it, through a
            // classification in which anything unrecognised escalates to a full
            // run instead of landing here. There is no gap in the evidence to
            // report, because there was no test the change could have broken.
            //
            // The honesty of this branch rests on THREE facts checked together
            // by `proved_empty_test_scope`, not on the wording of the reason: a
            // check that owns a test scope, a skip decided by a real execution
            // (never a pre-flight one), and provenance that RECORDS the empty
            // selection or the narrowed run that found nothing. A lint that
            // prints this sentence, a check that leaves no such record, or a
            // test check that ran the FULL suite and then skipped, falls
            // through to the branches
            // below and still blocks where policy requires it. The row itself
            // reads `skipped` with `outcome: skipped` — a suite that never ran
            // is never relabelled `passed` (contract §2.4).
            (
                PolicyConclusion::Satisfied,
                AnalysisStatus::Complete,
                MergeRecommendation::Approve,
            )
        } else if severity == PolicySeverity::Block {
            if reason.contains("fast remote-only preset") && self.config.remote_only {
                // Preserve the existing fast remote-only contract: the check is
                // intentionally omitted by the preset, so the signal is
                // degraded and review-required rather than blocking.
                (
                    PolicyConclusion::Advisory,
                    AnalysisStatus::Degraded,
                    MergeRecommendation::ReviewRequired,
                )
            } else if is_mode_skip_reason(reason) {
                // Strictly required check skipped by the selected mode: the run
                // is incomplete, but this is a declared caveat rather than a
                // missing tool/runtime failure.
                (
                    PolicyConclusion::Advisory,
                    AnalysisStatus::Incomplete,
                    MergeRecommendation::ReviewRequired,
                )
            } else {
                // Required but skipped for any other reason (missing tool,
                // runtime spawn failure): the gate cannot be trusted, so block.
                (
                    PolicyConclusion::Blocked,
                    AnalysisStatus::Incomplete,
                    MergeRecommendation::Block,
                )
            }
        } else if severity == PolicySeverity::Warn {
            // "Warn" severity skipping means it's an optional extra layer.
            (
                PolicyConclusion::Advisory,
                AnalysisStatus::Degraded,
                MergeRecommendation::ReviewRequired,
            )
        } else {
            // "Ignore" severity skipping.
            (
                PolicyConclusion::Satisfied,
                AnalysisStatus::Complete,
                MergeRecommendation::Approve,
            )
        }
    }

    pub fn evaluate_all(
        &self,
        checks: &[CheckResult],
        skipped_checks: &[SkippedCheck],
    ) -> PolicyRunSummary {
        let mut evaluations = Vec::with_capacity(checks.len() + skipped_checks.len());
        let mut analysis_status = AnalysisStatus::Complete;
        let mut merge_recommendation = MergeRecommendation::Approve;
        let mut blocking_issues = Vec::new();
        let mut review_caveats = Vec::new();

        for check in checks {
            let eval = self.evaluate_run(check);
            bump_summary_status(&mut analysis_status, &mut merge_recommendation, &eval);
            if eval.conclusion == PolicyConclusion::Blocked {
                blocking_issues.push(format!(
                    "{} ({})",
                    check.name,
                    display_status_label(check.status.as_str())
                ));
            } else if eval.conclusion == PolicyConclusion::Advisory {
                review_caveats.push(describe_advisory_check(&eval));
            }
            evaluations.push(eval);
        }

        for skipped in skipped_checks {
            let eval = self.evaluate_skip(skipped);
            bump_summary_status(&mut analysis_status, &mut merge_recommendation, &eval);
            if eval.conclusion == PolicyConclusion::Blocked {
                blocking_issues.push(format!("{} ({})", skipped.name, skipped.reason));
            } else if eval.conclusion == PolicyConclusion::Advisory {
                review_caveats.push(format!("{} skipped: {}", skipped.name, skipped.reason));
            }
            evaluations.push(eval);
        }

        PolicyRunSummary {
            analysis_status,
            merge_recommendation,
            evaluations,
            blocking_issues,
            review_caveats,
        }
    }
}

/// Whether a skip reason is prview's own "this change has no related test".
///
/// Prefix-matched on the constant, the same shape as [`is_mode_skip_reason`]:
/// the check may append how many inputs it considered, and that detail must not
/// change the classification. Necessary but NOT sufficient on its own — see
/// [`proved_empty_test_scope`].
fn is_no_tests_related_skip(reason: &str) -> bool {
    reason.starts_with(crate::checks::scope::NO_TESTS_RELATED_TO_THE_CHANGE)
}

/// Whether this skipped result really is an empty test scope, with evidence.
///
/// The exception that lets `no tests related to the change` approve a REQUIRED
/// gate must not be reachable by writing a sentence. Three independent facts
/// have to agree, and `reason` (already lowercased) is only one of them:
///
/// 1. the check owns an ecosystem's test scope (`Ecosystem::owning_check`), so
///    Clippy or Semgrep reporting this text is never eligible;
/// 2. it produced this result by running — the caller is `evaluate_run`; a
///    pre-flight `SkippedCheck` never reaches here, because a check that was
///    never dispatched cannot have resolved a selection;
/// 3. its own provenance says what it did: either
///    [`ExecutedScope::NothingSelected`], the empty selection that spawned no
///    command, or a narrowed execution that collected no test. Missing
///    provenance is NOT evidence — a check that reports this sentence while
///    leaving no record of having decided anything gets no exception, and a
///    check that ran the FULL suite and then reported it contradicts itself.
///    Either way the contradiction is resolved against the claim.
fn proved_empty_test_scope(result: &CheckResult, reason: &str) -> bool {
    use crate::checks::scope::{Ecosystem, ExecutedScope};

    if Ecosystem::owning_check(&result.name).is_none() || !is_no_tests_related_skip(reason) {
        return false;
    }
    result.provenance.as_ref().is_some_and(|provenance| {
        matches!(
            provenance.executed_scope,
            Some(ExecutedScope::NothingSelected | ExecutedScope::ChangeScoped { .. })
        )
    })
}

fn is_mode_skip_reason(reason: &str) -> bool {
    matches!(
        reason,
        "security disabled"
            | "lint disabled"
            | "tests disabled"
            | "heuristics disabled"
            | "requires --security-full"
    )
}

/// Whether a check is a semgrep scan whose output reports scan/parse errors —
/// meaning it analysed the target incompletely. Completeness is a policy signal
/// (it feeds `confidence_impact`), so evaluating it here keeps the analysis
/// status derived in one place; the detection itself lives in the semgrep module
/// as the single source of truth for "what a degraded semgrep scan looks like".
fn semgrep_scan_is_degraded(check_id: &str, result: &CheckResult) -> bool {
    check_id == "semgrep_scan" && crate::checks::semgrep_output_reports_scan_errors(&result.output)
}

/// Map a skip's execution-state classification to its reported tool outcome.
fn skip_outcome_for(execution_state: CheckExecutionState) -> ToolOutcome {
    match execution_state {
        CheckExecutionState::Skipped => ToolOutcome::Skipped,
        CheckExecutionState::Unavailable => ToolOutcome::Unavailable,
        CheckExecutionState::Unknown => ToolOutcome::Unknown,
        CheckExecutionState::Executed => ToolOutcome::Skipped,
    }
}

/// `empty_test_scope` carries the same proof [`skip_policy_outcome`] gets, so
/// the two cannot disagree about what a skip was: a reason that merely reads
/// like an empty selection is classified by the rules below it, not by its text.
fn classify_skip_execution_state(reason: &str, empty_test_scope: bool) -> CheckExecutionState {
    if reason.is_empty() {
        return CheckExecutionState::Unknown;
    }
    if reason.starts_with("profile")
        || is_mode_skip_reason(reason)
        || empty_test_scope
        || reason.contains("fast remote-only preset")
    {
        return CheckExecutionState::Skipped;
    }
    if reason.contains("not installed")
        || reason.contains("not found")
        || reason.contains("missing")
        || reason.contains("unavailable")
    {
        return CheckExecutionState::Unavailable;
    }
    CheckExecutionState::Unknown
}

fn bump_summary_status(
    analysis_status: &mut AnalysisStatus,
    merge_recommendation: &mut MergeRecommendation,
    eval: &CheckEvaluation,
) {
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

fn describe_advisory_check(eval: &CheckEvaluation) -> String {
    match eval.execution_state {
        CheckExecutionState::Executed => format!("{} returned {}", eval.name, eval.raw_status),
        CheckExecutionState::Skipped => format!("{} was skipped", eval.name),
        CheckExecutionState::Unavailable => format!("{} was unavailable for this run", eval.name),
        CheckExecutionState::Unknown => format!("{} needs manual review", eval.name),
    }
}

fn display_status_label(status: &str) -> String {
    let mut chars = status.chars();
    match chars.next() {
        Some(first) => format!("{}{}", first.to_ascii_uppercase(), chars.as_str()),
        None => status.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The two enum decision axes reach MERGE_GATE.json through `serde`, and
    /// `tools/validate_merge_gate.py` mirrors their spellings as a closed,
    /// case-sensitive vocabulary — the same arrangement `CheckStatus::EMITTED`
    /// has with `VALID_CHECK_STATUSES`. Renaming a variant without touching the
    /// validator would leave the contract gate certifying a word no reader
    /// ranks, so pin the wire spelling where the rename would happen.
    #[test]
    fn the_decision_axes_serialize_to_the_words_the_validator_knows() {
        let analysis = [
            (AnalysisStatus::Complete, "complete"),
            (AnalysisStatus::Degraded, "degraded"),
            (AnalysisStatus::Incomplete, "incomplete"),
        ];
        for (status, spelling) in analysis {
            assert_eq!(
                serde_json::to_value(status).expect("serialize analysis status"),
                serde_json::Value::String(spelling.to_string()),
                "VALID_ANALYSIS_STATUSES in tools/validate_merge_gate.py lists {spelling}"
            );
        }

        let recommendations = [
            (MergeRecommendation::Approve, "approve"),
            (MergeRecommendation::ReviewRequired, "review_required"),
            (MergeRecommendation::Block, "block"),
        ];
        for (recommendation, spelling) in recommendations {
            assert_eq!(
                serde_json::to_value(recommendation).expect("serialize recommendation"),
                serde_json::Value::String(spelling.to_string()),
                "VALID_MERGE_RECOMMENDATIONS in tools/validate_merge_gate.py lists {spelling}"
            );
        }
    }

    /// The rank table the validator ports. Every `verdict` the emitter derives
    /// is the MAX rank of the axes it derived it from, which is what lets the
    /// contract gate reject a verdict milder than its own axes.
    #[test]
    fn the_derived_verdict_is_the_most_conservative_axis() {
        let rank = |verdict: &str| match verdict {
            "PASS" => 1,
            "CONDITIONAL" => 2,
            _ => 3,
        };
        for recommendation in [
            MergeRecommendation::Approve,
            MergeRecommendation::ReviewRequired,
            MergeRecommendation::Block,
        ] {
            for analysis in [
                AnalysisStatus::Complete,
                AnalysisStatus::Degraded,
                AnalysisStatus::Incomplete,
            ] {
                for quality_pass in [true, false] {
                    let recommendation_rank = match recommendation {
                        MergeRecommendation::Approve => 1,
                        MergeRecommendation::ReviewRequired => 2,
                        MergeRecommendation::Block => 3,
                    };
                    // Only values that RULE OUT a milder outcome state a rank:
                    // `complete` and `quality_pass: true` are preconditions of
                    // PASS, not grants of it.
                    let analysis_rank = if analysis == AnalysisStatus::Complete {
                        1
                    } else {
                        2
                    };
                    let quality_rank = if quality_pass { 1 } else { 2 };
                    assert_eq!(
                        rank(recommendation.legacy_verdict(analysis, quality_pass)),
                        recommendation_rank.max(analysis_rank).max(quality_rank),
                        "{recommendation:?} + {analysis:?} + quality_pass={quality_pass}"
                    );
                }
            }
        }
    }

    #[test]
    fn explicitly_disabled_heuristics_is_a_mode_skip() {
        assert!(is_mode_skip_reason("heuristics disabled"));
        assert_eq!(
            classify_skip_execution_state("requires --security-full", false),
            CheckExecutionState::Skipped,
            "declared mode skips must not masquerade as unknown tool loss"
        );
    }
    // -----------------------------------------------------------------------
    // A test suite that does not apply to this change
    // -----------------------------------------------------------------------

    /// A gate that ran, found the change touches nothing it covers, and
    /// executed nothing.
    /// A skipped row with no provenance at all: nothing says what, if anything,
    /// this check decided or executed.
    fn skipped_without_provenance(name: &str, reason: &str) -> CheckResult {
        CheckResult {
            name: name.to_string(),
            status: CheckStatus::Skipped,
            duration: std::time::Duration::from_millis(1),
            output: reason.to_string(),
            cached: false,
            provenance: None,
        }
    }

    /// The shape a real test check produces for an empty selection: no command
    /// was spawned, and the empty selection itself is the recorded evidence.
    fn scope_skipped(name: &str, reason: &str) -> CheckResult {
        let mut result = skipped_without_provenance(name, reason);
        result.provenance = Some(crate::checks::CheckProvenance {
            command: "<no command recorded>".to_string(),
            tool_version: None,
            cwd: ".".to_string(),
            exit_code: None,
            started_at: "2026-09-16T00:00:00+00:00".to_string(),
            finished_at: "2026-09-16T00:00:00+00:00".to_string(),
            hard_fail_signatures: Vec::new(),
            cache_key: None,
            target_sha: None,
            tree_state: None,
            executed_scope: Some(crate::checks::scope::ExecutedScope::NothingSelected),
        });
        result
    }

    fn config_requiring_cargo_test() -> Config {
        let mut config = crate::config::test_config();
        config.policy.mode = crate::policy::PolicyMode::Block;
        config
            .policy
            .checks
            .insert("cargo_test".to_string(), PolicySeverity::Block);
        config
    }

    /// A docs-only PR: the decision selected no package, so `cargo test` ran
    /// nothing. Before this branch, a repository that REQUIRES the test gate
    /// would have been blocked by its own narrowing.
    #[test]
    fn a_required_suite_with_no_related_tests_does_not_block_the_merge() {
        let config = config_requiring_cargo_test();
        let engine = PolicyEngine::new(&config);

        let eval = engine.evaluate_run(&scope_skipped(
            "Cargo test",
            crate::checks::scope::NO_TESTS_RELATED_TO_THE_CHANGE,
        ));

        assert_eq!(eval.conclusion, PolicyConclusion::Satisfied);
        assert_eq!(eval.confidence_impact, AnalysisStatus::Complete);
        assert_eq!(eval.merge_impact, MergeRecommendation::Approve);
        // Never relabelled as a pass: the row still says a suite did not run.
        assert_eq!(eval.raw_status, "skipped");
        assert_eq!(eval.outcome, ToolOutcome::Skipped);
        assert_eq!(eval.execution_state, CheckExecutionState::Skipped);
    }

    /// Vitest appends its own detail after the constant, because the empty set
    /// is only knowable after the tool has resolved the import graph.
    #[test]
    fn the_branch_survives_the_detail_vitest_appends() {
        let config = config_requiring_cargo_test();
        let engine = PolicyEngine::new(&config);

        let eval = engine.evaluate_run(&scope_skipped(
            "Vitest",
            &format!(
                "{} (none of the 3 changed source files are imported by a test)",
                crate::checks::scope::NO_TESTS_RELATED_TO_THE_CHANGE
            ),
        ));

        assert_eq!(eval.conclusion, PolicyConclusion::Satisfied);
        assert_eq!(eval.merge_impact, MergeRecommendation::Approve);
    }

    /// The branch is narrow on purpose: a gate that could not run at all is
    /// still a hole in the evidence, and still blocks.
    #[test]
    fn a_required_suite_that_could_not_run_still_blocks() {
        let config = config_requiring_cargo_test();
        let engine = PolicyEngine::new(&config);

        let eval = engine.evaluate_run(&scope_skipped("Cargo test", "cargo not installed"));

        assert_eq!(eval.conclusion, PolicyConclusion::Blocked);
        assert_eq!(eval.merge_impact, MergeRecommendation::Block);
        assert_eq!(eval.execution_state, CheckExecutionState::Unavailable);
    }

    /// Only prview's own wording reaches the branch. A tool that prints
    /// something similar is not prview saying it proved anything.
    #[test]
    fn the_branch_matches_only_prviews_own_reason() {
        let config = config_requiring_cargo_test();
        let engine = PolicyEngine::new(&config);

        let eval = engine.evaluate_run(&scope_skipped(
            "Cargo test",
            "the harness reported no tests related to the change",
        ));

        assert_eq!(eval.conclusion, PolicyConclusion::Blocked);
    }

    /// The same result, with provenance saying how much actually executed.
    fn scope_skipped_with(
        name: &str,
        reason: &str,
        executed: Option<crate::checks::scope::ExecutedScope>,
    ) -> CheckResult {
        let mut result = skipped_without_provenance(name, reason);
        result.provenance = Some(crate::checks::CheckProvenance {
            command: format!("{name} --some-argument"),
            tool_version: None,
            cwd: ".".to_string(),
            exit_code: Some(0),
            started_at: "2026-09-16T00:00:00+00:00".to_string(),
            finished_at: "2026-09-16T00:00:01+00:00".to_string(),
            hard_fail_signatures: Vec::new(),
            cache_key: None,
            target_sha: None,
            tree_state: None,
            executed_scope: executed,
        });
        result
    }

    /// The reason is prview's own, but the check is not one that HAS a test
    /// scope. Nothing proved an empty selection, so a required gate that says
    /// this still blocks.
    #[test]
    fn a_lint_cannot_spell_its_way_into_the_empty_selection_branch() {
        let mut config = config_requiring_cargo_test();
        config
            .policy
            .checks
            .insert("clippy".to_string(), PolicySeverity::Block);
        let engine = PolicyEngine::new(&config);

        let eval = engine.evaluate_run(&scope_skipped(
            "Clippy",
            crate::checks::scope::NO_TESTS_RELATED_TO_THE_CHANGE,
        ));

        assert_eq!(eval.conclusion, PolicyConclusion::Blocked);
        assert_eq!(eval.merge_impact, MergeRecommendation::Block);
        assert_ne!(
            eval.execution_state,
            CheckExecutionState::Skipped,
            "a lint that was never scoped must not be classified as a scoped skip",
        );
    }

    /// A pre-flight skip is decided before anything runs, so it cannot be the
    /// result of a selection that came out empty — whatever its reason says.
    #[test]
    fn a_pre_flight_skip_never_reaches_the_empty_selection_branch() {
        let config = config_requiring_cargo_test();
        let engine = PolicyEngine::new(&config);

        let eval = engine.evaluate_skip(&SkippedCheck {
            id: "cargo_test".to_string(),
            name: "Cargo test".to_string(),
            reason: crate::checks::scope::NO_TESTS_RELATED_TO_THE_CHANGE.to_string(),
        });

        assert_eq!(eval.conclusion, PolicyConclusion::Blocked);
        assert_eq!(eval.merge_impact, MergeRecommendation::Block);
    }

    /// The check ran the WHOLE suite and then reported an empty selection. The
    /// two claims contradict each other, and the contradiction is resolved
    /// against the one that would approve a required gate.
    #[test]
    fn a_full_run_that_claims_an_empty_selection_still_blocks() {
        let config = config_requiring_cargo_test();
        let engine = PolicyEngine::new(&config);

        let eval = engine.evaluate_run(&scope_skipped_with(
            "Cargo test",
            crate::checks::scope::NO_TESTS_RELATED_TO_THE_CHANGE,
            Some(crate::checks::scope::ExecutedScope::Full {
                reason: "manifest or lockfile changed: Cargo.lock".to_string(),
            }),
        ));

        assert_eq!(eval.conclusion, PolicyConclusion::Blocked);
        assert_eq!(eval.merge_impact, MergeRecommendation::Block);
    }

    /// A narrowed run that executed and found nothing related: provenance and
    /// reason agree, so the branch applies.
    #[test]
    fn a_narrowed_run_that_found_no_related_test_is_approved() {
        let config = config_requiring_cargo_test();
        let engine = PolicyEngine::new(&config);

        let eval = engine.evaluate_run(&scope_skipped_with(
            "Vitest",
            crate::checks::scope::NO_TESTS_RELATED_TO_THE_CHANGE,
            Some(crate::checks::scope::ExecutedScope::ChangeScoped {
                selected: 2,
                selector: "related --run src/a.ts src/b.ts".to_string(),
            }),
        ));

        assert_eq!(eval.conclusion, PolicyConclusion::Satisfied);
        assert_eq!(eval.merge_impact, MergeRecommendation::Approve);
        assert_eq!(eval.execution_state, CheckExecutionState::Skipped);
    }

    /// A check that left provenance but no execution evidence at all. The claim
    /// has nothing backing it, so it does not reach the branch.
    #[test]
    fn an_unconfirmed_narrowing_does_not_reach_the_branch() {
        let config = config_requiring_cargo_test();
        let engine = PolicyEngine::new(&config);

        let eval = engine.evaluate_run(&scope_skipped_with(
            "Cargo test",
            crate::checks::scope::NO_TESTS_RELATED_TO_THE_CHANGE,
            None,
        ));

        assert_eq!(eval.conclusion, PolicyConclusion::Blocked);
    }

    /// Absent provenance is absence of evidence, not evidence of an empty
    /// selection. A required gate that reports this sentence while recording
    /// nothing about what it decided keeps blocking — the exception is earned
    /// by a proof the check publishes, never by a row that says nothing.
    #[test]
    fn a_skip_with_no_provenance_at_all_does_not_reach_the_branch() {
        let config = config_requiring_cargo_test();
        let engine = PolicyEngine::new(&config);

        let eval = engine.evaluate_run(&skipped_without_provenance(
            "Cargo test",
            crate::checks::scope::NO_TESTS_RELATED_TO_THE_CHANGE,
        ));

        assert_eq!(eval.conclusion, PolicyConclusion::Blocked);
        assert_eq!(eval.merge_impact, MergeRecommendation::Block);
    }

    /// The empty selection the checks actually publish: no command, and
    /// `NothingSelected` as the executed scope. This is the row the exception
    /// exists for.
    #[test]
    fn a_proven_empty_selection_is_approved() {
        let config = config_requiring_cargo_test();
        let engine = PolicyEngine::new(&config);

        let eval = engine.evaluate_run(&scope_skipped_with(
            "Cargo test",
            crate::checks::scope::NO_TESTS_RELATED_TO_THE_CHANGE,
            Some(crate::checks::scope::ExecutedScope::NothingSelected),
        ));

        assert_eq!(eval.conclusion, PolicyConclusion::Satisfied);
        assert_eq!(eval.merge_impact, MergeRecommendation::Approve);
        assert_eq!(eval.execution_state, CheckExecutionState::Skipped);
    }
}
