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
            let execution_state = classify_skip_execution_state(&reason);
            let (conclusion, confidence_impact, merge_impact) =
                self.skip_policy_outcome(severity, &reason);
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
        let execution_state = classify_skip_execution_state(&reason);
        let (conclusion, confidence_impact, merge_impact) =
            self.skip_policy_outcome(severity, &reason);

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
    ) -> (PolicyConclusion, AnalysisStatus, MergeRecommendation) {
        if reason.starts_with("profile") {
            // Profile mismatches (e.g. running a rust check on a JS repo) are
            // totally fine — the check simply does not apply here.
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

fn is_mode_skip_reason(reason: &str) -> bool {
    matches!(
        reason,
        "security disabled" | "lint disabled" | "tests disabled" | "requires --security-full"
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

fn classify_skip_execution_state(reason: &str) -> CheckExecutionState {
    if reason.is_empty() {
        return CheckExecutionState::Unknown;
    }
    if reason.starts_with("profile")
        || reason.contains("disabled")
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
