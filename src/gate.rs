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

/// JSON type a decision signal is expected to carry.
#[derive(Clone, Copy)]
pub(crate) enum JsonKind {
    String,
    Boolean,
}

impl JsonKind {
    fn matches(self, value: &serde_json::Value) -> bool {
        match self {
            Self::String => value.is_string(),
            Self::Boolean => value.is_boolean(),
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::String => "a string",
            Self::Boolean => "a boolean",
        }
    }
}

/// Human-readable JSON type name, for saying what was found instead.
fn json_type_name(value: &serde_json::Value) -> &'static str {
    match value {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "a boolean",
        serde_json::Value::Number(_) => "a number",
        serde_json::Value::String(_) => "a string",
        serde_json::Value::Array(_) => "an array",
        serde_json::Value::Object(_) => "an object",
    }
}

/// A decision signal, or `None` plus a caveat when it is present with the wrong
/// JSON type.
///
/// Absence is the one state a reader accepts in silence — it is the documented
/// shape of an older pack. A field that IS there but cannot be typed is a
/// different thing entirely, and collapsing the two through `as_str()` lets a
/// reader ignore a signal while reporting a clean passthrough.
///
/// Shared by both readers on purpose: the CLI and the MCP adapter answer the
/// same contract question about the same artifact, and the one that had this
/// rule while the other did not is how `merge_recommendation: 7` came back as
/// `storage_corrupt` from one surface and as `approve` from the other.
pub(crate) fn readable_signal<'v>(
    field: &str,
    value: Option<&'v serde_json::Value>,
    want: JsonKind,
    caveats: &mut Vec<String>,
) -> Option<&'v serde_json::Value> {
    let present = value?;
    if want.matches(present) {
        return Some(present);
    }
    caveats.push(format!(
        "unreadable_{field}: MERGE_GATE.json {field} is {}, not {}; it was ignored when deriving \
         this decision",
        json_type_name(present),
        want.label()
    ));
    None
}

/// Select the object a gate pack's decision is read from.
///
/// A stated `decision` object always wins, and only then does the presence of
/// `schema_version` decide what an absent one means:
///
/// * no `schema_version` — a pack predating the field. Its ROOT is the decision;
///   this is the legacy read-back surface every reader keeps. The tolerance
///   answers WHERE the decision sits when nothing else states it — it is not a
///   rule that the root outranks a `decision` object the pack did write. A
///   schema-less pack carrying one is read from it, because the alternative is
///   to read a plainly stated decision as a decision with every signal missing,
///   and every signal missing normalizes to BLOCK.
/// * `schema_version` stated — the `decision` object that schema is built around
///   is mandatory. Falling back to the root there would publish a verdict
///   nothing in the pack stated, which is a re-derivation wearing a reader's
///   clothes. `tools/validate_merge_gate.py` requires `decision` at every
///   version, so a reader that shrugs disagrees with the contract validator.
///
/// The legacy tolerance is about WHERE the decision sits, not about whether the
/// pack is a decision at all: a root that is an array, a scalar or `null` has no
/// fields to read, and accepting it let the CLI answer a normalized BLOCK for an
/// artifact the MCP reader called corrupt. Both now reject it.
///
/// `Err` describes which shape rule the pack broke; callers add their own
/// framing.
pub fn select_decision_object(
    value: &serde_json::Value,
) -> Result<&serde_json::Value, DecisionShapeError> {
    match value.get("decision") {
        Some(decision) if decision.is_object() => Ok(decision),
        _ if value.get("schema_version").is_some() => {
            Err(DecisionShapeError::VersionedWithoutDecision(
                value
                    .get("schema_version")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("?")
                    .to_string(),
            ))
        }
        _ if value.is_object() => Ok(value),
        _ => Err(DecisionShapeError::NonObjectRoot(json_type_name(value))),
    }
}

/// Why a gate pack carries no readable decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecisionShapeError {
    /// The pack names its schema and then omits the object that schema is built
    /// around.
    VersionedWithoutDecision(String),
    /// The pack states no schema, so its root WOULD be the decision — but the
    /// root is not an object.
    NonObjectRoot(&'static str),
}

impl DecisionShapeError {
    /// The defect, as a clause a caller can put in its own sentence.
    pub fn describe(&self) -> String {
        match self {
            Self::VersionedWithoutDecision(schema) => {
                format!("states schema_version {schema} but carries no `decision` object")
            }
            Self::NonObjectRoot(kind) => {
                format!("is {kind}, not a JSON object, so it states no decision at all")
            }
        }
    }
}

/// Conservativeness rank of one decision axis: 1 = clean pass, 2 = hold /
/// review required, 3 = block.
///
/// Both readers reconcile a decision by taking the MAX rank across the axes the
/// pack states, then publishing every axis from that one number. A pack whose
/// `verdict` says BLOCK beside a `merge_recommendation` of `approve` is
/// contradictory, and a reader that simply believes each field in turn
/// publishes an approval the artifact never gave. The rule lives here so the
/// CLI and the MCP adapter cannot answer it differently.
pub fn rank_from_merge_rec(s: &str) -> Option<u8> {
    match s.to_ascii_lowercase().as_str() {
        "block" => Some(3),
        "review_required" | "hold" => Some(2),
        "approve" => Some(1),
        _ => None,
    }
}

