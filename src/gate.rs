//! Quality gate adapter for contractual process exit codes.

use crate::output::CliJsonSummary;
use crate::policy::engine::{AnalysisStatus, MergeRecommendation};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::path::Path;

pub const GATE_EXECUTION_ERROR_EXIT_CODE: i32 = 3;

/// `schema_version` this build stamps into `MERGE_GATE.json`.
pub const MERGE_GATE_SCHEMA_VERSION: &str = "2.2";

/// `MERGE_GATE.json` schemas this build has actually seen, as `(MAJOR, MINOR)`.
///
/// This is the SAME set `tools/validate_merge_gate.py` accepts verbatim
/// (`1.0` / `2.0` / `2.1` / `2.2`), so "readable by the CLI/MCP" and "valid per the
/// contract validator" cannot drift apart for a version in the set. The reader
/// is deliberately broader in exactly two documented directions — an absent
/// field and a newer MINOR of a known MAJOR — and both are announced rather
/// than silent.
const MERGE_GATE_KNOWN_SCHEMAS: &[(u32, u32)] = &[(1, 0), (2, 0), (2, 1), (2, 2)];

/// Parse a strict `MAJOR.MINOR` version. Anything else — a bare `2`, a trailing
/// dot, or a third component like `2.1.3` — is NOT this contract's version
/// shape and must not be silently truncated into one.
///
/// Each component must also be spelled canonically. `u32::from_str` accepts a
/// leading `+` and leading zeros, so `02.02` and `+2.2` would parse to the same
/// `(2, 2)` as `2.2` and be read as a known schema — while
/// `tools/validate_merge_gate.py` compares the raw string and rejects them. The
/// accepted set must BE the validator's set, not a superset that happens to
/// parse into it.
fn parse_major_minor(version: &str) -> Option<(u32, u32)> {
    let (major, minor) = version.split_once('.')?;
    if minor.contains('.') {
        return None;
    }
    Some((canonical_u32(major)?, canonical_u32(minor)?))
}

/// Parse a decimal component written exactly as the validator would compare it:
/// digits only, and no leading zero unless the component IS `0`.
fn canonical_u32(component: &str) -> Option<u32> {
    if component.is_empty() || !component.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    if component.len() > 1 && component.starts_with('0') {
        return None;
    }
    component.parse().ok()
}

/// Newest MINOR this build knows for `major`, if the MAJOR is known at all.
fn newest_known_minor(major: u32) -> Option<u32> {
    MERGE_GATE_KNOWN_SCHEMAS
        .iter()
        .filter(|(known_major, _)| *known_major == major)
        .map(|(_, minor)| *minor)
        .max()
}

/// Check a pack's `MERGE_GATE.json` `schema_version` against what this build can
/// read, so readers stop guessing at packs they do not understand.
///
/// * absent — accepted silently; packs predating the field are the documented
///   legacy read-back surface (same safety net as the retired `ALLOW`/`HOLD`
///   verdict synonyms).
/// * known MAJOR, MINOR this build has seen — accepted silently.
/// * known MAJOR, newer MINOR — accepted with a caveat: the pack may carry
///   fields this build ignores, and the reader must say so. This holds on EVERY
///   known MAJOR, not just the current one: a `1.9` pack is as unseen as a
///   `2.9` one, and silence about it was a reader claiming a fidelity it does
///   not have.
/// * unknown MAJOR, or a version that is not `MAJOR.MINOR` at all — fail loud;
///   a reader that cannot name the schema cannot honestly name the verdict.
pub fn check_merge_gate_schema(raw: Option<&str>) -> Result<Option<String>> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    let Some((major, minor)) = parse_major_minor(raw) else {
        bail!("unreadable MERGE_GATE.json schema_version `{raw}` (expected MAJOR.MINOR)");
    };
    let Some(newest_minor) = newest_known_minor(major) else {
        let known: Vec<String> = MERGE_GATE_KNOWN_SCHEMAS
            .iter()
            .map(|(major, minor)| format!("{major}.{minor}"))
            .collect();
        bail!(
            "unsupported MERGE_GATE.json schema_version `{raw}`: major {major} is not readable by \
             this build (known schemas: {}; current schema {MERGE_GATE_SCHEMA_VERSION})",
            known.join(", ")
        );
    };
    if minor > newest_minor {
        return Ok(Some(format!(
            "schema_forward_compat: MERGE_GATE.json schema_version `{raw}` is newer than the \
             newest `{major}.{newest_minor}` this build knows; unknown fields were ignored"
        )));
    }
    Ok(None)
}

