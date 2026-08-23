//! Output formatting and reporting

use crate::artifacts::cargo_audit_cli_summary;
use crate::check_id::check_id_from_name;
use crate::checks::{CheckResult, CheckStatus};
use crate::config::Config;
use crate::git::{Diff, ResolvedRef};
use crate::heuristics::HeuristicsResult;
use colored::Colorize;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::io::{self, IsTerminal};
use std::path::{Path, PathBuf};
use std::time::Duration;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

const CLI_JSON_TOP_FAILURE_LIMIT: usize = 5;

/// Final report structure
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Report {
    pub target: String,
    pub bases: Vec<String>,
    pub diffs: Vec<Diff>,
    pub checks: Vec<CheckResult>,
    pub heuristics: Option<HeuristicsResult>,
    #[serde(rename = "output_dir", default)]
    pub artifacts_dir: PathBuf,
    #[serde(with = "duration_serde")]
    pub duration: Duration,
    /// True when update mode detected no new commits since the previous run.
    /// Callers should treat this as "nothing to do" — the report is minimal.
    #[serde(default)]
    pub unchanged: bool,
}

/// Compact machine-readable summary for `prview --json` stdout.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CliJsonSummary {
    pub schema_version: &'static str,
    pub status: &'static str,
    pub verdict: String,
    pub analysis_status: crate::policy::engine::AnalysisStatus,
    pub merge_recommendation: crate::policy::engine::MergeRecommendation,
    pub allow_merge: bool,
    pub quality_pass: bool,
    pub duration_secs: f32,
    pub output_dir: String,
    pub target: String,
    pub bases: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pr: Option<CliJsonPr>,
    pub mode: CliJsonMode,
    pub checks_summary: CliJsonChecksSummary,
    pub top_failures: Vec<CliJsonFailure>,
    pub context_artifacts: Vec<CliJsonContextArtifact>,
    pub artifacts: CliJsonArtifacts,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub why_blocked: Option<String>,
    /// Reader-side caveats raised while decoding `MERGE_GATE.json` — a verdict
    /// this build had to normalize, or a pack schema newer than it understands.
    /// Empty (and omitted from the wire) on a clean read.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub caveats: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CliJsonPr {
    pub number: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CliJsonMode {
    pub execution_mode: String,
    pub remote_only: bool,
    pub remote_mode: bool,
    pub fast_remote_only_standard: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct CliJsonChecksSummary {
    pub total: usize,
    pub passed: usize,
    pub failed: usize,
    pub warned: usize,
    pub skipped: usize,
    pub cached: usize,
    /// Warning-status checks in the artifact pack's canonical check list.
    ///
    /// `warned` counts only the checks the CLI itself ran. The artifact run
    /// appends more — `public_api_diff`, `unsafe_audit`, `ghost_refs`,
    /// `heuristics_loctree` — and those reach `MERGE_GATE.json` and the
    /// dashboard but never the in-memory `Report`. This is the complete
    /// number, so it is always `>= warned`, and it is what
    /// `--ci --fail-on-warnings` keys off.
    #[serde(default)]
    pub warned_in_pack: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CliJsonFailure {
    pub id: String,
    pub name: String,
    pub status: String,
    pub summary: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CliJsonContextArtifact {
    pub key: String,
    pub path: String,
    pub generated: bool,
    pub recommended: bool,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct CliJsonArtifacts {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub review_html: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dashboard_html: Option<String>,
    pub merge_gate_json: Option<String>,
    pub run_json: Option<String>,
    pub checks_status_json: Option<String>,
    pub pr_review_md: Option<String>,
    pub report_json: Option<String>,
}

#[derive(Debug, Clone)]
struct MergeGateSummary {
    verdict: String,
    analysis_status: crate::policy::engine::AnalysisStatus,
    merge_recommendation: crate::policy::engine::MergeRecommendation,
    allow_merge: bool,
    quality_pass: bool,
    reason: Option<String>,
    caveats: Vec<String>,
    /// Warning-status entries in the pack's canonical `checks[]` list.
    warned_checks: usize,
}

mod duration_serde {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use std::time::Duration;

    pub fn serialize<S>(duration: &Duration, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        duration.as_secs_f32().serialize(serializer)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Duration, D::Error>
    where
        D: Deserializer<'de>,
    {
        let secs = f32::deserialize(deserializer)?;
        Ok(Duration::from_secs_f32(secs))
    }
}

impl Report {
    pub fn has_failures(&self) -> bool {
        self.checks.iter().any(|c| c.is_failure())
    }
}

fn failure_summary_heading(
    report: &Report,
    gate: Option<&MergeGateSummary>,
) -> Option<&'static str> {
    if !report.has_failures() {
        return None;
    }
    if gate.is_some_and(failures_degraded_to_advisory) {
        Some("Check failures downgraded to advisory/pre-existing:")
    } else {
        Some("Some checks failed:")
    }
}

fn failures_degraded_to_advisory(gate: &MergeGateSummary) -> bool {
    gate.verdict == "PASS"
        && gate.allow_merge
        && gate.quality_pass
        && gate.merge_recommendation == crate::policy::engine::MergeRecommendation::Approve
}

/// Build the `--json` / gate summary from the pack's `MERGE_GATE.json`.
///
/// The gate artifact is the ONLY derivation of the verdict. There is no
/// re-derivation from the in-memory policy engine when the artifact is missing
/// or unparsable: that fallback used to publish `allow_merge = rec != Block`,
/// the single place in the codebase where `allow_merge: true` could coexist with
/// a `CONDITIONAL` verdict, breaking the `allow_merge == (verdict == "PASS")`
/// invariant of `docs/contracts/merge_gate.md`. An unreadable pack is now an
/// execution error (exit 3), not a guess.
pub fn build_cli_json_summary(config: &Config, report: &Report) -> anyhow::Result<CliJsonSummary> {
    let gate = read_merge_gate_summary(&report.artifacts_dir)?;
    let mut checks_summary = CliJsonChecksSummary::from_checks(&report.checks);
    // The pack's list is the canonical one; the CLI's own tally is a subset of
    // it. Taking the larger keeps a legacy pack (or one whose `checks` this
    // build could not read) from reporting FEWER warnings than the CLI already
    // knows about.
    checks_summary.warned_in_pack = gate.warned_checks.max(checks_summary.warned);

    Ok(CliJsonSummary {
        schema_version: "cli-json/v1",
        status: gate
            .merge_recommendation
            .machine_status(gate.analysis_status, gate.quality_pass),
        verdict: gate.verdict.clone(),
        analysis_status: gate.analysis_status,
        merge_recommendation: gate.merge_recommendation,
        allow_merge: gate.allow_merge,
        quality_pass: gate.quality_pass,
        duration_secs: report.duration.as_secs_f32(),
        output_dir: report.artifacts_dir.display().to_string(),
        target: report.target.clone(),
        bases: report.bases.clone(),
        pr: config.pr_number.map(|number| CliJsonPr {
            number,
            url: config.pr_url.clone(),
        }),
        mode: CliJsonMode {
            execution_mode: config.execution_mode.as_str().to_string(),
            remote_only: config.remote_only,
            remote_mode: config.remote_mode,
            fast_remote_only_standard: config.is_fast_remote_only_standard(),
        },
        checks_summary,
        top_failures: top_failures(&report.checks),
        context_artifacts: read_context_artifact_summaries(&report.artifacts_dir),
        artifacts: CliJsonArtifacts::from_output_dir(&report.artifacts_dir),
        why_blocked: if !gate.allow_merge {
            gate.reason.clone()
        } else {
            None
        },
        caveats: gate.caveats,
    })
}

/// Process exit code, derived from the merge *recommendation* rather than the
/// raw check tally (PV-04).
///
/// - A hard `Block` recommendation always fails the process (`block → != 0`).
/// - Outside CI that is the ONLY thing that fails it: advisory / review-required
///   signals are informational and must not force a non-zero exit (PV-04
///   variant A — advisory-fail does not force `exit != 0`).
/// - CI (`--ci`) is the explicit strict exception: it additionally fails when
///   the analysis did not fully pass, matching the documented "strict exit
///   codes" contract of `--ci` (`block || !quality_pass → 1`).
/// - `--ci --fail-on-warnings` is the opt-in escape hatch: warning-level checks
///   no longer break `quality_pass` (a warning is not a failure), so a team that
///   wants a warnings-clean trunk asks for that exit explicitly. It counts the
///   PACK's checks, not the CLI's own list: the signal checks the artifact run
///   generates (`public_api_diff`, `unsafe_audit`, `ghost_refs`,
///   `heuristics_loctree`) warn like any other check, and a flag that promises
///   to fail on any warning cannot be blind to four of them.
///
/// `strict` is the INVOCATION's answer to "did the caller ask for `--ci`?", not
/// a property read back off the published summary. It used to be derived from
/// `mode.execution_mode == "ci"`, and that label is a preset name, not a
/// strictness flag: `--update` outranks `--ci` when the preset is resolved, so
/// `--ci --fail-on-warnings --update` published `execution_mode: "update"` and
/// silently ran lenient — the flag clap had just insisted on `--ci` for could
/// not fire, and neither could the `!quality_pass` exit `--ci` promises.
pub fn compute_exit_code(summary: &CliJsonSummary, strict: bool, fail_on_warnings: bool) -> i32 {
    use crate::policy::engine::MergeRecommendation;

    if summary.merge_recommendation == MergeRecommendation::Block {
        return 1;
    }
    if strict && !summary.quality_pass {
        return 1;
    }
    if strict && fail_on_warnings && summary.checks_summary.warned_in_pack > 0 {
        return 1;
    }
    0
}

impl CliJsonChecksSummary {
    fn from_checks(checks: &[CheckResult]) -> Self {
        let mut summary = Self {
            total: checks.len(),
            ..Self::default()
        };

        for check in checks {
            match check.status {
                CheckStatus::Passed => summary.passed += 1,
                CheckStatus::Failed | CheckStatus::Error => summary.failed += 1,
                CheckStatus::Warnings => summary.warned += 1,
                CheckStatus::Skipped => summary.skipped += 1,
            }

            if check.cached {
                summary.cached += 1;
            }
        }

        summary
    }
}

impl CliJsonArtifacts {
    fn from_output_dir(output_dir: &Path) -> Self {
        Self {
            review_html: existing_relative_path(output_dir, "review.html"),
            dashboard_html: existing_relative_path(output_dir, "dashboard.html"),
            merge_gate_json: existing_relative_path(output_dir, "00_summary/MERGE_GATE.json"),
            run_json: existing_relative_path(output_dir, "00_summary/RUN.json"),
            checks_status_json: existing_relative_path(output_dir, "checks-status.json"),
            pr_review_md: existing_relative_path(output_dir, "PR_REVIEW.md"),
            report_json: existing_relative_path(output_dir, "report.json"),
        }
    }
}

fn existing_relative_path(output_dir: &Path, relative: &str) -> Option<String> {
    output_dir
        .join(relative)
        .exists()
        .then(|| relative.to_string())
}

/// Decode a pack's `MERGE_GATE.json` into the CLI summary surface.
///
/// Fail-loud: a missing, unreadable, or unparsable artifact is an error, and so
/// is a pack whose `schema_version` this build does not know. Everything the
/// reader had to normalize instead of read is reported back as a caveat.
fn read_merge_gate_summary(output_dir: &Path) -> anyhow::Result<MergeGateSummary> {
    use anyhow::Context;

    let gate_path = output_dir.join("00_summary").join("MERGE_GATE.json");
    let raw = std::fs::read_to_string(&gate_path).with_context(|| {
        format!(
            "cannot read the merge gate artifact {} — the run produced no readable verdict",
            gate_path.display()
        )
    })?;
    let value: Value = serde_json::from_str(&raw).with_context(|| {
        format!(
            "failed to parse merge gate artifact {}",
            gate_path.display()
        )
    })?;

    let mut caveats = Vec::new();
    if let Some(caveat) = crate::gate::check_merge_gate_schema_field(value.get("schema_version"))
        .with_context(|| format!("merge gate artifact {}", gate_path.display()))?
    {
        caveats.push(caveat);
    }

    // Legacy root-as-decision vs. mandatory `decision` object: one rule, shared
    // with the MCP adapter, because the two readers answering it differently is
    // exactly how the same pack became readable from one surface and corrupt
    // from the other.
    let decision = crate::gate::select_decision_object(&value).map_err(|shape| {
        anyhow::anyhow!(
            "merge gate artifact {} {} — the pack is corrupt and no verdict can be read from it",
            gate_path.display(),
            shape.describe(),
        )
    })?;
    // A decision object that states NO decision is not a decision with every
    // signal missing — it is the same corrupt pack the other two readers
    // already refuse. `tools/validate_merge_gate.py` rejects it for its missing
    // required fields, the MCP adapter returns `storage_corrupt`, and
    // `prview gate` cannot deserialize it; only this reader used to normalize
    // it to BLOCK and publish a summary, so a truncated artifact came back as a
    // clean `--ci` exit 1 with a verdict the pack never gave. Presence is the
    // test, not recognizability: a stated verdict outside the vocabulary IS a
    // decision, and the contract has it collapse to BLOCK with a caveat.
    if !["verdict", "merge_recommendation", "allow_merge"]
        .iter()
        .any(|field| decision.get(*field).is_some())
    {
        anyhow::bail!(
            "merge gate artifact {} states no verdict, merge_recommendation or allow_merge — \
             the pack is corrupt and no verdict can be read from it",
            gate_path.display(),
        );
    }

    // A decision signal present with the WRONG JSON type is not an absent one.
    // Reading each through `as_str()` / `as_bool()` collapsed the two, and
    // "absent" is the state this reader forgives: `merge_recommendation: 7`
    // became "no recommendation", the fallback below then reconstructed
    // `Approve` from `allow_merge`, and `--ci` exited 0 on a pack whose
    // decision this reader had silently failed to read. The MCP adapter has
    // named such a field since the unreadable-signal contract landed; the CLI
    // half of that contract was documented but never implemented.
    let mut unreadable = Vec::new();
    let raw_verdict = crate::gate::readable_signal(
        "verdict",
        decision.get("verdict"),
        crate::gate::JsonKind::String,
        &mut unreadable,
    )
    .and_then(Value::as_str);
    let raw_allow_merge = crate::gate::readable_signal(
        "allow_merge",
        decision.get("allow_merge"),
        crate::gate::JsonKind::Boolean,
        &mut unreadable,
    )
    .and_then(Value::as_bool);
    let raw_recommendation = crate::gate::readable_signal(
        "merge_recommendation",
        decision.get("merge_recommendation"),
        crate::gate::JsonKind::String,
        &mut unreadable,
    )
    .and_then(Value::as_str);
    // Read as an OPTION, not through `unwrap_or(false)`: the reconciliation
    // below has to tell a pack that states a failed quality axis from one
    // written before the field existed. It goes through `readable_signal` for
    // the third case those two hide between them — a `quality_pass` that is
    // PRESENT but not a boolean. Reading it with a bare `as_bool()` made
    // `"false"` indistinguishable from absent and published an approval with no
    // caveat at all; a stated-but-unreadable axis now normalizes to BLOCK like
    // every other one.
    let raw_quality_pass = crate::gate::readable_signal(
        "quality_pass",
        decision.get("quality_pass"),
        crate::gate::JsonKind::Boolean,
        &mut unreadable,
    )
    .and_then(Value::as_bool);
    // The confidence axis. The contract permits `PASS` only when the analysis is
    // `complete`, so this ranks beside the policy axes instead of being read
    // afterwards for display — which is what let an `incomplete` run publish a
    // clean approval.
    let raw_analysis_status = crate::gate::readable_signal(
        "analysis_status",
        decision.get("analysis_status"),
        crate::gate::JsonKind::String,
        &mut unreadable,
    )
    .and_then(Value::as_str);
    // The blocker axis, stated twice by the emitter:
    // `policy_allow_merge = blocking_issues.is_empty()`. A pack may carry either
    // or both, so both are read; agreeing on the same rank costs nothing and a
    // pack that states only one is still covered.
    let raw_policy_allow_merge = crate::gate::readable_signal(
        "policy_allow_merge",
        decision.get("policy_allow_merge"),
        crate::gate::JsonKind::Boolean,
        &mut unreadable,
    )
    .and_then(Value::as_bool);
    let raw_blocking_issues = crate::gate::readable_signal(
        "blocking_issues",
        decision.get("blocking_issues"),
        crate::gate::JsonKind::Array,
        &mut unreadable,
    )
    .and_then(Value::as_array);
    // Whether the verdict below is what the pack said or what this reader had to
    // substitute for it. A substituted verdict cannot leave the OTHER decision
    // axes reading whatever the same unreliable decision block claimed: that
    // published `verdict: "BLOCK"` beside `allow_merge: true` and an `approve`
    // recommendation, breaking the `allow_merge == (verdict == "PASS")`
    // invariant and letting `compute_exit_code` exit 0 on a BLOCK. An ignored
    // signal anywhere in the block earns the same treatment: a decision derived
    // from a partly unread block is not a decision this reader may publish as
    // permissive.
    let mut normalized_to_block = !unreadable.is_empty();
    let verdict_is_mistyped = decision.get("verdict").is_some() && raw_verdict.is_none();
    caveats.append(&mut unreadable);
    // The vocabulary itself lives in `gate::canonical_verdict`, shared with the
    // MCP adapter. This reader owning a second copy of it is exactly how the
    // two surfaces came to read one pack two ways.
    let verdict = match raw_verdict.map(|raw| (raw, crate::gate::canonical_verdict(raw))) {
        Some((_, Some(canonical))) => canonical,
        // Collapsing an unreadable verdict to BLOCK is the safe default, but it
        // is a normalization, not a reading — say so instead of letting the
        // caller mistake it for what the pack claimed.
        Some((other, None)) => {
            caveats.push(format!(
                "unknown_verdict: MERGE_GATE.json verdict `{other}` is not in the \
                 PASS/CONDITIONAL/BLOCK vocabulary; normalized to BLOCK"
            ));
            normalized_to_block = true;
            "BLOCK"
        }
        None => {
            // A verdict that IS there but could not be typed has already been
            // named by its `unreadable_verdict:` caveat; saying the decision
            // "carries no verdict" on top of that would be a second, false
            // claim about the same field.
            if !verdict_is_mistyped {
                caveats.push(
                    "unknown_verdict: MERGE_GATE.json decision carries no `verdict`; \
                     normalized to BLOCK"
                        .to_string(),
                );
            }
            normalized_to_block = true;
            "BLOCK"
        }
    };

    let reason = decision
        .get("reason")
        .or_else(|| decision.get("decision_reason"))
        .and_then(Value::as_str)
        .map(|s| s.to_string());

    // An unrecognized recommendation is not an absent one either. It cannot rank,
    // so it drops out of the reconciliation below — but it is named, exactly as
    // the MCP adapter names it, instead of vanishing into a confident surface
    // derived from the remaining signals.
    let recommendation_rank = raw_recommendation.and_then(crate::gate::rank_from_merge_rec);
    if let Some(raw) = raw_recommendation
        && recommendation_rank.is_none()
    {
        caveats.push(format!(
            "unknown_merge_recommendation: MERGE_GATE.json merge_recommendation `{raw}` is not in \
             the approve/review_required/block vocabulary; it was ignored when deriving this \
             decision"
        ));
    }

    // Conservativeness reconciliation, shared with the MCP adapter through
    // `gate::rank_from_*`: the most conservative axis the pack states wins, and
    // every axis is then published from that one rank. Believing each field in
    // turn let a pack that says `verdict: "BLOCK"` beside
    // `merge_recommendation: "approve"` publish an approval — and, because
    // `compute_exit_code` keys off the recommendation, exit 0 on a gate whose
    // own canonical artifact said BLOCK.
    let allow_rank = raw_allow_merge.map(|allow| if allow { 1 } else { 2 });
    // `quality_pass` is a decision axis too, and only its FALSE is informative.
    // `false` says "not a PASS" — the contract permits `PASS` only when quality
    // passes — so it ranks 2, exactly like `allow_merge: false`. `true` states
    // no rank: a quality-clean run is still held at CONDITIONAL by a
    // breaking-change escalation, so reading it as an assertion that the gate
    // passed would let one axis soften a verdict two others agree on. Absence
    // states nothing either — that is the shape of a pack written before the
    // field, and defaulting it to `false` would turn every one of them
    // CONDITIONAL.
    let quality_rank = match raw_quality_pass {
        Some(false) => Some(2),
        _ => None,
    };
    // The confidence axis, ranked by the same rule: only the values that RULE
    // OUT a more permissive outcome state a rank. `complete` rules nothing out
    // — it is a precondition of `PASS`, not a grant of it — so like
    // `quality_pass: true` it stays silent.
    let analysis_rank = raw_analysis_status.and_then(crate::gate::rank_from_analysis_status);
    if let Some(raw) = raw_analysis_status
        && !crate::gate::known_analysis_status(raw)
    {
        caveats.push(format!(
            "unknown_analysis_status: MERGE_GATE.json analysis_status `{raw}` is not in the \
             complete/degraded/incomplete vocabulary; it was ignored when deriving this decision"
        ));
    }
    // The blocker axis. `blocking_issues` is non-empty only when a check reached
    // `PolicyConclusion::Blocked`, whose `merge_impact` is `Block`, so a stated
    // blocker is a stated BLOCK — rank 3. `policy_allow_merge: false` is the
    // same fact by its own definition. Neither states anything in the permissive
    // direction: an empty list and `policy_allow_merge: true` mean only "policy
    // did not hard-block", which the contract is explicit is NOT the same as
    // `allow_merge`.
    let blocker_rank = (raw_policy_allow_merge == Some(false)
        || raw_blocking_issues.is_some_and(|issues| !issues.is_empty()))
    .then_some(3);
    let stated_ranks: Vec<u8> = [
        crate::gate::rank_from_verdict(verdict),
        recommendation_rank,
        allow_rank,
        quality_rank,
        analysis_rank,
        blocker_rank,
    ]
    .into_iter()
    .flatten()
    .collect();
    let final_rank = if normalized_to_block {
        3
    } else {
        stated_ranks.iter().copied().max().unwrap_or(3)
    };
    // Only the PACK's own axes can be inconsistent with each other. A verdict
    // this reader had to substitute is already named by its own caveat, and
    // calling the substitution an inconsistency would blame the artifact for
    // the reader's normalization.
    //
    // `allow_merge` is deliberately NOT one of the axes compared here, though it
    // still raises the rank above. `false` says "not a PASS" — it is `>= 2`, not
    // `== 2`, and cannot reach 3 at all — so comparing it as an exact rank
    // called every healthy BLOCK pack (`verdict: "BLOCK"`,
    // `merge_recommendation: "block"`, `allow_merge: false`) inconsistent with
    // itself, which is every BLOCK pack this tool writes. It contradicts the
    // decision only when the derived `allow_merge` disagrees with the stated
    // one, which is the test the MCP adapter already used.
    //
    // `quality_pass` needs no test of its own. It ranks 2 when false, so any
    // axis claiming 1 beside it already disagrees with the winning rank and
    // fires above; and a healthy BLOCK or CONDITIONAL pack states
    // `quality_pass: false` in agreement with everything else. A separate
    // `quality_pass == Some(false) && final_rank == 1` guard would be
    // unreachable — the rank it contributes is what makes `final_rank == 1`
    // impossible. It is still NAMED in the caveat, so a reader can see which
    // axis forced the downgrade.
    let textual_ranks = [crate::gate::rank_from_verdict(verdict), recommendation_rank];
    let axes_disagree = textual_ranks
        .iter()
        .flatten()
        .any(|rank| *rank != final_rank)
        || raw_allow_merge.is_some_and(|allow| allow != (final_rank == 1));
    if !normalized_to_block && axes_disagree {
        caveats.push(format!(
            "core_inconsistency: MERGE_GATE.json states verdict={verdict}, \
             merge_recommendation={}, allow_merge={}, quality_pass={}, analysis_status={}, \
             blocking_issues={}, policy_allow_merge={}; the most conservative signal wins",
            raw_recommendation.unwrap_or("null"),
            raw_allow_merge
                .map(|b| b.to_string())
                .unwrap_or_else(|| "null".to_string()),
            raw_quality_pass
                .map(|b| b.to_string())
                .unwrap_or_else(|| "null".to_string()),
            raw_analysis_status.unwrap_or("null"),
            raw_blocking_issues
                .map(|issues| issues.len().to_string())
                .unwrap_or_else(|| "null".to_string()),
            raw_policy_allow_merge
                .map(|b| b.to_string())
                .unwrap_or_else(|| "null".to_string()),
        ));
    }

    let verdict = crate::gate::verdict_from_rank(final_rank);
    let allow_merge = final_rank == 1;
    let merge_recommendation = match crate::gate::merge_rec_from_rank(final_rank) {
        "approve" => crate::policy::engine::MergeRecommendation::Approve,
        "review_required" => crate::policy::engine::MergeRecommendation::ReviewRequired,
        _ => crate::policy::engine::MergeRecommendation::Block,
    };

    // Ranking and PUBLISHING are different questions about the same absent
    // field. Absence states no rank — that is what keeps a pre-`quality_pass`
    // pack a PASS — but the summary still has to answer "did quality pass", and
    // answering `false` asserted a failure the pack never claimed: it derived
    // `analysis_status: Incomplete` from that, and `--ci` exited 1 on the same
    // artifact the MCP adapter approved.
    //
    // So an absent axis is derived from the RECONCILED outcome instead. The
    // contract permits `PASS` only when quality passes, so a reconciled `PASS`
    // implies it; anything held below `PASS` implies nothing about quality
    // specifically and stays conservative. This cannot launder a MISTYPED
    // value: an unreadable signal normalizes the whole decision to `BLOCK`, so
    // `allow_merge` is already false by the time it is read here.
    let quality_pass = raw_quality_pass.unwrap_or(allow_merge);

    // Reuses the value typed above, so a mistyped `analysis_status` reaches this
    // fallback as absent rather than being read a second time by a laxer rule.
    // Its absent case follows the same rule: a reconciled `PASS` requires a
    // complete analysis, and nothing below `PASS` implies one.
    let analysis_status = match raw_analysis_status {
        Some("complete") => crate::policy::engine::AnalysisStatus::Complete,
        Some("degraded") => crate::policy::engine::AnalysisStatus::Degraded,
        Some("incomplete") => crate::policy::engine::AnalysisStatus::Incomplete,
        _ if allow_merge && quality_pass => crate::policy::engine::AnalysisStatus::Complete,
        _ => crate::policy::engine::AnalysisStatus::Incomplete,
    };
    // `checks[]` sits at the pack ROOT, beside `decision`, and it is the only
    // complete list of what ran: the artifact stage appends its own signal
    // checks (`public_api_diff`, `unsafe_audit`, `ghost_refs`,
    // `heuristics_loctree`) to the list the gate is built from, and none of
    // them ever reaches the in-memory `Report` the CLI tallies.
    //
    // A status is matched against the vocabulary the writer emits, not against
    // the single string `"warnings"`. Anything else is UNREADABLE, not clean:
    // `"WARNINGS"` from another writer — or a stale pack `--update` reused
    // unchanged — used to count as "not a warning", so `--ci
    // --fail-on-warnings` exited 0 on an artifact whose warning signal this
    // reader could not read. It is the same rule the decision axes follow: a
    // present-but-untypeable signal normalizes conservatively and is reported,
    // while an ABSENT one may legitimately mean a legacy pack. Case is not
    // folded, deliberately — normalizing `"WARNINGS"` into a warning silently
    // would hide that the pack is off-contract, and the tally is the same
    // either way.
    let warned_checks = match value.get("checks") {
        Some(Value::Array(entries)) => {
            let mut warned = 0usize;
            let mut unreadable: Vec<String> = Vec::new();
            for (index, entry) in entries.iter().enumerate() {
                match entry.get("status").and_then(Value::as_str) {
                    Some("warnings") => warned += 1,
                    Some(status) if crate::checks::CheckStatus::EMITTED.contains(&status) => {}
                    _ => {
                        warned += 1;
                        unreadable.push(
                            entry
                                .get("id")
                                .and_then(Value::as_str)
                                .map(str::to_string)
                                .unwrap_or_else(|| format!("checks[{index}]")),
                        );
                    }
                }
            }
            if !unreadable.is_empty() {
                caveats.push(format!(
                    "unreadable_check_status: MERGE_GATE.json states a status outside the emitted \
                     vocabulary ({}) for {}; each one counts toward the warning tally",
                    crate::checks::CheckStatus::EMITTED.join(", "),
                    unreadable.join(", ")
                ));
            }
            warned
        }
        // The same rule one level up, on the container instead of an entry. This
        // used to fall back to "the checks this run executed", which on an
        // unchanged `--update` run is none at all: `--ci --fail-on-warnings`
        // exited 0 on a pack whose warning list the reader could not read. An
        // unreadable list is not an empty one, so it counts as at least one
        // warning. No legacy carve-out applies — `checks` has been emitted since
        // schema 1.0 and the contract validator has always required an array
        // there, so a non-array was never a valid shape.
        Some(other) => {
            caveats.push(format!(
                "unreadable_checks: MERGE_GATE.json checks is {}, not an array; the warning tally \
                 cannot be read and counts as at least one warning",
                match other {
                    Value::Null => "null",
                    Value::Bool(_) => "a boolean",
                    Value::Number(_) => "a number",
                    Value::String(_) => "a string",
                    Value::Object(_) => "an object",
                    Value::Array(_) => unreachable!("matched above"),
                }
            ));
            1
        }
        // ABSENT is the one tolerant case, and stays so: a pack that states no
        // list may simply predate this build, and the CLI's own tally still
        // applies through the `max` at the call site. Absent is not the same
        // question as present-but-unreadable.
        None => 0,
    };

    Ok(MergeGateSummary {
        verdict: verdict.to_string(),
        analysis_status,
        merge_recommendation,
        allow_merge,
        quality_pass,
        reason,
        caveats,
        warned_checks,
    })
}

fn read_context_artifact_summaries(output_dir: &Path) -> Vec<CliJsonContextArtifact> {
    let raw = match std::fs::read_to_string(output_dir.join("00_summary").join("RUN.json")) {
        Ok(raw) => raw,
        Err(_) => return Vec::new(),
    };
    let value: Value = match serde_json::from_str(&raw) {
        Ok(value) => value,
        Err(_) => return Vec::new(),
    };

    value
        .get("context_artifacts")
        .and_then(Value::as_array)
        .map(|artifacts| {
            artifacts
                .iter()
                .filter_map(|artifact| {
                    Some(CliJsonContextArtifact {
                        key: artifact.get("key")?.as_str()?.to_string(),
                        path: artifact.get("path")?.as_str()?.to_string(),
                        generated: artifact.get("generated")?.as_bool()?,
                        recommended: artifact.get("recommended")?.as_bool()?,
                        reason: artifact.get("reason")?.as_str()?.to_string(),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

fn top_failures(checks: &[CheckResult]) -> Vec<CliJsonFailure> {
    let mut failures: Vec<&CheckResult> = checks
        .iter()
        .filter(|check| matches!(check.status, CheckStatus::Failed | CheckStatus::Error))
        .collect();

    if failures.is_empty() {
        failures = checks
            .iter()
            .filter(|check| check.status == CheckStatus::Warnings)
            .collect();
    }

    failures
        .into_iter()
        .take(CLI_JSON_TOP_FAILURE_LIMIT)
        .map(|check| CliJsonFailure {
            id: check_id_from_name(&check.name),
            name: check.name.clone(),
            status: check.status.as_str().to_string(),
            summary: summarize_check_output(check),
        })
        .collect()
}

fn summarize_check_output(check: &CheckResult) -> String {
    if check.name.eq_ignore_ascii_case("cargo audit")
        && let Some(summary) = cargo_audit_cli_summary(&check.output)
    {
        return truncate_for_summary(&summary, 140);
    }

    if check.name.eq_ignore_ascii_case("semgrep scan")
        && let Some(summary) = semgrep_cli_summary(&check.output)
    {
        return truncate_for_summary(&summary, 140);
    }

    let first_line = check
        .output
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("");

    if !first_line.is_empty() {
        return truncate_for_summary(first_line, 140);
    }

    match check.status {
        CheckStatus::Warnings => "warnings present; see artifact log".to_string(),
        CheckStatus::Failed => "check failed; see artifact log".to_string(),
        CheckStatus::Error => "check errored; see artifact log".to_string(),
        CheckStatus::Passed => "passed".to_string(),
        CheckStatus::Skipped => "skipped".to_string(),
    }
}

fn semgrep_cli_summary(output: &str) -> Option<String> {
    for line in output
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
    {
        let lower = line.to_ascii_lowercase();
        if lower.contains("code findings") {
            let count: String = line.chars().filter(|ch| ch.is_ascii_digit()).collect();
            if !count.is_empty() {
                return Some(format!("{count} code findings"));
            }
            return Some("code findings reported".to_string());
        }
    }

    None
}

fn truncate_for_summary(input: &str, max_chars: usize) -> String {
    if input.chars().count() <= max_chars {
        return input.to_string();
    }

    let truncated: String = input.chars().take(max_chars.saturating_sub(1)).collect();
    format!("{truncated}…")
}

/// One logical row of the config box. Styling is applied only after a row has
/// been wrapped into physical terminal lines, so ANSI sequences never
/// participate in width calculations.
enum ConfigRow {
    Rule,
    Line {
        plain: String,
        style: ConfigLineStyle,
    },
}

#[derive(Clone, Copy)]
enum ConfigLineStyle {
    Label(&'static str),
    Plain,
    Note,
    Base,
}

const CONFIG_BOX_TITLE: &str = "PRVIEW CONFIG";
const CONFIG_BOX_MIN_INNER: usize = 64;
const CONFIG_BOX_FALLBACK_COLUMNS: usize = 100;
const CONFIG_BOX_NARROW_THRESHOLD: usize = 24;

/// Inner width (columns between the two walls) for the config box. The desired
/// width retains the historical 64-column minimum and a right margin, but is
/// capped to the actual terminal width so the terminal never wraps a wall.
fn config_box_inner_width(
    plain_lines: &[String],
    title: &str,
    terminal_columns: usize,
) -> Option<usize> {
    if terminal_columns < CONFIG_BOX_NARROW_THRESHOLD {
        return None;
    }

    let widest = plain_lines
        .iter()
        .map(|s| UnicodeWidthStr::width(s.as_str()))
        .max()
        .unwrap_or(0)
        .max(UnicodeWidthStr::width(title));
    let desired = widest.max(CONFIG_BOX_MIN_INNER - 1) + 1;
    Some(desired.min(terminal_columns.saturating_sub(2)))
}

fn config_output_columns() -> Option<usize> {
    if io::stdout().is_terminal() {
        crossterm::terminal::size()
            .map(|(columns, _)| usize::from(columns))
            .ok()
    } else {
        Some(CONFIG_BOX_FALLBACK_COLUMNS)
    }
}

fn split_at_display_width(input: &str, max_width: usize) -> (&str, &str) {
    let mut width = 0;
    let mut split = 0;

    for grapheme in input.graphemes(true) {
        let grapheme_width = UnicodeWidthStr::width(grapheme);
        if width + grapheme_width > max_width && split > 0 {
            break;
        }
        width += grapheme_width;
        split += grapheme.len();
        if width >= max_width {
            break;
        }
    }

    input.split_at(split)
}

/// Word-wrap one logical row to display columns. Long unbroken refs are split
/// on grapheme boundaries; continuation lines preserve the original indent.
fn wrap_config_line(input: &str, max_width: usize) -> Vec<String> {
    if UnicodeWidthStr::width(input) <= max_width {
        return vec![input.to_string()];
    }

    let leading: String = input.chars().take_while(|ch| *ch == ' ').collect();
    let indent = if UnicodeWidthStr::width(leading.as_str()) < max_width {
        leading
    } else {
        " ".to_string()
    };
    let indent_width = UnicodeWidthStr::width(indent.as_str());
    let content_width = max_width.saturating_sub(indent_width).max(1);
    let mut lines = Vec::new();
    let mut current = indent.clone();

    for word in input.split_whitespace() {
        let separator = if UnicodeWidthStr::width(current.as_str()) > indent_width {
            1
        } else {
            0
        };
        if UnicodeWidthStr::width(current.as_str()) + separator + UnicodeWidthStr::width(word)
            <= max_width
        {
            if separator == 1 {
                current.push(' ');
            }
            current.push_str(word);
            continue;
        }

        if UnicodeWidthStr::width(current.as_str()) > indent_width {
            lines.push(current);
            current = indent.clone();
        }

        let mut remainder = word;
        while UnicodeWidthStr::width(remainder) > content_width {
            let (chunk, rest) = split_at_display_width(remainder, content_width);
            let mut line = indent.clone();
            line.push_str(chunk);
            lines.push(line);
            remainder = rest;
        }
        current.push_str(remainder);
    }

    if UnicodeWidthStr::width(current.as_str()) > indent_width || lines.is_empty() {
        lines.push(current);
    }
    lines
}

fn style_config_line(line: &str, style: ConfigLineStyle, color: bool) -> String {
    if !color {
        return line.to_string();
    }

    match style {
        ConfigLineStyle::Label(label) => {
            let prefix = format!(" {label}:");
            if let Some(rest) = line.strip_prefix(&prefix) {
                format!(" {}:{}", label.bold(), rest)
            } else {
                line.to_string()
            }
        }
        ConfigLineStyle::Plain => line.to_string(),
        ConfigLineStyle::Note => {
            if let Some(rest) = line.strip_prefix("    note: ") {
                format!("    note: {}", rest.dimmed())
            } else {
                line.dimmed().to_string()
            }
        }
        ConfigLineStyle::Base => {
            let rendered = line.replacen('✓', &"✓".green().to_string(), 1);
            ["[remote]", "[local]"]
                .into_iter()
                .fold(rendered, |text, source| {
                    text.replace(source, &source.dimmed().to_string())
                })
        }
    }
}

fn render_unboxed_config(rows: &[ConfigRow], max_width: Option<usize>, color: bool) -> String {
    let physical_lines = |input: &str| match max_width {
        Some(width) => wrap_config_line(input, width.max(1)),
        None => vec![input.to_string()],
    };

    let mut output = String::new();
    for line in physical_lines(CONFIG_BOX_TITLE) {
        output.push_str(&line);
        output.push('\n');
    }
    for row in rows {
        match row {
            ConfigRow::Rule => output.push('\n'),
            ConfigRow::Line { plain, style } => {
                for line in physical_lines(plain) {
                    output.push_str(&style_config_line(&line, *style, color));
                    output.push('\n');
                }
            }
        }
    }
    output
}

fn render_config(rows: &[ConfigRow], terminal_columns: Option<usize>, color: bool) -> String {
    let Some(terminal_columns) = terminal_columns else {
        // A TTY whose width cannot be queried must not receive a guessed frame:
        // an oversized right wall is worse than the terminal's natural wrapping.
        return render_unboxed_config(rows, None, color);
    };

    let plain_lines: Vec<String> = rows
        .iter()
        .filter_map(|row| match row {
            ConfigRow::Line { plain, .. } => Some(plain.clone()),
            ConfigRow::Rule => None,
        })
        .collect();

    let Some(inner) = config_box_inner_width(&plain_lines, CONFIG_BOX_TITLE, terminal_columns)
    else {
        return render_unboxed_config(rows, Some(terminal_columns), color);
    };

    let heavy = "═".repeat(inner);
    let light = "─".repeat(inner);
    let paint_border = |line: String| {
        if color { line.cyan().to_string() } else { line }
    };
    let mut output = String::new();

    output.push_str(&paint_border(format!("╔{heavy}╗")));
    output.push('\n');

    let title_width = UnicodeWidthStr::width(CONFIG_BOX_TITLE);
    let left = (inner - title_width) / 2;
    let right = inner - title_width - left;
    let title_line = format!(
        "║{}{}{}║",
        " ".repeat(left),
        CONFIG_BOX_TITLE,
        " ".repeat(right)
    );
    output.push_str(&if color {
        title_line.cyan().bold().to_string()
    } else {
        title_line
    });
    output.push('\n');
    output.push_str(&paint_border(format!("╠{heavy}╣")));
    output.push('\n');

    let content_width = inner.saturating_sub(1).max(1);
    for row in rows {
        match row {
            ConfigRow::Rule => {
                output.push_str(&paint_border(format!("╟{light}╢")));
                output.push('\n');
            }
            ConfigRow::Line { plain, style } => {
                for line in wrap_config_line(plain, content_width) {
                    let width = UnicodeWidthStr::width(line.as_str());
                    let pad = " ".repeat(inner.saturating_sub(width));
                    let left_wall = paint_border("║".to_string());
                    let right_wall = paint_border("║".to_string());
                    output.push_str(&left_wall);
                    output.push_str(&style_config_line(&line, *style, color));
                    output.push_str(&pad);
                    output.push_str(&right_wall);
                    output.push('\n');
                }
            }
        }
    }

    output.push_str(&paint_border(format!("╚{heavy}╝")));
    output.push('\n');
    output
}

/// Print configuration block
pub fn print_config(config: &Config, target: &ResolvedRef, bases: &[ResolvedRef]) {
    let mut rows: Vec<ConfigRow> = Vec::new();

    rows.push(ConfigRow::Line {
        plain: format!(" Target: {}", target.name),
        style: ConfigLineStyle::Label("Target"),
    });
    let target_sha = crate::git::short_sha(&target.commit_id);
    rows.push(ConfigRow::Line {
        plain: format!("    commit: {}", target_sha),
        style: ConfigLineStyle::Plain,
    });
    let target_src = if target.is_remote { "remote" } else { "local" };
    rows.push(ConfigRow::Line {
        plain: format!("    source: {}", target_src),
        style: ConfigLineStyle::Plain,
    });

    rows.push(ConfigRow::Rule);
    let mode = describe_run_mode(config);
    rows.push(ConfigRow::Line {
        plain: format!(" Mode: {}", mode),
        style: ConfigLineStyle::Label("Mode"),
    });
    let checks = describe_enabled_steps(config);
    rows.push(ConfigRow::Line {
        plain: format!(" Checks: {}", checks),
        style: ConfigLineStyle::Label("Checks"),
    });
    if config.is_fast_remote_only_standard() {
        let note = "fast remote-only preset skips tests and heuristics; use --with-tests, --with-lint, or --deep for a heavier pass";
        rows.push(ConfigRow::Line {
            plain: format!("    note: {}", note),
            style: ConfigLineStyle::Note,
        });
    }

    rows.push(ConfigRow::Rule);
    rows.push(ConfigRow::Line {
        plain: " Bases:".to_string(),
        style: ConfigLineStyle::Label("Bases"),
    });
    for base in bases {
        let sha = crate::git::short_sha(&base.commit_id);
        let src = if base.is_remote { "remote" } else { "local" };
        rows.push(ConfigRow::Line {
            plain: format!("    ✓ {} → {} [{}]", base.name, sha, src),
            style: ConfigLineStyle::Base,
        });
    }

    println!("{}", render_config(&rows, config_output_columns(), true));
}

fn describe_run_mode(config: &Config) -> String {
    let mut labels = vec![config.execution_mode.as_str().to_string()];

    if config.remote_only {
        labels.push("remote-only".to_string());
    } else if config.remote_mode {
        labels.push("remote".to_string());
    } else if config.local_only {
        labels.push("local-only".to_string());
    }

    if config.is_fast_remote_only_standard() {
        labels.push("fast preset".to_string());
    }

    labels.join(" · ")
}

fn describe_enabled_steps(config: &Config) -> String {
    let mut steps = vec!["diff".to_string()];

    if which::which("semgrep").is_ok() {
        steps.push("semgrep".to_string());
    }
    if config.profile.has_cargo {
        steps.push("cargo-check".to_string());
    }
    if config.run_lint {
        steps.push("lint".to_string());
    }
    if config.should_run_heavy_rust_lint() {
        steps.push("rust-heavy-lint".to_string());
    }
    if config.run_tests {
        steps.push("tests".to_string());
    }
    if config.run_security {
        steps.push("security+".to_string());
    } else if config.profile.has_cargo {
        steps.push("cargo-audit".to_string());
    }
    if config.run_heuristics {
        steps.push("heuristics".to_string());
    }
    if config.run_bundle {
        steps.push("bundle".to_string());
    }

    steps.join(", ")
}

/// Print artifact directory tree based on what actually exists on disk
fn print_artifact_tree(dir: &std::path::Path) {
    let exists = |rel: &str| dir.join(rel).exists();

    // Top-level files
    if exists("review.html") {
        println!("   - review.html");
    }
    if exists("dashboard.html") {
        println!("   - dashboard.html");
    }

    // 00_summary/
    let summary_files: Vec<&str> = [
        "ARTIFACT_VERSION.txt",
        "RUN.json",
        "MANIFEST.json",
        "SANITY.json",
        "MERGE_GATE.json",
        "MERGE_GATE.md",
        "FAILURES_SUMMARY.md",
        "system_meta.txt",
        "git_meta.txt",
        "pr-metadata.txt",
        "commit-list.txt",
        "file-status.txt",
    ]
    .iter()
    .filter(|f| exists(&format!("00_summary/{f}")))
    .copied()
    .collect();

    if !summary_files.is_empty() {
        println!("   - 00_summary/");
        for (i, f) in summary_files.iter().enumerate() {
            let connector = if i == summary_files.len() - 1 {
                "└──"
            } else {
                "├──"
            };
            println!("     {connector} {f}");
        }
    }

    // 10_diff/
    let mut diff_items: Vec<String> = Vec::new();
    if exists("10_diff/full.patch") {
        diff_items.push("full.patch".to_string());
    }
    if exists("10_diff/per-commit-diffs") {
        let count = std::fs::read_dir(dir.join("10_diff/per-commit-diffs"))
            .map(|rd| rd.count())
            .unwrap_or(0);
        diff_items.push(format!("per-commit-diffs/ ({count} patches)"));
    }
    if exists("10_diff/per-file-diffs") {
        let count = std::fs::read_dir(dir.join("10_diff/per-file-diffs"))
            .map(|rd| rd.count())
            .unwrap_or(0);
        if count > 0 {
            diff_items.push(format!("per-file-diffs/ ({count} hotspots)"));
        }
    }
    if !diff_items.is_empty() {
        println!("   - 10_diff/");
        for (i, item) in diff_items.iter().enumerate() {
            let connector = if i == diff_items.len() - 1 {
                "└──"
            } else {
                "├──"
            };
            println!("     {connector} {item}");
        }
    }

    // 20_quality/
    let quality_dir = dir.join("20_quality");
    if quality_dir.exists() {
        let mut quality_items: Vec<String> = Vec::new();

        // Count per-gate result files
        let gate_count = std::fs::read_dir(&quality_dir)
            .map(|rd| {
                rd.filter_map(|e| e.ok())
                    .filter(|e| {
                        e.path().extension().is_some_and(|ext| ext == "json")
                            && e.file_name().to_string_lossy().ends_with(".result.json")
                    })
                    .count()
            })
            .unwrap_or(0);
        if gate_count > 0 {
            quality_items.push(format!("{gate_count} gate results (.result.json + .log)"));
        }

        for f in ["full-checks.log", "checks-errors.log"] {
            if quality_dir.join(f).exists() {
                quality_items.push(f.to_string());
            }
        }
        if quality_dir.join("BREAKING_CHANGES.md").exists() {
            quality_items.push("BREAKING_CHANGES.md".to_string());
        }
        if quality_dir.join("coverage-delta.txt").exists() {
            quality_items.push("coverage-delta.txt".to_string());
        }
        let pfd = quality_dir.join("per-file-diffs");
        if pfd.exists() {
            let count = std::fs::read_dir(&pfd).map(|rd| rd.count()).unwrap_or(0);
            if count > 0 {
                quality_items.push(format!("per-file-diffs/ ({count} files)"));
            }
        }

        if !quality_items.is_empty() {
            println!("   - 20_quality/");
            for (i, item) in quality_items.iter().enumerate() {
                let connector = if i == quality_items.len() - 1 {
                    "└──"
                } else {
                    "├──"
                };
                println!("     {connector} {item}");
            }
        }
    }

    // 30_context/
    let ctx_dir = dir.join("30_context");
    if ctx_dir.exists() {
        let ctx_files: Vec<String> = std::fs::read_dir(&ctx_dir)
            .into_iter()
            .flat_map(|rd| rd.filter_map(|e| e.ok()))
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        if !ctx_files.is_empty() {
            println!("   - 30_context/");
            let mut sorted = ctx_files;
            sorted.sort();
            for (i, f) in sorted.iter().enumerate() {
                let connector = if i == sorted.len() - 1 {
                    "└──"
                } else {
                    "├──"
                };
                println!("     {connector} {f}");
            }
        }
    }

    // ZIP
    if let Ok(rd) = std::fs::read_dir(dir) {
        for entry in rd.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.ends_with(".zip") {
                let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
                let size_str = if size > 1024 * 1024 {
                    format!("{:.1}M", size as f64 / (1024.0 * 1024.0))
                } else {
                    format!("{:.1}K", size as f64 / 1024.0)
                };
                println!("   - {name} ({size_str})");
            }
        }
    }
}

/// Print final summary
pub fn print_summary(report: &Report) {
    println!();
    println!(
        "{} ({})",
        "=== DONE! ===".cyan().bold(),
        format_duration(report.duration)
    );
    println!();

    println!(
        "{} Artifacts: {}",
        "ℹ".blue(),
        report.artifacts_dir.display()
    );
    print_artifact_tree(&report.artifacts_dir);

    // Show heuristics summary
    if let Some(ref h) = report.heuristics
        && let Some(ref loctree) = h.loctree
    {
        println!();
        println!(
            "{} Loctree: {} files, {} LOC",
            "ℹ".blue(),
            loctree.stats.total_files,
            loctree.stats.total_loc
        );
        if !loctree.dead_exports.is_empty() {
            println!(
                "   {} {} dead exports",
                "⚠".yellow(),
                loctree.dead_exports.len()
            );
        }
        if !loctree.cycles.is_empty() {
            println!(
                "   {} {} circular imports",
                "⚠".yellow(),
                loctree.cycles.len()
            );
        }
    }

    let gate = read_merge_gate_summary(&report.artifacts_dir);
    if let Some(heading) = failure_summary_heading(report, gate.as_ref().ok()) {
        println!();
        println!("{} {heading}", "⚠".yellow());
        for check in &report.checks {
            if check.is_failure() {
                println!(
                    "   - {} ({})",
                    check.name,
                    format!("{:?}", check.status).red()
                );
            }
        }
    }

    // Final authoritative line: the merge-gate verdict, in the same vocabulary
    // as `--json` (PV-03). Never print a bare "all checks passed" that could
    // contradict a BLOCK/CONDITIONAL gate — the stdout summary must not lie.
    match gate {
        Ok(gate) => {
            println!();
            let (icon, label) = match gate.verdict.as_str() {
                "PASS" => ("✓".green(), "PASS".green().bold()),
                "BLOCK" => ("🛑".red(), "BLOCK".red().bold()),
                _ => ("⚠".yellow(), "CONDITIONAL".yellow().bold()),
            };
            match gate.reason.as_deref() {
                Some(reason) if !reason.trim().is_empty() => {
                    println!("{icon} Verdict: {label} — {reason}");
                }
                _ => println!("{icon} Verdict: {label}"),
            }
            for caveat in &gate.caveats {
                println!("   {} {caveat}", "⚠".yellow());
            }
        }
        Err(err) => {
            // No readable gate artifact. The raw check tally is NOT a verdict —
            // announcing "all checks passed" here is exactly the guess this
            // path used to make. Name the missing truth instead; the process
            // exits 3 on the same condition.
            println!();
            println!("{} No merge-gate verdict for this run: {err}", "⚠".yellow());
        }
    }

    println!();
    println!(
        "{} Artifact pack: {}",
        "📦".blue(),
        report.artifacts_dir.display()
    );
}

fn format_duration(d: Duration) -> String {
    let secs = d.as_secs();
    if secs >= 60 {
        format!("{}m {}s", secs / 60, secs % 60)
    } else {
        format!("{}s", secs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::checks::{CheckResult, CheckStatus};
    use crate::cli::ExecutionMode;
    use crate::config::{test_config, test_rust_profile};

    /// Minimal artifact pack carrying one `MERGE_GATE.json` decision. The gate
    /// artifact is now the ONLY source of the verdict, so every summary test
    /// has to plant one instead of leaning on a re-derivation fallback.
    /// Exit code the CLI would return for a pack, with no check of its own to
    /// contribute — so the code reflects the gate artifact alone.
    fn exit_code_for(pack: &tempfile::TempDir, strict: bool) -> i32 {
        let mut config = test_config();
        if strict {
            config.execution_mode = ExecutionMode::Ci;
        }
        let report = Report {
            target: "feature/legacy".to_string(),
            bases: vec!["main".to_string()],
            diffs: vec![],
            checks: vec![],
            heuristics: None,
            artifacts_dir: pack.path().to_path_buf(),
            duration: Duration::from_secs(1),
            unchanged: false,
        };
        let summary = build_cli_json_summary(&config, &report).expect("gate artifact is readable");
        compute_exit_code(&summary, strict, false)
    }

    fn pack_with_gate(decision: &str) -> tempfile::TempDir {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(temp.path().join("00_summary")).unwrap();
        std::fs::write(
            temp.path().join("00_summary/MERGE_GATE.json"),
            format!(r#"{{"schema_version":"2.1","decision":{decision}}}"#),
        )
        .unwrap();
        temp
    }

    #[test]
    fn config_box_inner_width_fits_longest_line_and_title() {
        let rows = vec![
            " Target: fix/truth-of-findings".to_string(),
            "    commit: 6568f50".to_string(),
            // A long Checks line that previously overflowed the fixed 64-col box.
            " Checks: diff, semgrep, cargo-check, lint, rust-heavy-lint, tests, security+, heuristics"
                .to_string(),
            " Bases:".to_string(),
        ];
        let inner = config_box_inner_width(&rows, CONFIG_BOX_TITLE, 200).unwrap();
        let widest = rows
            .iter()
            .map(|row| UnicodeWidthStr::width(row.as_str()))
            .max()
            .unwrap();

        // Every content line fits with a right margin, and the box never shrinks
        // below the historical minimum or the title width.
        assert!(inner > widest, "longest line must fit with a right margin");
        assert!(inner >= CONFIG_BOX_MIN_INNER);
        assert!(inner >= CONFIG_BOX_TITLE.chars().count());

        // Padding makes every content row reach exactly `inner` columns, so the
        // right wall lines up with the borders (the bug in the screenshot).
        for r in &rows {
            let width = UnicodeWidthStr::width(r.as_str());
            let pad = inner - width;
            assert_eq!(width + pad, inner);
        }
        // Centered title also fills the row exactly.
        let tw = UnicodeWidthStr::width(CONFIG_BOX_TITLE);
        let left = (inner - tw) / 2;
        let right = inner - tw - left;
        assert_eq!(left + tw + right, inner);
    }

    #[test]
    fn config_box_inner_width_floors_at_minimum() {
        let rows = vec![" Mode: standard".to_string()];
        assert_eq!(
            config_box_inner_width(&rows, CONFIG_BOX_TITLE, 200),
            Some(CONFIG_BOX_MIN_INNER)
        );
    }

    fn config_box_test_rows() -> Vec<ConfigRow> {
        vec![
            ConfigRow::Line {
                plain: " Target: feature/zażółć/e\u{301}/界/👩‍💻-and-a-very-long-unbroken-reference-name"
                    .to_string(),
                style: ConfigLineStyle::Label("Target"),
            },
            ConfigRow::Line {
                plain: "    commit: 77d1f2a".to_string(),
                style: ConfigLineStyle::Plain,
            },
            ConfigRow::Rule,
            ConfigRow::Line {
                plain: "    note: fast remote-only preset skips tests and heuristics; use --with-tests, --with-lint, or --deep for a heavier pass"
                    .to_string(),
                style: ConfigLineStyle::Note,
            },
            ConfigRow::Rule,
            ConfigRow::Line {
                plain: "    ✓ main → 2e11cc6 [remote]".to_string(),
                style: ConfigLineStyle::Base,
            },
        ]
    }

    #[test]
    fn config_box_never_exceeds_the_terminal_width() {
        let rows = config_box_test_rows();

        for columns in [20, 23, 40, 64, 80, 100, 116, 124, 160] {
            let rendered = render_config(&rows, Some(columns), false);
            for line in rendered.lines() {
                assert!(
                    UnicodeWidthStr::width(line) <= columns,
                    "{columns}-column terminal overflowed with: {line:?}"
                );
            }
        }
    }

    #[test]
    fn config_box_walls_align_after_wrapping() {
        let rows = config_box_test_rows();

        for columns in [40, 64, 80, 100, 116, 124] {
            let rendered = render_config(&rows, Some(columns), false);
            let lines: Vec<&str> = rendered.lines().collect();
            let expected_width = UnicodeWidthStr::width(lines[0]);
            assert_eq!(expected_width, columns);
            for line in lines {
                assert_eq!(
                    UnicodeWidthStr::width(line),
                    expected_width,
                    "misaligned line at {columns} columns: {line:?}"
                );
                assert!(
                    (line.starts_with('║') && line.ends_with('║'))
                        || (line.starts_with('╔') && line.ends_with('╗'))
                        || (line.starts_with('╠') && line.ends_with('╣'))
                        || (line.starts_with('╟') && line.ends_with('╢'))
                        || (line.starts_with('╚') && line.ends_with('╝'))
                );
            }
        }
    }

    #[test]
    fn config_box_uses_unboxed_fallback_when_terminal_is_too_narrow() {
        let rendered = render_config(&config_box_test_rows(), Some(20), false);

        assert!(rendered.starts_with("PRVIEW CONFIG\n"));
        assert!(rendered.contains(" Target:"));
        assert!(rendered.contains("zażółć"));
        assert!(
            rendered
                .lines()
                .all(|line| UnicodeWidthStr::width(line) <= 20)
        );
        assert!(!rendered.contains(['╔', '║', '╚']));
    }

    #[test]
    fn config_box_uses_unboxed_fallback_when_tty_width_is_unknown() {
        let rendered = render_config(&config_box_test_rows(), None, false);

        assert!(rendered.starts_with("PRVIEW CONFIG\n"));
        assert!(rendered.contains("feature/zażółć/e\u{301}/界/👩‍💻"));
        assert!(!rendered.contains(['╔', '║', '╚']));
    }

    #[test]
    fn config_box_splitter_keeps_grapheme_clusters_intact() {
        let input = "a👩‍💻e\u{301}b";
        let (first, rest) = split_at_display_width(input, 3);

        assert_eq!(first, "a👩‍💻");
        assert_eq!(rest, "e\u{301}b");
    }

    #[test]
    fn test_format_duration_seconds() {
        assert_eq!(format_duration(Duration::from_secs(5)), "5s");
        assert_eq!(format_duration(Duration::from_secs(59)), "59s");
    }

    #[test]
    fn test_format_duration_minutes() {
        assert_eq!(format_duration(Duration::from_secs(60)), "1m 0s");
        assert_eq!(format_duration(Duration::from_secs(90)), "1m 30s");
        assert_eq!(format_duration(Duration::from_secs(125)), "2m 5s");
    }

    #[test]
    fn test_report_has_failures_no_checks() {
        let report = Report {
            target: "main".to_string(),
            bases: vec!["develop".to_string()],
            diffs: vec![],
            checks: vec![],
            heuristics: None,
            artifacts_dir: PathBuf::from("."),
            duration: Duration::from_secs(1),
            unchanged: false,
        };
        assert!(!report.has_failures());
    }

    #[test]
    fn test_report_has_failures_all_passed() {
        let report = Report {
            target: "main".to_string(),
            bases: vec!["develop".to_string()],
            diffs: vec![],
            checks: vec![CheckResult {
                name: "test".to_string(),
                status: CheckStatus::Passed,
                duration: Duration::from_secs(1),
                output: String::new(),
                cached: false,
                provenance: None,
            }],
            heuristics: None,
            artifacts_dir: PathBuf::from("."),
            duration: Duration::from_secs(1),
            unchanged: false,
        };
        assert!(!report.has_failures());
    }

    #[test]
    fn test_report_has_failures_one_failed() {
        let report = Report {
            target: "main".to_string(),
            bases: vec!["develop".to_string()],
            diffs: vec![],
            checks: vec![
                CheckResult {
                    name: "test1".to_string(),
                    status: CheckStatus::Passed,
                    duration: Duration::from_secs(1),
                    output: String::new(),
                    cached: false,
                    provenance: None,
                },
                CheckResult {
                    name: "test2".to_string(),
                    status: CheckStatus::Failed,
                    duration: Duration::from_secs(1),
                    output: String::new(),
                    cached: false,
                    provenance: None,
                },
            ],
            heuristics: None,
            artifacts_dir: PathBuf::from("."),
            duration: Duration::from_secs(1),
            unchanged: false,
        };
        assert!(report.has_failures());
    }

    #[test]
    fn test_report_has_failures_one_error() {
        let report = Report {
            target: "main".to_string(),
            bases: vec!["develop".to_string()],
            diffs: vec![],
            checks: vec![CheckResult {
                name: "test".to_string(),
                status: CheckStatus::Error,
                duration: Duration::from_secs(1),
                output: String::new(),
                cached: false,
                provenance: None,
            }],
            heuristics: None,
            artifacts_dir: PathBuf::from("."),
            duration: Duration::from_secs(1),
            unchanged: false,
        };
        assert!(report.has_failures());
    }

    #[test]
    fn test_report_has_failures_warnings_ok() {
        let report = Report {
            target: "main".to_string(),
            bases: vec!["develop".to_string()],
            diffs: vec![],
            checks: vec![CheckResult {
                name: "test".to_string(),
                status: CheckStatus::Warnings,
                duration: Duration::from_secs(1),
                output: String::new(),
                cached: false,
                provenance: None,
            }],
            heuristics: None,
            artifacts_dir: PathBuf::from("."),
            duration: Duration::from_secs(1),
            unchanged: false,
        };
        assert!(!report.has_failures());
    }

    #[test]
    fn test_report_has_failures_skipped_ok() {
        let report = Report {
            target: "main".to_string(),
            bases: vec!["develop".to_string()],
            diffs: vec![],
            checks: vec![CheckResult {
                name: "test".to_string(),
                status: CheckStatus::Skipped,
                duration: Duration::from_secs(1),
                output: String::new(),
                cached: false,
                provenance: None,
            }],
            heuristics: None,
            artifacts_dir: PathBuf::from("."),
            duration: Duration::from_secs(1),
            unchanged: false,
        };
        assert!(!report.has_failures());
    }

    #[test]
    fn test_report_serialization() {
        let report = Report {
            target: "feature/test".to_string(),
            bases: vec!["main".to_string()],
            diffs: vec![],
            checks: vec![],
            heuristics: None,
            artifacts_dir: PathBuf::from("/tmp/artifacts"),
            duration: Duration::from_secs(10),
            unchanged: false,
        };
        let json = serde_json::to_string(&report).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();

        assert_eq!(value["target"], "feature/test");
        assert_eq!(value["bases"], serde_json::json!(["main"]));
        assert_eq!(value["output_dir"], "/tmp/artifacts");
    }

    #[test]
    fn test_report_deserialization() {
        let json = r#"{"target":"main","bases":["develop"],"diffs":[],"checks":[],"heuristics":null,"output_dir":"/tmp/out","duration":5.0}"#;
        let report: Report = serde_json::from_str(json).unwrap();
        assert_eq!(report.target, "main");
        assert_eq!(report.bases, vec!["develop"]);
        assert_eq!(report.artifacts_dir, PathBuf::from("/tmp/out"));
        assert_eq!(report.duration.as_secs(), 5);
    }

    #[test]
    fn test_report_deserialization_missing_output_dir_defaults_empty() {
        let json = r#"{"target":"main","bases":["develop"],"diffs":[],"checks":[],"heuristics":null,"duration":5.0}"#;
        let report: Report = serde_json::from_str(json).unwrap();
        assert_eq!(report.artifacts_dir, PathBuf::new());
    }

    #[test]
    fn test_report_clone() {
        let report = Report {
            target: "test".to_string(),
            bases: vec!["main".to_string()],
            diffs: vec![],
            checks: vec![],
            heuristics: None,
            artifacts_dir: PathBuf::from("."),
            duration: Duration::from_secs(1),
            unchanged: false,
        };
        let cloned = report.clone();
        assert_eq!(report.target, cloned.target);
        assert_eq!(report.bases, cloned.bases);
    }

    #[test]
    fn test_report_with_multiple_checks() {
        let report = Report {
            target: "main".to_string(),
            bases: vec!["develop".to_string()],
            diffs: vec![],
            checks: vec![
                CheckResult {
                    name: "check1".to_string(),
                    status: CheckStatus::Passed,
                    duration: Duration::from_secs(1),
                    output: String::new(),
                    cached: false,
                    provenance: None,
                },
                CheckResult {
                    name: "check2".to_string(),
                    status: CheckStatus::Passed,
                    duration: Duration::from_secs(2),
                    output: String::new(),
                    cached: true,
                    provenance: None,
                },
            ],
            heuristics: None,
            artifacts_dir: PathBuf::from("."),
            duration: Duration::from_secs(3),
            unchanged: false,
        };
        assert!(!report.has_failures());
        assert_eq!(report.checks.len(), 2);
    }

    #[test]
    fn test_cli_json_summary_is_compact_and_includes_artifact_paths() {
        let temp = tempfile::tempdir().unwrap();
        let mut config = test_config();
        config.execution_mode = ExecutionMode::Standard;
        config.remote_only = true;
        config.pr_number = Some(23);
        config.pr_url = Some("https://github.com/vetcoders/prview/pull/23".to_string());
        std::fs::create_dir_all(temp.path().join("00_summary")).unwrap();
        std::fs::create_dir_all(temp.path().join("10_diff")).unwrap();
        std::fs::create_dir_all(temp.path().join("20_quality")).unwrap();
        std::fs::create_dir_all(temp.path().join("30_context")).unwrap();
        std::fs::write(temp.path().join("report.json"), "{}").unwrap();
        std::fs::write(
            temp.path().join("00_summary/RUN.json"),
            r#"{
              "context_artifacts": [
                {
                  "key": "tsc_trace",
                  "path": "30_context/tsc-trace.log",
                  "generated": false,
                  "recommended": true,
                  "reason": "skipped by default in fast remote-only runs; generate when investigating because resolution-related files changed (package.json)"
                }
              ]
            }"#,
        )
        .unwrap();
        std::fs::write(
            temp.path().join("00_summary/MERGE_GATE.json"),
            r#"{"verdict":"BLOCK","allow_merge":false,"quality_pass":false}"#,
        )
        .unwrap();
        std::fs::write(temp.path().join("AI_INDEX.md"), "# AI Review Index").unwrap();
        std::fs::write(temp.path().join("PR_REVIEW.md"), "# Review").unwrap();
        std::fs::write(temp.path().join("review.html"), "<html></html>").unwrap();
        std::fs::write(temp.path().join("dashboard.html"), "<html></html>").unwrap();
        std::fs::write(temp.path().join("10_diff/full.patch"), "diff").unwrap();
        std::fs::write(temp.path().join("20_quality/full-checks.log"), "checks").unwrap();
        std::fs::write(temp.path().join("30_context/INLINE_FINDINGS.sarif"), "{}").unwrap();

        let report = Report {
            target: "feature/test".to_string(),
            bases: vec!["main".to_string()],
            diffs: vec![],
            checks: vec![
                CheckResult {
                    name: "cargo check".to_string(),
                    status: CheckStatus::Passed,
                    duration: Duration::from_secs(1),
                    output: "ok".to_string(),
                    cached: true,
                    provenance: None,
                },
                CheckResult {
                    name: "cargo test".to_string(),
                    status: CheckStatus::Failed,
                    duration: Duration::from_secs(2),
                    output: "raw failure output".to_string(),
                    cached: false,
                    provenance: None,
                },
                CheckResult {
                    name: "clippy".to_string(),
                    status: CheckStatus::Error,
                    duration: Duration::from_secs(3),
                    output: "raw error output".to_string(),
                    cached: false,
                    provenance: None,
                },
            ],
            heuristics: None,
            artifacts_dir: temp.path().to_path_buf(),
            duration: Duration::from_secs(6),
            unchanged: false,
        };

        let summary = build_cli_json_summary(&config, &report).expect("gate artifact is readable");
        let value = serde_json::to_value(&summary).unwrap();

        assert_eq!(summary.schema_version, "cli-json/v1");
        assert_eq!(summary.status, "fail");
        assert_eq!(summary.verdict, "BLOCK");
        assert!(!summary.allow_merge);
        assert!(!summary.quality_pass);
        assert_eq!(summary.duration_secs, 6.0);
        assert_eq!(summary.output_dir, temp.path().display().to_string());
        assert_eq!(summary.pr.as_ref().map(|pr| pr.number), Some(23));
        assert_eq!(summary.mode.execution_mode, "standard");
        assert!(summary.mode.remote_only);
        assert_eq!(
            summary.checks_summary,
            CliJsonChecksSummary {
                total: 3,
                passed: 1,
                failed: 2,
                warned: 0,
                skipped: 0,
                cached: 1,
                warned_in_pack: 0,
            }
        );
        assert_eq!(summary.top_failures.len(), 2);
        assert_eq!(summary.top_failures[0].id, "cargo_test");
        assert_eq!(summary.top_failures[0].name, "cargo test");
        assert_eq!(summary.top_failures[0].summary, "raw failure output");
        assert_eq!(summary.top_failures[1].status, "error");
        assert_eq!(summary.context_artifacts.len(), 1);
        assert_eq!(summary.context_artifacts[0].key, "tsc_trace");
        assert!(summary.context_artifacts[0].recommended);
        assert_eq!(
            summary.artifacts.report_json,
            Some("report.json".to_string())
        );
        assert_eq!(
            summary.artifacts.review_html,
            Some("review.html".to_string())
        );
        assert_eq!(
            summary.artifacts.dashboard_html,
            Some("dashboard.html".to_string())
        );
        assert!(value.get("diffs").is_none());
        assert!(value["checks_summary"].is_object());
        assert!(value["context_artifacts"].is_array());
        assert!(value["artifacts"]["report_json"].is_string());
        assert!(
            !serde_json::to_string(&value)
                .unwrap()
                .contains("\"output\":")
        );
    }

    #[test]
    fn test_cli_json_summary_marks_warning_runs_without_failures() {
        let config = test_config();
        let pack = pack_with_gate(
            r#"{"verdict":"CONDITIONAL","analysis_status":"degraded",
                "merge_recommendation":"review_required","allow_merge":false,
                "quality_pass":true}"#,
        );
        let report = Report {
            target: "main".to_string(),
            bases: vec!["develop".to_string()],
            diffs: vec![],
            checks: vec![CheckResult {
                name: "lint".to_string(),
                status: CheckStatus::Warnings,
                duration: Duration::from_secs(1),
                output: String::new(),
                cached: false,
                provenance: None,
            }],
            heuristics: None,
            artifacts_dir: pack.path().to_path_buf(),
            duration: Duration::from_secs(1),
            unchanged: false,
        };

        let summary = build_cli_json_summary(&config, &report).expect("gate artifact is readable");

        assert_eq!(summary.verdict, "CONDITIONAL");
        assert_eq!(summary.status, "fail");
        assert_eq!(
            summary.merge_recommendation,
            crate::policy::engine::MergeRecommendation::ReviewRequired
        );
        assert_eq!(summary.top_failures.len(), 1);
        assert_eq!(summary.top_failures[0].status, "warnings");
    }

    #[test]
    fn test_exit_code_uses_merge_gate_truth_over_raw_failed_checks() {
        let config = test_config();
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(temp.path().join("00_summary")).unwrap();
        std::fs::write(
            temp.path().join("00_summary/MERGE_GATE.json"),
            r#"{"decision":{"verdict":"PASS","allow_merge":true,"quality_pass":true}}"#,
        )
        .unwrap();

        let report = Report {
            target: "feature/preexisting".to_string(),
            bases: vec!["main".to_string()],
            diffs: vec![],
            checks: vec![CheckResult {
                name: "cargo audit".to_string(),
                status: CheckStatus::Failed,
                duration: Duration::from_secs(1),
                output: "pre-existing advisory".to_string(),
                cached: false,
                provenance: None,
            }],
            heuristics: None,
            artifacts_dir: temp.path().to_path_buf(),
            duration: Duration::from_secs(1),
            unchanged: false,
        };

        let summary = build_cli_json_summary(&config, &report).expect("gate artifact is readable");
        assert_eq!(summary.status, "ok");
        assert_eq!(compute_exit_code(&summary, false, false), 0);
    }

    #[test]
    fn test_exit_code_non_ci_is_lenient_on_advisory_quality_failure() {
        // PV-04 variant A: outside CI, a non-blocking (warn-severity) check
        // failure is a review-required advisory — the status is still "fail",
        // but the process exits 0 because only a hard Block fails a non-CI run.
        let config = test_config();
        let pack = pack_with_gate(
            r#"{"verdict":"CONDITIONAL","analysis_status":"complete",
                "merge_recommendation":"review_required","allow_merge":false,
                "quality_pass":false}"#,
        );
        let report = Report {
            target: "feature/broken".to_string(),
            bases: vec!["main".to_string()],
            diffs: vec![],
            checks: vec![CheckResult {
                name: "cargo test".to_string(),
                status: CheckStatus::Failed,
                duration: Duration::from_secs(1),
                output: "failed".to_string(),
                cached: false,
                provenance: None,
            }],
            heuristics: None,
            artifacts_dir: pack.path().to_path_buf(),
            duration: Duration::from_secs(1),
            unchanged: false,
        };

        let summary = build_cli_json_summary(&config, &report).expect("gate artifact is readable");
        assert_eq!(summary.status, "fail");
        assert_eq!(
            summary.merge_recommendation,
            crate::policy::engine::MergeRecommendation::ReviewRequired
        );
        assert_eq!(compute_exit_code(&summary, false, false), 0);
    }

    #[test]
    fn test_exit_code_ci_mode_is_strict_on_quality_failure() {
        // CI keeps the strict contract: the same advisory failure that a plain
        // run tolerates fails the process under --ci.
        let mut config = test_config();
        config.execution_mode = ExecutionMode::Ci;
        let pack = pack_with_gate(
            r#"{"verdict":"CONDITIONAL","analysis_status":"complete",
                "merge_recommendation":"review_required","allow_merge":false,
                "quality_pass":false}"#,
        );
        let report = Report {
            target: "feature/broken".to_string(),
            bases: vec!["main".to_string()],
            diffs: vec![],
            checks: vec![CheckResult {
                name: "cargo test".to_string(),
                status: CheckStatus::Failed,
                duration: Duration::from_secs(1),
                output: "failed".to_string(),
                cached: false,
                provenance: None,
            }],
            heuristics: None,
            artifacts_dir: pack.path().to_path_buf(),
            duration: Duration::from_secs(1),
            unchanged: false,
        };

        let summary = build_cli_json_summary(&config, &report).expect("gate artifact is readable");
        assert_eq!(summary.mode.execution_mode, "ci");
        assert_eq!(compute_exit_code(&summary, true, false), 1);
    }

    #[test]
    fn test_exit_code_ci_warnings_only_passes_unless_opted_in() {
        // Warning→failure P0: a run whose only signal is a warning-level check
        // has `quality_pass == true`, so --ci exits 0. `--fail-on-warnings` is
        // the opt-in escape hatch that restores the old exit 1.
        let mut config = test_config();
        config.execution_mode = ExecutionMode::Ci;
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(temp.path().join("00_summary")).unwrap();
        std::fs::write(
            temp.path().join("00_summary/MERGE_GATE.json"),
            r#"{"decision":{"verdict":"CONDITIONAL","merge_recommendation":"review_required","allow_merge":false,"quality_pass":true}}"#,
        )
        .unwrap();

        let report = Report {
            target: "feature/warnings-only".to_string(),
            bases: vec!["main".to_string()],
            diffs: vec![],
            checks: vec![CheckResult {
                name: "Cargo audit".to_string(),
                status: CheckStatus::Warnings,
                duration: Duration::from_secs(1),
                output: "1 unmaintained crate".to_string(),
                cached: false,
                provenance: None,
            }],
            heuristics: None,
            artifacts_dir: temp.path().to_path_buf(),
            duration: Duration::from_secs(1),
            unchanged: false,
        };

        let summary = build_cli_json_summary(&config, &report).expect("gate artifact is readable");
        assert_eq!(summary.mode.execution_mode, "ci");
        assert_eq!(summary.checks_summary.warned, 1);
        assert!(summary.quality_pass);
        assert_eq!(compute_exit_code(&summary, true, false), 0);
        assert_eq!(compute_exit_code(&summary, true, true), 1);
    }

    #[test]
    fn test_fail_on_warnings_is_scoped_to_ci() {
        // Outside --ci the exit stays derived from the merge recommendation
        // alone; the escape hatch never silently hardens a plain local run.
        let config = test_config();
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(temp.path().join("00_summary")).unwrap();
        std::fs::write(
            temp.path().join("00_summary/MERGE_GATE.json"),
            r#"{"decision":{"verdict":"CONDITIONAL","merge_recommendation":"review_required","allow_merge":false,"quality_pass":true}}"#,
        )
        .unwrap();
        let report = Report {
            target: "feature/warnings-only".to_string(),
            bases: vec!["main".to_string()],
            diffs: vec![],
            checks: vec![CheckResult {
                name: "Cargo audit".to_string(),
                status: CheckStatus::Warnings,
                duration: Duration::from_secs(1),
                output: "1 unmaintained crate".to_string(),
                cached: false,
                provenance: None,
            }],
            heuristics: None,
            artifacts_dir: temp.path().to_path_buf(),
            duration: Duration::from_secs(1),
            unchanged: false,
        };

        let summary = build_cli_json_summary(&config, &report).expect("gate artifact is readable");
        assert_ne!(summary.mode.execution_mode, "ci");
        assert_eq!(compute_exit_code(&summary, false, true), 0);
    }

    #[test]
    fn the_preset_label_does_not_decide_ci_strictness() {
        // `--update` outranks `--ci` when the preset is resolved, so a
        // `--ci --fail-on-warnings --update` run publishes
        // `execution_mode: "update"`. Deriving strictness from that label made
        // the flag clap had just insisted on `--ci` for silently inert, and took
        // the `!quality_pass` exit `--ci` promises down with it.
        let mut config = test_config();
        config.execution_mode = ExecutionMode::Update;
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(temp.path().join("00_summary")).unwrap();
        std::fs::write(
            temp.path().join("00_summary/MERGE_GATE.json"),
            r#"{"decision":{"verdict":"CONDITIONAL","merge_recommendation":"review_required","allow_merge":false,"quality_pass":true},
                "checks":[{"id":"rustfmt","status":"warnings"}]}"#,
        )
        .unwrap();
        let report = Report {
            target: "feature/update-strictness".to_string(),
            bases: vec!["main".to_string()],
            diffs: vec![],
            checks: vec![],
            heuristics: None,
            artifacts_dir: temp.path().to_path_buf(),
            duration: Duration::from_secs(1),
            unchanged: true,
        };

        let summary = build_cli_json_summary(&config, &report).expect("gate artifact is readable");
        assert_eq!(summary.mode.execution_mode, "update");
        assert_eq!(summary.checks_summary.warned_in_pack, 1);
        assert_eq!(
            compute_exit_code(&summary, true, true),
            1,
            "the caller asked for --ci, so the pack's warning fails the run"
        );
        assert_eq!(
            compute_exit_code(&summary, false, true),
            0,
            "without --ci the escape hatch stays inert, preset or not"
        );
    }

    #[test]
    fn test_exit_code_block_recommendation_always_fails() {
        // A hard Block fails the process regardless of mode (block → != 0).
        let config = test_config();
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(temp.path().join("00_summary")).unwrap();
        std::fs::write(
            temp.path().join("00_summary/MERGE_GATE.json"),
            r#"{"decision":{"verdict":"BLOCK","merge_recommendation":"block","allow_merge":false,"quality_pass":true}}"#,
        )
        .unwrap();

        let report = Report {
            target: "feature/blocked".to_string(),
            bases: vec!["main".to_string()],
            diffs: vec![],
            checks: vec![],
            heuristics: None,
            artifacts_dir: temp.path().to_path_buf(),
            duration: Duration::from_secs(1),
            unchanged: false,
        };

        let summary = build_cli_json_summary(&config, &report).expect("gate artifact is readable");
        assert_eq!(
            summary.merge_recommendation,
            crate::policy::engine::MergeRecommendation::Block
        );
        assert_eq!(compute_exit_code(&summary, false, false), 1);
    }

    #[test]
    fn test_cli_json_summary_canonicalizes_cargo_audit_failure_summary() {
        let config = test_config();
        let pack = pack_with_gate(
            r#"{"verdict":"BLOCK","analysis_status":"complete",
                "merge_recommendation":"block","allow_merge":false,"quality_pass":false}"#,
        );
        let report = Report {
            target: "feature/security".to_string(),
            bases: vec!["main".to_string()],
            diffs: vec![],
            checks: vec![CheckResult {
                name: "cargo audit".to_string(),
                status: CheckStatus::Failed,
                duration: Duration::from_secs(1),
                output: r#"{
  "vulnerabilities": {
    "found": true,
    "count": 2,
    "list": [
      {
        "advisory": {
          "id": "RUSTSEC-2024-0001",
          "title": "Unsound transmute in example crate"
        },
        "package": {
          "name": "example-crate",
          "version": "0.3.1"
        },
        "versions": {
          "patched": [">=0.3.2"]
        }
      },
      {
        "advisory": {
          "id": "RUSTSEC-2024-0002",
          "title": "Use-after-free in example crate"
        },
        "package": {
          "name": "other-crate",
          "version": "1.4.0"
        },
        "versions": {
          "patched": [">=1.4.1"]
        }
      }
    ]
  }
}"#
                .to_string(),
                cached: false,
                provenance: None,
            }],
            heuristics: None,
            artifacts_dir: pack.path().to_path_buf(),
            duration: Duration::from_secs(1),
            unchanged: false,
        };

        let summary = build_cli_json_summary(&config, &report).expect("gate artifact is readable");
        let failure = &summary.top_failures[0];

        assert_eq!(failure.id, "cargo_audit");
        assert_eq!(
            failure.summary,
            "2 security advisories affecting 2 locked dependencies (RUSTSEC-2024-0001, RUSTSEC-2024-0002)"
        );
        assert!(!failure.summary.contains("\"vulnerabilities\""));
    }

    #[test]
    fn test_cli_json_summary_canonicalizes_semgrep_failure_summary() {
        let config = test_config();
        let pack = pack_with_gate(
            r#"{"verdict":"BLOCK","analysis_status":"complete",
                "merge_recommendation":"block","allow_merge":false,"quality_pass":false}"#,
        );
        let report = Report {
            target: "feature/security".to_string(),
            bases: vec!["main".to_string()],
            diffs: vec![],
            checks: vec![CheckResult {
                name: "Semgrep scan".to_string(),
                status: CheckStatus::Failed,
                duration: Duration::from_secs(1),
                output: r#"
┌───────────────────┐
│ 149 Code Findings │
└───────────────────┘

api-router/app/core/cache.py
"#
                .to_string(),
                cached: false,
                provenance: None,
            }],
            heuristics: None,
            artifacts_dir: pack.path().to_path_buf(),
            duration: Duration::from_secs(1),
            unchanged: false,
        };

        let summary = build_cli_json_summary(&config, &report).expect("gate artifact is readable");
        let failure = &summary.top_failures[0];

        assert_eq!(failure.id, "semgrep_scan");
        assert_eq!(failure.summary, "149 code findings");
        assert!(!failure.summary.contains("┌"));
    }

    #[test]
    fn artifact_consistency_explicit_incomplete_status_stays_incomplete() {
        let temp = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(temp.path().join("00_summary")).expect("summary dir");
        std::fs::write(
            temp.path().join("00_summary").join("MERGE_GATE.json"),
            r#"{
  "decision": {
    "verdict": "PASS",
    "analysis_status": "incomplete",
    "merge_recommendation": "approve",
    "allow_merge": true,
    "quality_pass": true
  }
}"#,
        )
        .expect("write gate");

        let gate = read_merge_gate_summary(temp.path()).expect("gate");

        assert_eq!(
            gate.analysis_status,
            crate::policy::engine::AnalysisStatus::Incomplete
        );
    }

    #[test]
    fn missing_merge_gate_is_an_error_not_a_re_derived_verdict() {
        // The removed `fallback_merge_gate_summary` published
        // `allow_merge = rec != Block`, so a re-derived CONDITIONAL run came back
        // with `allow_merge: true` — the one place the
        // `allow_merge == (verdict == "PASS")` invariant could be violated. An
        // unreadable pack must now fail loud instead.
        let config = test_config();
        let empty = tempfile::tempdir().unwrap();
        let report = Report {
            target: "feature/no-gate".to_string(),
            bases: vec!["main".to_string()],
            diffs: vec![],
            checks: vec![CheckResult {
                name: "cargo test".to_string(),
                status: CheckStatus::Failed,
                duration: Duration::from_secs(1),
                output: "failed".to_string(),
                cached: false,
                provenance: None,
            }],
            heuristics: None,
            artifacts_dir: empty.path().to_path_buf(),
            duration: Duration::from_secs(1),
            unchanged: false,
        };

        let err = build_cli_json_summary(&config, &report)
            .expect_err("a pack with no MERGE_GATE.json carries no verdict");
        assert!(
            format!("{err:#}").contains("MERGE_GATE.json"),
            "error must name the missing artifact: {err:#}"
        );
    }

    #[test]
    fn unparsable_merge_gate_is_an_error() {
        let config = test_config();
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(temp.path().join("00_summary")).unwrap();
        std::fs::write(temp.path().join("00_summary/MERGE_GATE.json"), "{not json").unwrap();
        let report = Report {
            target: "feature/corrupt".to_string(),
            bases: vec!["main".to_string()],
            diffs: vec![],
            checks: vec![],
            heuristics: None,
            artifacts_dir: temp.path().to_path_buf(),
            duration: Duration::from_secs(1),
            unchanged: false,
        };

        assert!(build_cli_json_summary(&config, &report).is_err());
    }

    #[test]
    fn unknown_verdict_collapses_to_block_with_an_explicit_caveat() {
        // Collapsing to BLOCK is safe, but the reader must say it normalized
        // rather than let the caller read it as the pack's own verdict.
        let pack = pack_with_gate(r#"{"verdict":"PROBABLY","allow_merge":false}"#);
        let gate = read_merge_gate_summary(pack.path()).expect("gate is readable");

        assert_eq!(gate.verdict, "BLOCK");
        let caveat = gate
            .caveats
            .iter()
            .find(|c| c.starts_with("unknown_verdict:"))
            .expect("unknown_verdict caveat present");
        assert!(caveat.contains("PROBABLY"), "{caveat}");
    }

    #[test]
    fn legacy_verdict_synonyms_are_folded_without_a_caveat() {
        // `allow_merge` matches each verdict here on purpose: `ALLOW` means a
        // clean pass, so pairing it with `allow_merge: false` would be a
        // contradictory pack, and a contradictory pack is supposed to earn a
        // `core_inconsistency` caveat. What this test pins is the synonym
        // folding, not the reconciliation.
        for (legacy, unified, allow) in [("ALLOW", "PASS", true), ("HOLD", "CONDITIONAL", false)] {
            let pack = pack_with_gate(&format!(
                r#"{{"verdict":"{legacy}","allow_merge":{allow}}}"#
            ));
            let gate = read_merge_gate_summary(pack.path()).expect("gate is readable");
            assert_eq!(gate.verdict, unified);
            assert!(
                gate.caveats.is_empty(),
                "legacy `{legacy}` is recognized vocabulary: {:?}",
                gate.caveats
            );
        }
    }

    #[test]
    fn both_readers_agree_on_every_verdict_spelling() {
        // The two surfaces used to carry two vocabularies for one field: the CLI
        // matched the raw string case-sensitively while the MCP adapter ranked
        // it through an ASCII-uppercase fold. A pack saying `verdict: "pass"`
        // was therefore a clean PASS to MCP automation and an unknown verdict
        // normalized to BLOCK on the CLI — the same artifact, approved by one
        // reader and rejected by the other. `APPROVE` diverged the same way,
        // case aside: it ranked as a pass but was not in the CLI's fold.
        for (spelling, expected) in [
            ("PASS", "PASS"),
            ("pass", "PASS"),
            ("Pass", "PASS"),
            ("ALLOW", "PASS"),
            ("allow", "PASS"),
            ("APPROVE", "PASS"),
            ("CONDITIONAL", "CONDITIONAL"),
            ("conditional", "CONDITIONAL"),
            ("HOLD", "CONDITIONAL"),
            ("hold", "CONDITIONAL"),
            ("BLOCK", "BLOCK"),
            ("block", "BLOCK"),
        ] {
            let pack = pack_with_gate(&format!(
                r#"{{"verdict":"{spelling}","merge_recommendation":"{rec}",
                     "allow_merge":{allow},"quality_pass":true,
                     "analysis_status":"complete"}}"#,
                rec = match expected {
                    "PASS" => "approve",
                    "CONDITIONAL" => "review_required",
                    _ => "block",
                },
                allow = expected == "PASS",
            ));

            let cli = read_merge_gate_summary(pack.path()).expect("gate is readable");
            let mcp = crate::mcp::read::read_decision(pack.path()).expect("gate is readable");

            assert_eq!(
                cli.verdict, expected,
                "CLI read `{spelling}` as {}: {:?}",
                cli.verdict, cli.caveats
            );
            assert_eq!(
                mcp.verdict, expected,
                "MCP read `{spelling}` as {}",
                mcp.verdict
            );
            assert_eq!(
                cli.verdict, mcp.verdict,
                "the two readers must not disagree about `{spelling}`"
            );
            assert!(
                !cli.caveats
                    .iter()
                    .any(|c| c.starts_with("unknown_verdict:")),
                "`{spelling}` is recognized vocabulary, not an unknown verdict: {:?}",
                cli.caveats
            );
        }
    }

    #[test]
    fn unknown_schema_major_fails_loud_and_newer_minor_only_caveats() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(temp.path().join("00_summary")).unwrap();
        std::fs::write(
            temp.path().join("00_summary/MERGE_GATE.json"),
            r#"{"schema_version":"9.0","decision":{"verdict":"PASS","allow_merge":true}}"#,
        )
        .unwrap();
        let err = read_merge_gate_summary(temp.path()).expect_err("unknown major must fail loud");
        assert!(format!("{err:#}").contains("9.0"), "{err:#}");

        std::fs::write(
            temp.path().join("00_summary/MERGE_GATE.json"),
            r#"{"schema_version":"2.9","decision":{"verdict":"PASS","allow_merge":true,"quality_pass":true}}"#,
        )
        .unwrap();
        let gate = read_merge_gate_summary(temp.path()).expect("newer minor is readable");
        assert_eq!(gate.verdict, "PASS");
        assert!(
            gate.caveats
                .iter()
                .any(|c| c.starts_with("schema_forward_compat:")),
            "caveats: {:?}",
            gate.caveats
        );
    }

    #[test]
    fn non_string_schema_version_fails_loud_instead_of_reading_as_legacy() {
        // `and_then(Value::as_str)` collapsed a present-but-wrongly-typed field
        // to `None`, which the schema checker reads as "pre-2.1 pack, accept
        // silently". A pack that states a version this reader cannot even type
        // is exactly the case fail-loud exists for.
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(temp.path().join("00_summary")).unwrap();
        for bad in [
            r#"{"schema_version":2.1,"decision":{"verdict":"PASS","allow_merge":true}}"#,
            r#"{"schema_version":null,"decision":{"verdict":"PASS","allow_merge":true}}"#,
            r#"{"schema_version":{"major":2},"decision":{"verdict":"PASS","allow_merge":true}}"#,
            r#"{"schema_version":["2.1"],"decision":{"verdict":"PASS","allow_merge":true}}"#,
        ] {
            std::fs::write(temp.path().join("00_summary/MERGE_GATE.json"), bad).unwrap();
            let err = read_merge_gate_summary(temp.path())
                .expect_err("a non-string schema_version must fail loud");
            assert!(
                format!("{err:#}").contains("schema_version"),
                "{err:#} for {bad}"
            );
        }
    }

    #[test]
    fn verdict_normalized_to_block_forces_every_derived_axis_conservative() {
        // A verdict collapsed to BLOCK while `allow_merge`/`merge_recommendation`
        // stayed permissive published a decision that contradicted itself: the
        // human surface said BLOCK, the machine surface said approve, and
        // `compute_exit_code` keyed off the latter and let automation through.
        // The documented invariant is `allow_merge == (verdict == "PASS")`.
        let pack = pack_with_gate(
            r#"{"verdict":"MAYBE","merge_recommendation":"approve",
                "allow_merge":true,"quality_pass":true,
                "analysis_status":"complete"}"#,
        );

        let summary = read_merge_gate_summary(pack.path()).expect("pack stays readable");
        assert_eq!(summary.verdict, "BLOCK");
        assert!(
            !summary.allow_merge,
            "a normalized BLOCK cannot keep allow_merge: {summary:?}"
        );
        assert_eq!(
            summary.merge_recommendation,
            crate::policy::engine::MergeRecommendation::Block,
            "the recommendation must follow the verdict it was normalized to: {summary:?}"
        );
        assert!(
            summary
                .caveats
                .iter()
                .any(|c| c.starts_with("unknown_verdict:")),
            "the normalization is still reported: {:?}",
            summary.caveats
        );

        let config = test_config();
        let report = Report {
            target: "feature/unknown-verdict".to_string(),
            bases: vec!["main".to_string()],
            diffs: vec![],
            checks: vec![],
            heuristics: None,
            artifacts_dir: pack.path().to_path_buf(),
            duration: Duration::from_secs(1),
            unchanged: false,
        };
        let cli = build_cli_json_summary(&config, &report).expect("gate artifact is readable");
        assert_eq!(
            compute_exit_code(&cli, false, false),
            1,
            "a BLOCK verdict must not exit 0"
        );
    }

    #[test]
    fn a_gate_whose_root_is_not_an_object_is_corrupt_not_a_block() {
        // The legacy tolerance says WHERE the decision sits, not that anything
        // parseable is a decision. A pack that parses to an array, a scalar or
        // `null` has no fields at all: the CLI read one as a decision with no
        // signals and answered a normalized BLOCK — a successful summary for an
        // artifact the MCP reader rejects as corrupt.
        for root in ["[1,2,3]", "\"BLOCK\"", "null", "7"] {
            let temp = tempfile::tempdir().unwrap();
            std::fs::create_dir_all(temp.path().join("00_summary")).unwrap();
            std::fs::write(temp.path().join("00_summary/MERGE_GATE.json"), root).unwrap();

            let err = read_merge_gate_summary(temp.path())
                .expect_err("a non-object gate root carries no decision");
            let message = format!("{err:#}");
            assert!(
                message.contains("not a JSON object"),
                "the error must name the real defect, got: {message}"
            );
        }
    }

    #[test]
    fn an_explicit_block_verdict_overrides_an_approve_recommendation() {
        // Every field here is present, correctly typed and in vocabulary, so
        // none of the unreadable/unknown guards fire — and the reader simply
        // believed each field in turn: it published the pack's `BLOCK` verdict
        // beside an `Approve` recommendation, and `compute_exit_code` keys off
        // the recommendation, so a gate whose own canonical artifact said BLOCK
        // exited 0 outside CI. The MCP adapter has reconciled these axes by
        // conservativeness since it was written.
        let pack = pack_with_gate(
            r#"{"verdict":"BLOCK","merge_recommendation":"approve",
                "allow_merge":false,"quality_pass":true,
                "analysis_status":"complete"}"#,
        );

        let summary = read_merge_gate_summary(pack.path()).expect("pack stays readable");
        assert_eq!(summary.verdict, "BLOCK");
        assert_eq!(
            summary.merge_recommendation,
            crate::policy::engine::MergeRecommendation::Block,
            "an explicit BLOCK cannot leave an approve recommendation: {summary:?}"
        );
        assert!(!summary.allow_merge, "{summary:?}");
        assert!(
            summary
                .caveats
                .iter()
                .any(|c| c.starts_with("core_inconsistency:")),
            "the contradiction must be named, not silently resolved: {:?}",
            summary.caveats
        );

        let config = test_config();
        let report = Report {
            target: "feature/contradictory-gate".to_string(),
            bases: vec!["main".to_string()],
            diffs: vec![],
            checks: vec![],
            heuristics: None,
            artifacts_dir: pack.path().to_path_buf(),
            duration: Duration::from_secs(1),
            unchanged: false,
        };
        let cli = build_cli_json_summary(&config, &report).expect("gate artifact is readable");
        assert_eq!(
            compute_exit_code(&cli, false, false),
            1,
            "a BLOCK gate must fail the process even outside CI"
        );
    }

    #[test]
    fn a_permissive_flag_never_lowers_a_stated_verdict() {
        // The mirror direction of the same rule: `allow_merge: true` beside a
        // `review_required` recommendation must not buy a PASS.
        let pack = pack_with_gate(
            r#"{"verdict":"CONDITIONAL","merge_recommendation":"review_required",
                "allow_merge":true,"quality_pass":true,
                "analysis_status":"complete"}"#,
        );

        let summary = read_merge_gate_summary(pack.path()).expect("pack stays readable");
        assert_eq!(summary.verdict, "CONDITIONAL");
        assert!(
            !summary.allow_merge,
            "allow_merge == (verdict == PASS) is the documented invariant: {summary:?}"
        );
        assert_eq!(
            summary.merge_recommendation,
            crate::policy::engine::MergeRecommendation::ReviewRequired,
            "{summary:?}"
        );
    }

    #[test]
    fn a_consistent_pack_earns_no_inconsistency_caveat() {
        // Guard against over-reach: reconciliation must be silent when the
        // axes already agree.
        let pack = pack_with_gate(
            r#"{"verdict":"PASS","merge_recommendation":"approve",
                "allow_merge":true,"quality_pass":true,
                "analysis_status":"complete"}"#,
        );

        let summary = read_merge_gate_summary(pack.path()).expect("pack stays readable");
        assert_eq!(summary.verdict, "PASS");
        assert!(summary.allow_merge, "{summary:?}");
        assert!(
            summary.caveats.is_empty(),
            "a consistent pack reads clean: {:?}",
            summary.caveats
        );
    }

    #[test]
    fn fail_on_warnings_counts_the_checks_the_artifact_run_generated() {
        // `--fail-on-warnings` promises to fail when ANY check warns, but it
        // read `Report.checks` — the list the CLI itself executed. The artifact
        // stage appends `public_api_diff`, `unsafe_audit`, `ghost_refs` and the
        // synthetic heuristics check to the list `MERGE_GATE.json` is built
        // from, and none of them ever returns to the CLI. A run whose only
        // warning came from one of those exited 0 under the flag.
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(temp.path().join("00_summary")).unwrap();
        std::fs::write(
            temp.path().join("00_summary/MERGE_GATE.json"),
            r#"{"schema_version":"2.2",
                "checks":[
                  {"name":"Cargo check","status":"passed"},
                  {"name":"public_api_diff","status":"warnings"}
                ],
                "decision":{"verdict":"PASS","merge_recommendation":"approve",
                            "allow_merge":true,"quality_pass":true,
                            "analysis_status":"complete"}}"#,
        )
        .unwrap();

        let mut config = test_config();
        config.execution_mode = ExecutionMode::Ci;
        let report = Report {
            target: "feature/generated-warning".to_string(),
            bases: vec!["main".to_string()],
            diffs: vec![],
            checks: vec![CheckResult {
                name: "Cargo check".to_string(),
                status: CheckStatus::Passed,
                duration: Duration::from_secs(1),
                output: String::new(),
                cached: false,
                provenance: None,
            }],
            heuristics: None,
            artifacts_dir: temp.path().to_path_buf(),
            duration: Duration::from_secs(1),
            unchanged: false,
        };

        let cli = build_cli_json_summary(&config, &report).expect("gate artifact is readable");
        assert_eq!(
            cli.checks_summary.warned, 0,
            "the CLI's own list genuinely has no warning: {:?}",
            cli.checks_summary
        );
        assert_eq!(
            cli.checks_summary.warned_in_pack, 1,
            "the pack's canonical list has one: {:?}",
            cli.checks_summary
        );
        assert_eq!(
            compute_exit_code(&cli, true, true),
            1,
            "--ci --fail-on-warnings must fail on a warning only the pack knows about"
        );
        assert_eq!(
            compute_exit_code(&cli, true, false),
            0,
            "without the flag a warning still does not fail the run"
        );
    }

    #[test]
    fn a_pack_without_a_checks_list_keeps_the_cli_warning_tally() {
        // Guard the fallback: a legacy pack with no `checks` array must not
        // report FEWER warnings than the CLI already counted itself.
        let pack = pack_with_gate(
            r#"{"verdict":"CONDITIONAL","merge_recommendation":"review_required",
                "allow_merge":false,"quality_pass":true,
                "analysis_status":"complete"}"#,
        );

        let mut config = test_config();
        config.execution_mode = ExecutionMode::Ci;
        let report = Report {
            target: "feature/legacy-pack".to_string(),
            bases: vec!["main".to_string()],
            diffs: vec![],
            checks: vec![CheckResult {
                name: "Semgrep scan".to_string(),
                status: CheckStatus::Warnings,
                duration: Duration::from_secs(1),
                output: String::new(),
                cached: false,
                provenance: None,
            }],
            heuristics: None,
            artifacts_dir: pack.path().to_path_buf(),
            duration: Duration::from_secs(1),
            unchanged: false,
        };

        let cli = build_cli_json_summary(&config, &report).expect("gate artifact is readable");
        assert_eq!(
            cli.checks_summary.warned_in_pack, 1,
            "{:?}",
            cli.checks_summary
        );
        assert_eq!(compute_exit_code(&cli, true, true), 1);
    }

    #[test]
    fn a_reused_pack_with_an_unreadable_check_status_still_fails_on_warnings() {
        // The tally compared against the exact string `"warnings"`, so a status
        // this build does not emit — `"WARNINGS"` from another writer, a stale
        // pack `--update` reused unchanged — counted as NOT a warning. The run
        // exited 0 under `--ci --fail-on-warnings` on an artifact whose warning
        // signal the reader could not read. Present-but-unreadable is not zero:
        // it joins the tally and says so.
        let pack = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(pack.path().join("00_summary")).unwrap();
        std::fs::write(
            pack.path().join("00_summary/MERGE_GATE.json"),
            r#"{"schema_version":"2.2",
                "checks":[{"id":"semgrep","status":"WARNINGS"}],
                "decision":{"verdict":"CONDITIONAL",
                            "merge_recommendation":"review_required",
                            "allow_merge":false,"quality_pass":true,
                            "analysis_status":"complete"}}"#,
        )
        .unwrap();

        let mut config = test_config();
        config.execution_mode = ExecutionMode::Ci;
        let report = Report {
            target: "feature/reused-pack".to_string(),
            bases: vec!["main".to_string()],
            diffs: vec![],
            checks: vec![],
            heuristics: None,
            artifacts_dir: pack.path().to_path_buf(),
            duration: Duration::from_secs(1),
            unchanged: true,
        };

        let cli = build_cli_json_summary(&config, &report).expect("gate artifact is readable");
        assert_eq!(
            cli.checks_summary.warned_in_pack, 1,
            "an unreadable status is not a clean one: {:?}",
            cli.checks_summary
        );
        assert_eq!(
            compute_exit_code(&cli, true, true),
            1,
            "--ci --fail-on-warnings must not pass a pack it cannot read"
        );
        assert!(
            cli.caveats
                .iter()
                .any(|caveat| caveat.starts_with("unreadable_check_status:")),
            "the reader must say what it could not read, got: {:?}",
            cli.caveats
        );
    }

    #[test]
    fn a_reused_pack_with_an_unreadable_checks_container_still_fails_on_warnings() {
        // The same rule as the per-entry status, one level up: `checks` present
        // but not an array left the tally at zero and fell back to the checks
        // this run executed — which on an unchanged `--update` run is none at
        // all. `--ci --fail-on-warnings` then exited 0 on a pack whose warning
        // list the reader could not read.
        //
        // ABSENT `checks` keeps its tolerance and is covered by
        // `a_pack_without_a_checks_list_keeps_the_cli_warning_tally`: a pack that
        // states no list may simply be an old one. A pack that states something
        // unreadable is not, and no legacy carve-out applies — `checks` has been
        // emitted since 1.0, so a non-array there was never a valid shape.
        for container in [
            r#"{"semgrep":"warnings"}"#,
            r#""warnings""#,
            "7",
            "null",
            "true",
        ] {
            let pack = tempfile::tempdir().unwrap();
            std::fs::create_dir_all(pack.path().join("00_summary")).unwrap();
            std::fs::write(
                pack.path().join("00_summary/MERGE_GATE.json"),
                format!(
                    r#"{{"schema_version":"2.2",
                        "checks":{container},
                        "decision":{{"verdict":"PASS","merge_recommendation":"approve",
                                     "allow_merge":true,"quality_pass":true,
                                     "analysis_status":"complete"}}}}"#
                ),
            )
            .unwrap();

            let mut config = test_config();
            config.execution_mode = ExecutionMode::Ci;
            let report = Report {
                target: "feature/reused-pack".to_string(),
                bases: vec!["main".to_string()],
                diffs: vec![],
                checks: vec![],
                heuristics: None,
                artifacts_dir: pack.path().to_path_buf(),
                duration: Duration::from_secs(1),
                unchanged: true,
            };

            let cli = build_cli_json_summary(&config, &report).expect("gate artifact is readable");
            assert!(
                cli.checks_summary.warned_in_pack >= 1,
                "an unreadable checks list is not an empty one ({container}): {:?}",
                cli.checks_summary
            );
            assert_eq!(
                compute_exit_code(&cli, true, true),
                1,
                "--ci --fail-on-warnings must not pass a pack it cannot read ({container})"
            );
            assert!(
                cli.caveats
                    .iter()
                    .any(|caveat| caveat.starts_with("unreadable_checks:")),
                "the reader must say what it could not read ({container}), got: {:?}",
                cli.caveats
            );
        }
    }

    #[test]
    fn a_canonical_check_status_is_not_reported_as_unreadable() {
        // The guard on that widening: every status this build emits must stay
        // readable, or the caveat becomes noise on every clean run and a
        // `passed` check inflates the warning tally.
        let pack = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(pack.path().join("00_summary")).unwrap();
        std::fs::write(
            pack.path().join("00_summary/MERGE_GATE.json"),
            r#"{"schema_version":"2.2",
                "checks":[{"id":"a","status":"passed"},{"id":"b","status":"failed"},
                          {"id":"c","status":"skipped"},{"id":"d","status":"error"}],
                "decision":{"verdict":"PASS","merge_recommendation":"approve",
                            "allow_merge":true,"quality_pass":true,
                            "analysis_status":"complete"}}"#,
        )
        .unwrap();

        let mut config = test_config();
        config.execution_mode = ExecutionMode::Ci;
        let report = Report {
            target: "feature/clean-pack".to_string(),
            bases: vec!["main".to_string()],
            diffs: vec![],
            checks: vec![],
            heuristics: None,
            artifacts_dir: pack.path().to_path_buf(),
            duration: Duration::from_secs(1),
            unchanged: true,
        };

        let cli = build_cli_json_summary(&config, &report).expect("gate artifact is readable");
        assert_eq!(
            cli.checks_summary.warned_in_pack, 0,
            "no check in that pack warned: {:?}",
            cli.checks_summary
        );
        assert!(
            !cli.caveats
                .iter()
                .any(|caveat| caveat.starts_with("unreadable_check_status:")),
            "the emitted vocabulary is readable, got: {:?}",
            cli.caveats
        );
    }

    #[test]
    fn a_mistyped_recommendation_is_not_read_as_an_absent_one() {
        // `merge_recommendation: 7` collapsed through `as_str()` into "no
        // recommendation", and the fallback then RECONSTRUCTED `Approve` from
        // `allow_merge` — so a pack carrying a signal this reader could not
        // read reported success, silently, and `--ci` exited 0. The documented
        // contract says a mistyped signal normalizes conservatively and is
        // reported; only the MCP surface actually did that.
        let pack = pack_with_gate(
            r#"{"verdict":"PASS","merge_recommendation":7,
                "allow_merge":true,"quality_pass":true,
                "analysis_status":"complete"}"#,
        );

        let summary = read_merge_gate_summary(pack.path()).expect("pack stays readable");
        assert!(
            summary
                .caveats
                .iter()
                .any(|c| c.starts_with("unreadable_merge_recommendation:")),
            "an ignored signal must be named: {:?}",
            summary.caveats
        );
        assert_eq!(
            summary.merge_recommendation,
            crate::policy::engine::MergeRecommendation::Block,
            "an unreadable signal cannot leave a permissive recommendation: {summary:?}"
        );
        assert!(
            !summary.allow_merge,
            "an unreadable signal cannot leave allow_merge: {summary:?}"
        );

        let config = test_config();
        let report = Report {
            target: "feature/mistyped-recommendation".to_string(),
            bases: vec!["main".to_string()],
            diffs: vec![],
            checks: vec![],
            heuristics: None,
            artifacts_dir: pack.path().to_path_buf(),
            duration: Duration::from_secs(1),
            unchanged: false,
        };
        let cli = build_cli_json_summary(&config, &report).expect("gate artifact is readable");
        assert_eq!(
            compute_exit_code(&cli, false, false),
            1,
            "a pack with an unreadable decision signal must not exit 0"
        );
    }

    #[test]
    fn a_mistyped_allow_merge_is_named_not_silently_defaulted() {
        // `allow_merge: "false"` already defaulted to `false`, which is the safe
        // direction — but silently. The reader ignored a field and reported a
        // clean read, which is the same contract breach in a quieter costume.
        let pack = pack_with_gate(
            r#"{"verdict":"PASS","merge_recommendation":"approve",
                "allow_merge":"true","quality_pass":true,
                "analysis_status":"complete"}"#,
        );

        let summary = read_merge_gate_summary(pack.path()).expect("pack stays readable");
        assert!(
            summary
                .caveats
                .iter()
                .any(|c| c.starts_with("unreadable_allow_merge:")),
            "an ignored signal must be named: {:?}",
            summary.caveats
        );
        assert!(!summary.allow_merge, "{summary:?}");
        assert_eq!(
            summary.merge_recommendation,
            crate::policy::engine::MergeRecommendation::Block,
            "{summary:?}"
        );
    }

    #[test]
    fn a_mistyped_verdict_says_so_instead_of_claiming_none_was_present() {
        // The `None` arm's message ("carries no `verdict`") is a lie for a
        // verdict that IS present and merely untypable.
        let pack = pack_with_gate(
            r#"{"verdict":7,"merge_recommendation":"approve",
                "allow_merge":true,"quality_pass":true}"#,
        );

        let summary = read_merge_gate_summary(pack.path()).expect("pack stays readable");
        assert_eq!(summary.verdict, "BLOCK");
        assert!(
            summary
                .caveats
                .iter()
                .any(|c| c.starts_with("unreadable_verdict:")),
            "the mistyped verdict must be named: {:?}",
            summary.caveats
        );
        assert!(
            !summary
                .caveats
                .iter()
                .any(|c| c.contains("carries no `verdict`")),
            "a present-but-untypable verdict is not an absent one: {:?}",
            summary.caveats
        );
    }

    #[test]
    fn a_well_typed_pack_gains_no_unreadable_caveats() {
        // Guard against over-reach: the conservative path must fire on
        // mistyped signals only, never on an ordinary pack.
        let pack = pack_with_gate(
            r#"{"verdict":"PASS","merge_recommendation":"approve",
                "allow_merge":true,"quality_pass":true,
                "analysis_status":"complete"}"#,
        );

        let summary = read_merge_gate_summary(pack.path()).expect("pack stays readable");
        assert_eq!(summary.verdict, "PASS");
        assert!(summary.allow_merge, "{summary:?}");
        assert_eq!(
            summary.merge_recommendation,
            crate::policy::engine::MergeRecommendation::Approve,
            "{summary:?}"
        );
        assert!(
            !summary.caveats.iter().any(|c| c.starts_with("unreadable_")),
            "a well-typed pack carries no unreadable caveats: {:?}",
            summary.caveats
        );
    }

    #[test]
    fn absent_verdict_normalized_to_block_is_equally_conservative() {
        // Same collapse through the other arm: no `verdict` at all, with the
        // remaining fields permissive.
        let pack = pack_with_gate(
            r#"{"merge_recommendation":"approve","allow_merge":true,"quality_pass":true}"#,
        );

        let summary = read_merge_gate_summary(pack.path()).expect("pack stays readable");
        assert_eq!(summary.verdict, "BLOCK");
        assert!(!summary.allow_merge);
        assert_eq!(
            summary.merge_recommendation,
            crate::policy::engine::MergeRecommendation::Block
        );
    }

    #[test]
    fn versioned_pack_without_a_decision_object_is_an_error() {
        // From 2.1 the pack states its schema, and that schema has a `decision`
        // object — `tools/validate_merge_gate.py` requires one and the MCP
        // reader errors without one. Treating the root as the decision instead
        // let a structurally broken pack normalize quietly to BLOCK/false with
        // a caveat, which is a re-derived verdict wearing a reader's clothes.
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(temp.path().join("00_summary")).unwrap();
        for bad in [
            r#"{"schema_version":"2.2"}"#,
            r#"{"schema_version":"2.2","decision":null}"#,
            r#"{"schema_version":"2.2","decision":[]}"#,
            r#"{"schema_version":"2.2","decision":"PASS"}"#,
            r#"{"schema_version":"1.0","verdict":"PASS","allow_merge":true}"#,
        ] {
            std::fs::write(temp.path().join("00_summary/MERGE_GATE.json"), bad).unwrap();
            let err = read_merge_gate_summary(temp.path())
                .expect_err("a versioned pack without a decision object must fail loud");
            assert!(
                format!("{err:#}").contains("decision"),
                "the error must name the missing object: {err:#} for {bad}"
            );
        }
    }

    #[test]
    fn a_decision_stating_no_signal_is_corrupt_on_both_readers() {
        // The object is THERE and it is an object, so the structural check
        // above passes — and it states nothing. Normalizing that to BLOCK
        // published a verdict for an artifact that never gave one, and did it
        // on the one surface that mattered: `tools/validate_merge_gate.py`
        // rejects the same pack for its missing required fields, the MCP
        // adapter returns `storage_corrupt`, and `prview gate` cannot even
        // deserialize it. Absence is forgiven per FIELD, because that is the
        // shape of an older pack; a decision block with no signal at all is not
        // an older pack, it is a truncated one.
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(temp.path().join("00_summary")).unwrap();
        for bad in [
            r#"{"schema_version":"2.2","decision":{}}"#,
            r#"{"schema_version":"2.2","decision":{"quality_pass":true,"analysis_status":"complete"}}"#,
            r#"{}"#,
        ] {
            std::fs::write(temp.path().join("00_summary/MERGE_GATE.json"), bad).unwrap();

            let err = read_merge_gate_summary(temp.path())
                .expect_err("a decision that states nothing is not a BLOCK verdict");
            assert!(
                format!("{err:#}").contains("corrupt"),
                "the CLI must call it corrupt: {err:#} for {bad}"
            );

            let mcp_err = crate::mcp::read::read_decision(temp.path())
                .expect_err("the MCP adapter rejects the same pack");
            assert_eq!(
                mcp_err.class,
                crate::mcp::types::error_class::STORAGE_CORRUPT,
                "the two readers must agree on {bad}"
            );
        }
    }

    #[test]
    fn an_incomplete_analysis_cannot_be_published_as_a_pass() {
        // The contract permits `PASS` only when `analysis_status == "complete"`,
        // so an `incomplete` analysis beside a clean approval contradicts
        // itself. The axis was read only AFTER the reconciliation, for
        // reporting, so it never raised the rank and the approval published
        // verbatim — a run that says it did not finish looking.
        for status in ["incomplete", "degraded"] {
            let pack = pack_with_gate(&format!(
                r#"{{"verdict":"PASS","merge_recommendation":"approve","allow_merge":true,"quality_pass":true,"analysis_status":"{status}"}}"#
            ));

            let cli = read_merge_gate_summary(pack.path()).expect("readable");
            assert_eq!(
                cli.verdict, "CONDITIONAL",
                "{status} analysis is not a PASS"
            );
            assert!(!cli.allow_merge);
            assert!(
                cli.caveats.iter().any(|c| {
                    c.starts_with("core_inconsistency:")
                        && c.contains(&format!("analysis_status={status}"))
                }),
                "the contradiction must be named: {:?}",
                cli.caveats
            );

            let mcp = crate::mcp::read::read_decision(pack.path()).expect("readable");
            assert_eq!(
                mcp.verdict, cli.verdict,
                "the two readers must not disagree"
            );
            assert_eq!(mcp.merge_recommendation, "review_required");
            assert!(!mcp.allow_merge);
            assert!(mcp.normalized);
            assert!(
                mcp.caveats.iter().any(|c| {
                    c.contains("core_inconsistency")
                        && c.contains(&format!("analysis_status={status}"))
                }),
                "the contradiction must be named: {:?}",
                mcp.caveats
            );
        }
    }

    #[test]
    fn a_stated_blocker_cannot_be_published_as_a_pass() {
        // `blocking_issues` is non-empty only when a check reached
        // `PolicyConclusion::Blocked`, which carries `merge_impact == Block`, so
        // a pack that lists one beside a clean approval is stating a BLOCK it
        // did not publish. `policy_allow_merge` is the same fact
        // (`policy_allow_merge = blocking_issues.is_empty()`), so a pack may
        // state either of them.
        for decision in [
            r#"{"verdict":"PASS","merge_recommendation":"approve","allow_merge":true,"quality_pass":true,"blocking_issues":["Clippy (failed)"]}"#,
            r#"{"verdict":"PASS","merge_recommendation":"approve","allow_merge":true,"quality_pass":true,"policy_allow_merge":false}"#,
        ] {
            let pack = pack_with_gate(decision);

            let cli = read_merge_gate_summary(pack.path()).expect("readable");
            assert_eq!(cli.verdict, "BLOCK", "a stated blocker is a BLOCK");
            assert!(!cli.allow_merge);
            assert!(
                cli.caveats
                    .iter()
                    .any(|c| c.starts_with("core_inconsistency:")),
                "the contradiction must be named: {:?}",
                cli.caveats
            );

            let mcp = crate::mcp::read::read_decision(pack.path()).expect("readable");
            assert_eq!(
                mcp.verdict, cli.verdict,
                "the two readers must not disagree"
            );
            assert_eq!(mcp.merge_recommendation, "block");
            assert!(!mcp.allow_merge);
            assert!(mcp.normalized);
        }
    }

    #[test]
    fn a_healthy_block_pack_states_no_inconsistency() {
        // The false positive the ranking must not create: a BLOCK pack states a
        // blocker, `policy_allow_merge: false`, `quality_pass: false` and a
        // COMPLETE analysis — every axis agreeing on rank 3 — and that is every
        // BLOCK pack this tool writes.
        let pack = pack_with_gate(
            r#"{"verdict":"BLOCK","merge_recommendation":"block","allow_merge":false,"quality_pass":false,"analysis_status":"complete","policy_allow_merge":false,"blocking_issues":["Clippy (failed)"]}"#,
        );

        let cli = read_merge_gate_summary(pack.path()).expect("readable");
        assert_eq!(cli.verdict, "BLOCK");
        assert!(!cli.allow_merge);
        assert!(
            !cli.caveats
                .iter()
                .any(|c| c.starts_with("core_inconsistency:")),
            "a pack whose axes all agree is not inconsistent: {:?}",
            cli.caveats
        );

        let mcp = crate::mcp::read::read_decision(pack.path()).expect("readable");
        assert_eq!(mcp.verdict, "BLOCK");
        assert!(
            !mcp.caveats.iter().any(|c| c.contains("core_inconsistency")),
            "a pack whose axes all agree is not inconsistent: {:?}",
            mcp.caveats
        );
        assert!(!mcp.normalized, "a healthy BLOCK pack is a faithful read");
    }

    #[test]
    fn a_healthy_conditional_pack_states_no_inconsistency() {
        // The other side of the same false positive: a CONDITIONAL pack states
        // a degraded analysis and a failed quality axis with NO blocker, and
        // every axis agrees on rank 2.
        let pack = pack_with_gate(
            r#"{"verdict":"CONDITIONAL","merge_recommendation":"review_required","allow_merge":false,"quality_pass":false,"analysis_status":"degraded","policy_allow_merge":true,"blocking_issues":[]}"#,
        );

        let cli = read_merge_gate_summary(pack.path()).expect("readable");
        assert_eq!(cli.verdict, "CONDITIONAL");
        assert!(
            !cli.caveats
                .iter()
                .any(|c| c.starts_with("core_inconsistency:")),
            "a pack whose axes all agree is not inconsistent: {:?}",
            cli.caveats
        );

        let mcp = crate::mcp::read::read_decision(pack.path()).expect("readable");
        assert_eq!(mcp.verdict, "CONDITIONAL");
        assert!(
            !mcp.normalized,
            "a healthy CONDITIONAL pack is a faithful read"
        );
    }

    #[test]
    fn a_pack_stating_none_of_the_new_axes_is_read_exactly_as_before() {
        // Absence states nothing, on every axis. A pack written before
        // `analysis_status`, `policy_allow_merge` or `blocking_issues` existed
        // must not be dragged to CONDITIONAL by their mere omission.
        let pack = pack_with_gate(
            r#"{"verdict":"PASS","merge_recommendation":"approve","allow_merge":true,"quality_pass":true}"#,
        );

        let cli = read_merge_gate_summary(pack.path()).expect("readable");
        assert_eq!(cli.verdict, "PASS");
        assert!(cli.allow_merge);
        assert!(
            !cli.caveats
                .iter()
                .any(|c| c.starts_with("core_inconsistency:")),
            "absence is not a contradiction: {:?}",
            cli.caveats
        );

        let mcp = crate::mcp::read::read_decision(pack.path()).expect("readable");
        assert_eq!(mcp.verdict, "PASS");
        assert!(mcp.allow_merge);
        assert!(!mcp.normalized);
    }

    #[test]
    fn the_new_axes_are_conservative_when_they_cannot_be_typed() {
        // Same rule as `quality_pass`: present-but-unreadable is neither a
        // stated value nor an absent one, on every axis that now ranks.
        for (field, decision) in [
            (
                "analysis_status",
                r#"{"verdict":"PASS","merge_recommendation":"approve","allow_merge":true,"quality_pass":true,"analysis_status":7}"#,
            ),
            (
                "policy_allow_merge",
                r#"{"verdict":"PASS","merge_recommendation":"approve","allow_merge":true,"quality_pass":true,"policy_allow_merge":"false"}"#,
            ),
            (
                "blocking_issues",
                r#"{"verdict":"PASS","merge_recommendation":"approve","allow_merge":true,"quality_pass":true,"blocking_issues":"Clippy"}"#,
            ),
        ] {
            let pack = pack_with_gate(decision);

            let cli = read_merge_gate_summary(pack.path()).expect("readable");
            assert_eq!(cli.verdict, "BLOCK", "{field} cannot be typed");
            assert!(!cli.allow_merge);
            assert!(
                cli.caveats
                    .iter()
                    .any(|c| c.starts_with(&format!("unreadable_{field}:"))),
                "the unreadable axis must be named: {:?}",
                cli.caveats
            );

            let mcp = crate::mcp::read::read_decision(pack.path()).expect("readable");
            assert_eq!(mcp.verdict, "BLOCK", "{field} cannot be typed");
            assert!(mcp.normalized);
            assert!(
                mcp.caveats
                    .iter()
                    .any(|c| c.starts_with(&format!("unreadable_{field}:"))),
                "the unreadable axis must be named: {:?}",
                mcp.caveats
            );
        }
    }

    #[test]
    fn an_analysis_status_outside_the_vocabulary_is_named_not_ranked() {
        // Mirrors `unknown_merge_recommendation`: a value that IS a string but
        // is not one this contract defines cannot rank, so it drops out of the
        // reconciliation — and is named rather than vanishing into a confident
        // surface derived from the remaining axes.
        let pack = pack_with_gate(
            r#"{"verdict":"PASS","merge_recommendation":"approve","allow_merge":true,"quality_pass":true,"analysis_status":"partial"}"#,
        );

        let cli = read_merge_gate_summary(pack.path()).expect("readable");
        assert!(
            cli.caveats
                .iter()
                .any(|c| c.starts_with("unknown_analysis_status:")),
            "the unrecognized value must be named: {:?}",
            cli.caveats
        );

        let mcp = crate::mcp::read::read_decision(pack.path()).expect("readable");
        assert!(
            mcp.caveats
                .iter()
                .any(|c| c.starts_with("unknown_analysis_status:")),
            "the unrecognized value must be named: {:?}",
            mcp.caveats
        );
    }

    #[test]
    fn a_quality_axis_that_cannot_be_typed_is_not_read_as_absent() {
        // The gap between the two states the previous test relies on: a
        // `quality_pass` that is PRESENT but not a boolean. `as_bool()` returned
        // `None` for it, which is exactly what an OLDER pack looks like, so the
        // string `"false"` bought a clean approval with no caveat at all — on
        // both surfaces. A stated-but-unreadable axis is now a mistyped signal
        // like every other one: BLOCK, and named.
        let pack = pack_with_gate(
            r#"{"verdict":"PASS","merge_recommendation":"approve","allow_merge":true,"quality_pass":"false"}"#,
        );

        let cli = read_merge_gate_summary(pack.path()).expect("readable");
        assert_eq!(
            cli.verdict, "BLOCK",
            "a signal that cannot be typed normalizes to BLOCK"
        );
        assert!(!cli.allow_merge);
        assert!(
            cli.caveats
                .iter()
                .any(|c| c.starts_with("unreadable_quality_pass:")),
            "the unreadable axis must be named: {:?}",
            cli.caveats
        );

        let mcp = crate::mcp::read::read_decision(pack.path()).expect("readable");
        assert_eq!(
            mcp.verdict, cli.verdict,
            "the two readers must not disagree"
        );
        assert_eq!(mcp.merge_recommendation, "block");
        assert!(!mcp.allow_merge);
        assert!(mcp.normalized);
        assert!(
            mcp.caveats
                .iter()
                .any(|c| c.starts_with("unreadable_quality_pass:")),
            "the unreadable axis must be named: {:?}",
            mcp.caveats
        );
    }

    #[test]
    fn a_failed_quality_axis_cannot_be_published_as_a_pass() {
        // The contract permits `PASS` only when quality passes, so a pack that
        // states `quality_pass: false` beside a clean approval contradicts
        // itself. Leaving that axis out of the reconciliation published the
        // approval verbatim — with `allow_merge: true`, and on BOTH surfaces, so
        // automation approved it too.
        let pack = pack_with_gate(
            r#"{"verdict":"PASS","merge_recommendation":"approve","allow_merge":true,"quality_pass":false}"#,
        );

        let cli = read_merge_gate_summary(pack.path()).expect("readable");
        assert_eq!(
            cli.verdict, "CONDITIONAL",
            "a failed quality axis is not a PASS"
        );
        assert!(!cli.allow_merge);
        assert!(
            cli.caveats
                .iter()
                .any(|c| c.starts_with("core_inconsistency:") && c.contains("quality_pass=false")),
            "the contradiction must be named: {:?}",
            cli.caveats
        );

        let mcp = crate::mcp::read::read_decision(pack.path()).expect("readable");
        assert_eq!(
            mcp.verdict, cli.verdict,
            "the two readers must not disagree"
        );
        assert_eq!(mcp.merge_recommendation, "review_required");
        assert!(!mcp.allow_merge);
        assert!(mcp.normalized);
        assert!(
            mcp.caveats
                .iter()
                .any(|c| c.contains("core_inconsistency") && c.contains("quality_pass=false")),
            "the contradiction must be named: {:?}",
            mcp.caveats
        );
    }

    #[test]
    fn a_pack_that_states_no_quality_axis_is_read_exactly_as_before() {
        // Absence stays forgiven per FIELD — that is the shape of an older pack.
        // Only a STATED `quality_pass: false` ranks; defaulting an absent one to
        // `false` would have turned every pre-quality_pass pack into a
        // CONDITIONAL.
        let pack = pack_with_gate(
            r#"{"verdict":"PASS","merge_recommendation":"approve","allow_merge":true}"#,
        );

        let cli = read_merge_gate_summary(pack.path()).expect("readable");
        assert_eq!(cli.verdict, "PASS");
        assert!(cli.allow_merge);
        assert!(
            !cli.caveats
                .iter()
                .any(|c| c.starts_with("core_inconsistency:")),
            "{:?}",
            cli.caveats
        );
        // The reconciliation kept the PASS, but the SUMMARY has to agree with
        // it. Defaulting the absent axis to `false` published a failed quality
        // gate the pack never claimed, derived `analysis_status: Incomplete`
        // from that, and made `--ci` exit 1 — on the same artifact the MCP
        // adapter approved.
        assert!(
            cli.quality_pass,
            "a reconciled PASS implies the quality axis passed"
        );
        assert_eq!(
            cli.analysis_status,
            crate::policy::engine::AnalysisStatus::Complete,
            "a reconciled PASS implies a complete analysis"
        );
        assert_eq!(
            exit_code_for(&pack, true),
            0,
            "a legacy PASS pack must not fail --ci"
        );

        let mcp = crate::mcp::read::read_decision(pack.path()).expect("readable");
        assert_eq!(mcp.verdict, "PASS");
        assert!(mcp.allow_merge);
        assert!(!mcp.normalized);
        assert_eq!(
            mcp.allow_merge, cli.allow_merge,
            "the two readers must not disagree on the same artifact"
        );
    }

    #[test]
    fn an_absent_quality_axis_stays_conservative_when_the_pack_is_not_a_pass() {
        // The derivation runs off the RECONCILED outcome, so it only ever says
        // "passed" where the contract already implies it. A legacy pack that is
        // held at CONDITIONAL or BLOCK states no quality axis either, and
        // inferring a pass for it would soften a verdict on no evidence.
        for decision in [
            r#"{"verdict":"CONDITIONAL","merge_recommendation":"review_required","allow_merge":false}"#,
            r#"{"verdict":"BLOCK","merge_recommendation":"block","allow_merge":false}"#,
        ] {
            let cli = read_merge_gate_summary(pack_with_gate(decision).path()).expect("readable");
            assert!(!cli.quality_pass, "{decision}");
            assert_eq!(
                cli.analysis_status,
                crate::policy::engine::AnalysisStatus::Incomplete,
                "{decision}"
            );
        }
    }

    #[test]
    fn a_mistyped_quality_axis_is_never_inferred_from_the_verdict() {
        // The absent/mistyped split from round 20 is load-bearing here: a
        // `quality_pass` that is PRESENT but unreadable normalizes the whole
        // decision to BLOCK, so the derivation below can never read it back as
        // a pass. Inferring from the verdict must not become a way around that.
        let pack = pack_with_gate(
            r#"{"verdict":"PASS","merge_recommendation":"approve","allow_merge":true,"quality_pass":"true"}"#,
        );

        let cli = read_merge_gate_summary(pack.path()).expect("readable");
        assert_eq!(cli.verdict, "BLOCK");
        assert!(!cli.allow_merge);
        assert!(
            !cli.quality_pass,
            "a signal that cannot be read is not a pass"
        );
        assert_eq!(
            exit_code_for(&pack, true),
            1,
            "an unreadable quality axis still fails --ci"
        );
    }

    #[test]
    fn a_quality_axis_that_passes_does_not_soften_a_conservative_verdict() {
        // The asymmetry that keeps the healthy packs quiet: `quality_pass: true`
        // does not assert the gate passed — a breaking-change escalation holds a
        // quality-clean run at CONDITIONAL — so it states no rank of its own and
        // never contradicts the verdict beside it.
        let pack = pack_with_gate(
            r#"{"verdict":"CONDITIONAL","merge_recommendation":"review_required","allow_merge":false,"quality_pass":true}"#,
        );

        let cli = read_merge_gate_summary(pack.path()).expect("readable");
        assert_eq!(cli.verdict, "CONDITIONAL");
        assert!(
            !cli.caveats
                .iter()
                .any(|c| c.starts_with("core_inconsistency:")),
            "{:?}",
            cli.caveats
        );

        let mcp = crate::mcp::read::read_decision(pack.path()).expect("readable");
        assert_eq!(mcp.verdict, "CONDITIONAL");
        assert!(!mcp.caveats.iter().any(|c| c.contains("core_inconsistency")));
    }

    #[test]
    fn a_self_consistent_pack_raises_no_inconsistency_on_either_surface() {
        // `allow_merge` is a two-valued axis: `false` can never rank as high as
        // BLOCK, so comparing it to the numeric rank of the winning verdict made
        // every healthy BLOCK pack look self-contradictory. The consistency
        // check compares the textual axes to each other and `allow_merge` to the
        // flag actually published — nothing else.
        for decision in [
            r#"{"merge_recommendation":"block","verdict":"BLOCK","allow_merge":false,"quality_pass":false}"#,
            r#"{"merge_recommendation":"review_required","verdict":"CONDITIONAL","allow_merge":false,"quality_pass":true}"#,
            r#"{"merge_recommendation":"approve","verdict":"PASS","allow_merge":true,"quality_pass":true}"#,
        ] {
            let pack = pack_with_gate(decision);

            let cli = read_merge_gate_summary(pack.path()).expect("readable");
            assert!(
                !cli.caveats
                    .iter()
                    .any(|c| c.starts_with("core_inconsistency:")),
                "the CLI invented a disagreement in {decision}: {:?}",
                cli.caveats
            );

            let mcp = crate::mcp::read::read_decision(pack.path()).expect("readable");
            assert!(
                !mcp.caveats.iter().any(|c| c.contains("core_inconsistency")),
                "the MCP adapter invented a disagreement in {decision}: {:?}",
                mcp.caveats
            );
            assert!(
                !mcp.normalized,
                "a self-consistent pack is published as stated: {decision}"
            );
        }
    }

    #[test]
    fn a_present_but_unrankable_decision_reads_the_same_on_both_surfaces() {
        // The residual of the previous round: `storage_corrupt` is reserved for
        // a decision that states NOTHING. A signal that is present but cannot
        // rank — a verdict outside the vocabulary, a lone `allow_merge` — is a
        // decision the pack gave, and both readers must normalize it the same
        // conservative way instead of one publishing a summary while the other
        // calls the identical artifact corrupt.
        for decision in [
            r#"{"verdict":"PROBABLY","allow_merge":false}"#,
            r#"{"allow_merge":false}"#,
            r#"{"allow_merge":true}"#,
            r#"{"merge_recommendation":"approve","verdict":"MAYBE","allow_merge":true}"#,
        ] {
            let pack = pack_with_gate(decision);

            let cli = read_merge_gate_summary(pack.path()).expect("the CLI reads a stated signal");
            let mcp = crate::mcp::read::read_decision(pack.path())
                .expect("the MCP adapter reads the same signal");

            assert_eq!(
                cli.verdict, "BLOCK",
                "a decision this reader had to substitute is not an approval: {decision}"
            );
            assert_eq!(
                mcp.verdict, cli.verdict,
                "the two readers must not disagree about {decision}"
            );
            assert_eq!(
                mcp.merge_recommendation, "block",
                "every axis follows the substituted verdict: {decision}"
            );
            assert!(!mcp.allow_merge, "{decision}");
            assert!(mcp.normalized, "a substituted verdict is a normalization");
        }
    }

    #[test]
    fn an_unversioned_allow_merge_only_pack_reads_the_same_on_both_surfaces() {
        // The legacy root shape of the same case: a pre-2.1 pack whose root IS
        // the decision and whose only correctly typed field is `allow_merge`.
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(temp.path().join("00_summary")).unwrap();
        for root in [r#"{"allow_merge":false}"#, r#"{"allow_merge":true}"#] {
            std::fs::write(temp.path().join("00_summary/MERGE_GATE.json"), root).unwrap();

            let cli = read_merge_gate_summary(temp.path()).expect("legacy pack stays readable");
            let mcp = crate::mcp::read::read_decision(temp.path())
                .expect("the MCP adapter reads the same legacy pack");

            assert_eq!(cli.verdict, "BLOCK", "{root}");
            assert_eq!(mcp.verdict, cli.verdict, "the readers must agree on {root}");
            assert!(!mcp.allow_merge, "{root}");
        }
    }

    #[test]
    fn a_decision_stating_one_unreadable_signal_is_still_read() {
        // The other direction, and the reason the rule counts PRESENCE rather
        // than recognizability: a pack that states a verdict outside the
        // vocabulary DID state a decision. The contract has it collapse to
        // BLOCK with an `unknown_verdict:` caveat, and calling it corrupt
        // instead would retire a documented read.
        let pack = pack_with_gate(r#"{"verdict":"PROBABLY"}"#);
        let summary = read_merge_gate_summary(pack.path()).expect("a stated verdict is a decision");
        assert_eq!(summary.verdict, "BLOCK");
        assert!(
            summary
                .caveats
                .iter()
                .any(|c| c.starts_with("unknown_verdict:")),
            "{:?}",
            summary.caveats
        );
    }

    #[test]
    fn unversioned_pack_still_reads_the_root_as_its_decision() {
        // The other direction: a pack with NO `schema_version` predates the
        // field and is the documented legacy read-back surface. Tightening the
        // structural check must not retire it.
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(temp.path().join("00_summary")).unwrap();
        std::fs::write(
            temp.path().join("00_summary/MERGE_GATE.json"),
            r#"{"verdict":"ALLOW","allow_merge":true,"quality_pass":true}"#,
        )
        .unwrap();

        let summary = read_merge_gate_summary(temp.path()).expect("legacy pack stays readable");
        assert_eq!(summary.verdict, "PASS");
        assert!(summary.allow_merge);
        assert!(
            summary.caveats.is_empty(),
            "a legacy pack read as intended raises no caveat: {:?}",
            summary.caveats
        );
    }

    #[test]
    fn test_format_duration_zero() {
        assert_eq!(format_duration(Duration::from_secs(0)), "0s");
    }

    #[test]
    fn test_format_duration_large() {
        assert_eq!(format_duration(Duration::from_secs(3600)), "60m 0s");
    }

    #[test]
    fn artifact_consistency_degraded_pass_summary_avoids_failed_heading() {
        let report = Report {
            target: "feature".to_string(),
            bases: vec!["main".to_string()],
            diffs: vec![],
            checks: vec![CheckResult {
                name: "Semgrep scan".to_string(),
                status: CheckStatus::Failed,
                duration: Duration::from_secs(1),
                output: "{}".to_string(),
                cached: false,
                provenance: None,
            }],
            heuristics: None,
            artifacts_dir: PathBuf::from("."),
            duration: Duration::from_secs(1),
            unchanged: false,
        };
        let gate = MergeGateSummary {
            verdict: "PASS".to_string(),
            analysis_status: crate::policy::engine::AnalysisStatus::Complete,
            merge_recommendation: crate::policy::engine::MergeRecommendation::Approve,
            allow_merge: true,
            quality_pass: true,
            reason: Some("pre-existing findings outside the change".to_string()),
            caveats: Vec::new(),
            warned_checks: 0,
        };

        let heading = failure_summary_heading(&report, Some(&gate)).expect("heading");

        assert!(!heading.contains("Some checks failed"));
        assert!(heading.contains("advisory") || heading.contains("pre-existing"));
    }

    #[test]
    fn test_describe_run_mode_marks_fast_remote_only_preset() {
        let mut config = test_config();
        config.execution_mode = ExecutionMode::Standard;
        config.remote_only = true;

        assert_eq!(
            describe_run_mode(&config),
            "standard · remote-only · fast preset"
        );
    }

    #[test]
    fn test_describe_enabled_steps_includes_fast_remote_only_shape() {
        let mut config = test_config();
        config.profile = test_rust_profile(true);
        config.execution_mode = ExecutionMode::Standard;
        config.remote_only = true;
        config.run_lint = true;
        config.run_tests = false;
        config.run_heuristics = false;

        let steps = describe_enabled_steps(&config);
        assert!(steps.contains("diff"));
        assert!(steps.contains("cargo-check"));
        assert!(steps.contains("lint"));
        assert!(steps.contains("cargo-audit"));
    }
}
