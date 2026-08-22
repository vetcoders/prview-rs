//! Quality gate adapter for contractual process exit codes.

use crate::output::CliJsonSummary;
use crate::policy::engine::{AnalysisStatus, MergeRecommendation};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::path::Path;

pub const GATE_EXECUTION_ERROR_EXIT_CODE: i32 = 3;

/// `schema_version` this build stamps into `MERGE_GATE.json`.
pub const MERGE_GATE_SCHEMA_VERSION: &str = "2.1";

/// MAJOR versions of `MERGE_GATE.json` this build knows how to read. Matches the
/// set accepted by `tools/validate_merge_gate.py` (1.0 / 2.0 / 2.1).
const MERGE_GATE_KNOWN_MAJORS: &[u32] = &[1, 2];

fn parse_major_minor(version: &str) -> Option<(u32, u32)> {
    let mut parts = version.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next().unwrap_or("0").parse().ok()?;
    Some((major, minor))
}

/// Check a pack's `MERGE_GATE.json` `schema_version` against what this build can
/// read, so readers stop guessing at packs they do not understand.
///
/// * absent — accepted silently; packs predating the field are the documented
///   legacy read-back surface (same safety net as the retired `ALLOW`/`HOLD`
///   verdict synonyms).
/// * known MAJOR, same-or-older MINOR — accepted silently.
/// * known MAJOR, newer MINOR — accepted with a caveat: the pack may carry
///   fields this build ignores, and the reader must say so.
/// * unknown or unparsable MAJOR — fail loud; a reader that cannot name the
///   schema cannot honestly name the verdict.
pub fn check_merge_gate_schema(raw: Option<&str>) -> Result<Option<String>> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    let Some((major, minor)) = parse_major_minor(raw) else {
        bail!("unreadable MERGE_GATE.json schema_version `{raw}` (expected MAJOR.MINOR)");
    };
    if !MERGE_GATE_KNOWN_MAJORS.contains(&major) {
        bail!(
            "unsupported MERGE_GATE.json schema_version `{raw}`: major {major} is not readable by \
             this build (known majors: 1, 2; current schema {MERGE_GATE_SCHEMA_VERSION})"
        );
    }
    let (current_major, current_minor) = parse_major_minor(MERGE_GATE_SCHEMA_VERSION)
        .expect("MERGE_GATE_SCHEMA_VERSION is a MAJOR.MINOR literal");
    if major == current_major && minor > current_minor {
        return Ok(Some(format!(
            "schema_forward_compat: MERGE_GATE.json schema_version `{raw}` is newer than this \
             build's `{MERGE_GATE_SCHEMA_VERSION}`; unknown fields were ignored"
        )));
    }
    Ok(None)
}

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

    #[test]
    fn schema_check_accepts_absent_and_known_versions_silently() {
        assert_eq!(check_merge_gate_schema(None).unwrap(), None);
        assert_eq!(check_merge_gate_schema(Some("2.1")).unwrap(), None);
        assert_eq!(check_merge_gate_schema(Some("2.0")).unwrap(), None);
        assert_eq!(check_merge_gate_schema(Some("1.0")).unwrap(), None);
    }

    #[test]
    fn schema_check_tolerates_newer_minor_with_caveat() {
        let caveat = check_merge_gate_schema(Some("2.7"))
            .expect("newer minor is tolerated")
            .expect("newer minor emits a caveat");
        assert!(caveat.starts_with("schema_forward_compat:"), "{caveat}");
        assert!(caveat.contains("2.7"), "{caveat}");
    }

    #[test]
    fn schema_check_fails_loud_on_unknown_major() {
        let err = check_merge_gate_schema(Some("3.0")).expect_err("unknown major must fail loud");
        assert!(err.to_string().contains("unsupported"), "{err}");
        assert!(
            check_merge_gate_schema(Some("not-a-version")).is_err(),
            "unparsable schema_version must fail loud"
        );
    }
}