/// The canonical verdict a stored spelling means, or `None` when it is outside
/// the vocabulary entirely.
///
/// This is THE verdict vocabulary. Every reader folds through it — the CLI
/// `--json` summary and the MCP adapter both — because two surfaces owning two
/// copies of one vocabulary is how they came to disagree about the same file:
/// the CLI matched the raw string case-sensitively while the adapter ranked it
/// through an uppercase fold, so `verdict: "pass"` was a clean PASS to MCP
/// automation and an unknown verdict normalized to BLOCK on the CLI.
///
/// Case is not meaning. Neither is a retired synonym: `ALLOW`/`APPROVE` are the
/// pre-2.1 spellings of a clean pass and `HOLD` of `CONDITIONAL`, kept readable
/// so a legacy pack on disk still normalizes instead of failing loud. What a
/// pack states is what it stated — reading `"pass"` as a block would fabricate
/// a verdict the artifact never gave, which is the same defect in the other
/// direction.
pub fn canonical_verdict(raw: &str) -> Option<&'static str> {
    match raw.to_ascii_uppercase().as_str() {
        "BLOCK" => Some("BLOCK"),
        "CONDITIONAL" | "HOLD" => Some("CONDITIONAL"),
        "PASS" | "APPROVE" | "ALLOW" => Some("PASS"),
        _ => None,
    }
}

pub fn rank_from_verdict(s: &str) -> Option<u8> {
    canonical_verdict(s).map(|canonical| match canonical {
        "BLOCK" => 3,
        "CONDITIONAL" => 2,
        _ => 1,
    })
}

pub fn merge_rec_from_rank(rank: u8) -> &'static str {
    match rank {
        3 => "block",
        2 => "review_required",
        _ => "approve",
    }
}

pub fn verdict_from_rank(rank: u8) -> &'static str {
    match rank {
        3 => "BLOCK",
        2 => "CONDITIONAL",
        _ => "PASS",
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
    // Fold the stored spelling before comparing. This check guards against the
    // summary and the pack stating DIFFERENT decisions; a pack spelling its
    // verdict `ALLOW`, `hold` or `pass` states the same decision the summary
    // read, and firing here on that made `prview gate` reject an artifact both
    // other readers accept.
    let stated = match canonical_verdict(&decision.verdict) {
        Some(canonical) => canonical,
        None => bail!("unknown gate verdict `{}`", decision.verdict),
    };
    if summary.verdict != stated {
        bail!(
            "gate verdict mismatch: CLI summary has `{}`, MERGE_GATE.json has `{}`",
            summary.verdict,
            decision.verdict
        );
    }

    let verdict = GateVerdict::try_from(stated)?;
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
        // `GateVerdict` is the TYPED parser for canonical values, so it stays
        // strict. Legacy and non-canonical spellings are folded by
        // `canonical_verdict` before they ever reach it.
        assert!(GateVerdict::try_from("HOLD").is_err());
        assert!(GateVerdict::try_from("ALLOW").is_err());
        assert!(GateVerdict::try_from("pass").is_err());
    }

    #[test]
    fn one_vocabulary_folds_every_spelling_the_readers_accept() {
        for spelling in ["PASS", "pass", "Pass", "ALLOW", "allow", "APPROVE"] {
            assert_eq!(canonical_verdict(spelling), Some("PASS"), "{spelling}");
        }
        for spelling in ["CONDITIONAL", "conditional", "HOLD", "hold"] {
            assert_eq!(
                canonical_verdict(spelling),
                Some("CONDITIONAL"),
                "{spelling}"
            );
        }
        for spelling in ["BLOCK", "block", "Block"] {
            assert_eq!(canonical_verdict(spelling), Some("BLOCK"), "{spelling}");
        }
        // Outside the vocabulary stays outside it: folding is about spelling,
        // not about inventing a reading.
        for spelling in ["", "MAYBE", "pas", "approved"] {
            assert_eq!(canonical_verdict(spelling), None, "{spelling}");
        }
    }

    #[test]
    fn the_rank_of_a_verdict_follows_its_canonical_form() {
        // The ranking and the folding cannot drift apart, because the ranking
        // is derived from the folding.
        for spelling in ["PASS", "pass", "ALLOW", "APPROVE"] {
            assert_eq!(rank_from_verdict(spelling), Some(1), "{spelling}");
        }
        for spelling in ["CONDITIONAL", "hold", "HOLD"] {
            assert_eq!(rank_from_verdict(spelling), Some(2), "{spelling}");
        }
        for spelling in ["BLOCK", "block"] {
            assert_eq!(rank_from_verdict(spelling), Some(3), "{spelling}");
        }
        assert_eq!(rank_from_verdict("MAYBE"), None);
    }

    #[test]
    fn a_decision_object_wins_over_the_root_even_without_a_schema() {
        // The precedence is deliberate and load-bearing, so it is asserted
        // rather than left implicit.
        //
        // The legacy tolerance answers WHERE a pack's decision sits when
        // nothing else states it — not "the root always wins". Preferring the
        // root whenever `schema_version` is absent would read this pack, whose
        // decision is stated plainly in a `decision` object, as a decision with
        // every signal missing, and every signal missing normalizes to BLOCK:
        // a fabricated block for an artifact that stated an approval.
        let nested_only = serde_json::json!({
            "checks": [],
            "decision": {"verdict": "PASS", "allow_merge": true},
        });
        let decision =
            select_decision_object(&nested_only).expect("a stated decision object is readable");
        assert_eq!(
            decision.get("verdict").and_then(|v| v.as_str()),
            Some("PASS")
        );

        // With no `decision` object the root IS the decision, which is the
        // whole of the legacy read-back surface.
        let root_only = serde_json::json!({"verdict": "ALLOW", "allow_merge": true});
        let decision = select_decision_object(&root_only).expect("a legacy root is readable");
        assert_eq!(
            decision.get("verdict").and_then(|v| v.as_str()),
            Some("ALLOW")
        );
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
