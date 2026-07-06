//! Quality gate adapter for contractual process exit codes.

use crate::output::CliJsonSummary;
use crate::policy::engine::{AnalysisStatus, MergeRecommendation};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::path::Path;

pub const GATE_EXECUTION_ERROR_EXIT_CODE: i32 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum GateVerdict {
    #[serde(rename = "PASS")]
    Pass,
    #[serde(rename = "CONDITIONAL")]
    Conditional,
    #[serde(rename = "BLOCK")]
    Block,
}

impl GateVerdict {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pass => "PASS",
            Self::Conditional => "CONDITIONAL",
            Self::Block => "BLOCK",
        }
    }
}

impl TryFrom<&str> for GateVerdict {
    type Error = anyhow::Error;

    fn try_from(value: &str) -> Result<Self> {
        match value {
            "PASS" => Ok(Self::Pass),
            "CONDITIONAL" => Ok(Self::Conditional),
            "BLOCK" => Ok(Self::Block),
            other => bail!("unknown gate verdict `{other}`"),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct GateJsonOutput {
    pub schema_version: &'static str,
    pub verdict: GateVerdict,
    pub exit_code: i32,
    pub strict: bool,
    pub status: String,
    pub analysis_status: AnalysisStatus,
    pub merge_recommendation: MergeRecommendation,
    pub allow_merge: bool,
    pub quality_pass: bool,
    pub output_dir: String,
    pub merge_gate_json: String,
    pub caveats: Vec<String>,
    pub blocking_issues: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub decision_reason: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct MergeGateFile {
    decision: MergeGateDecision,
}

#[derive(Debug, Clone, Deserialize)]
struct MergeGateDecision {
    verdict: String,
    analysis_status: AnalysisStatus,
    merge_recommendation: MergeRecommendation,
    allow_merge: bool,
    quality_pass: bool,
    #[serde(default)]
    decision_reason: Option<String>,
    #[serde(default)]
    review_caveats: Vec<String>,
    #[serde(default)]
    blocking_issues: Vec<String>,
}

pub fn gate_exit_code(verdict: GateVerdict, strict: bool) -> i32 {
    match verdict {
        GateVerdict::Pass => 0,
        GateVerdict::Conditional if strict => 2,
        GateVerdict::Conditional => 0,
        GateVerdict::Block => 1,
    }
}

pub fn build_gate_json_output(
    summary: &CliJsonSummary,
    merge_gate_path: &Path,
    strict: bool,
) -> Result<GateJsonOutput> {
    let decision = read_merge_gate_decision(merge_gate_path)?;
    if summary.verdict != decision.verdict {
        bail!(
            "gate verdict mismatch: CLI summary has `{}`, MERGE_GATE.json has `{}`",
            summary.verdict,
            decision.verdict
        );
    }

    let verdict = GateVerdict::try_from(decision.verdict.as_str())?;
    let exit_code = gate_exit_code(verdict, strict);

    Ok(GateJsonOutput {
        schema_version: "gate-json/v1",
        verdict,
        exit_code,
        strict,
        status: summary.status.to_string(),
        analysis_status: decision.analysis_status,
        merge_recommendation: decision.merge_recommendation,
        allow_merge: decision.allow_merge,
        quality_pass: decision.quality_pass,
        output_dir: summary.output_dir.clone(),
        merge_gate_json: merge_gate_path.display().to_string(),
        caveats: decision.review_caveats,
        blocking_issues: decision.blocking_issues,
        decision_reason: decision.decision_reason,
    })
}

fn read_merge_gate_decision(path: &Path) -> Result<MergeGateDecision> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read merge gate artifact {}", path.display()))?;
    let gate: MergeGateFile = serde_json::from_str(&raw)
        .with_context(|| format!("failed to parse merge gate artifact {}", path.display()))?;
    Ok(gate.decision)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gate_exit_code_maps_contract() {
        assert_eq!(gate_exit_code(GateVerdict::Pass, false), 0);
        assert_eq!(gate_exit_code(GateVerdict::Pass, true), 0);
        assert_eq!(gate_exit_code(GateVerdict::Conditional, false), 0);
        assert_eq!(gate_exit_code(GateVerdict::Conditional, true), 2);
        assert_eq!(gate_exit_code(GateVerdict::Block, false), 1);
        assert_eq!(gate_exit_code(GateVerdict::Block, true), 1);
    }

    #[test]
    fn gate_verdict_parse_fails_loud_for_unknown_values() {
        assert!(GateVerdict::try_from("HOLD").is_err());
        assert!(GateVerdict::try_from("ALLOW").is_err());
    }
}