/// [`check_merge_gate_schema`] for a raw JSON field, distinguishing "absent"
/// from "present but not a string".
///
/// Every reader used to reach the checker through `.and_then(Value::as_str)`,
/// which maps a number, an object, or an explicit `null` onto `None` — the one
/// input the checker accepts in silence, because an absent field means a
/// pre-2.1 pack. A pack that states a `schema_version` this build cannot even
/// type is the opposite of a legacy pack and must fail loud.
pub fn check_merge_gate_schema_field(field: Option<&serde_json::Value>) -> Result<Option<String>> {
    match field {
        None => check_merge_gate_schema(None),
        Some(serde_json::Value::String(raw)) => check_merge_gate_schema(Some(raw)),
        Some(other) => bail!(
            "unreadable MERGE_GATE.json schema_version: expected a MAJOR.MINOR string, found {}",
            match other {
                serde_json::Value::Null => "null".to_string(),
                serde_json::Value::Bool(_) => "a boolean".to_string(),
                serde_json::Value::Number(n) => format!("the number {n}"),
                serde_json::Value::Array(_) => "an array".to_string(),
                serde_json::Value::Object(_) => "an object".to_string(),
                serde_json::Value::String(_) => unreachable!("handled above"),
            }
        ),
    }
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
        assert_eq!(check_merge_gate_schema(Some("2.2")).unwrap(), None);
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
    fn schema_check_rejects_versions_that_are_not_major_minor() {
        // `tools/validate_merge_gate.py` accepts an exact string set, so a
        // reader that silently truncates `2.1.3` to `2.1` calls a pack readable
        // that the contract validator rejects.
        for raw in ["2.1.3", "2", "2.", "2.1.", ".1", "2.1.0.0"] {
            assert!(
                check_merge_gate_schema(Some(raw)).is_err(),
                "`{raw}` is not MAJOR.MINOR and must fail loud"
            );
        }
    }

    #[test]
    fn schema_check_rejects_non_canonical_component_spelling() {
        // `u32::from_str` accepts leading zeros and a leading `+`, so `02.02`,
        // `2.02` and `+2.2` all normalized to the known `(2, 2)` and were read
        // as the current schema — while the validator rejects those exact
        // strings. The accepted set has to be the validator's set, not a
        // superset that happens to parse.
        for raw in ["02.2", "2.02", "+2.2", "2.+2", "02.02", " 2.2", "2.2 "] {
            assert!(
                check_merge_gate_schema(Some(raw)).is_err(),
                "`{raw}` is not canonical MAJOR.MINOR and must fail loud"
            );
        }
        // The canonical spellings, including a genuine zero component, stay read.
        assert_eq!(check_merge_gate_schema(Some("2.0")).unwrap(), None);
        assert_eq!(check_merge_gate_schema(Some("1.0")).unwrap(), None);
    }

    #[test]
    fn schema_check_caveats_newer_minor_of_a_legacy_major() {
        // A `1.x` pack newer than the only released `1.0` was accepted in total
        // silence, while the same situation on the current major produced a
        // caveat. Unknown minors carry unknown fields on every known major.
        let caveat = check_merge_gate_schema(Some("1.9"))
            .expect("a known major stays readable")
            .expect("a minor this build never saw must be named");
        assert!(caveat.starts_with("schema_forward_compat:"), "{caveat}");
        assert!(caveat.contains("1.9"), "{caveat}");
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
