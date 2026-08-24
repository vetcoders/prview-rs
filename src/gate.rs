//! Quality gate adapter for contractual process exit codes.

use crate::output::CliJsonSummary;
use crate::policy::engine::{
    AnalysisStatus, EnforcementAction, EnforcementDisposition, EnforcementMode, MergeRecommendation,
};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::path::Path;

pub const GATE_EXECUTION_ERROR_EXIT_CODE: i32 = 3;

/// `schema_version` this build stamps into `MERGE_GATE.json`.
pub const MERGE_GATE_SCHEMA_VERSION: &str = "2.3";

/// `MERGE_GATE.json` schemas this build has actually seen, as `(MAJOR, MINOR)`.
///
/// This is the SAME set `tools/validate_merge_gate.py` accepts verbatim
/// (`1.0` / `2.0` / `2.1` / `2.2` / `2.3`), so "readable by the CLI/MCP" and "valid per the
/// contract validator" cannot drift apart for a version in the set. The reader
/// is deliberately broader in exactly two documented directions — an absent
/// field and a newer MINOR of a known MAJOR — and both are announced rather
/// than silent.
const MERGE_GATE_KNOWN_SCHEMAS: &[(u32, u32)] = &[(1, 0), (2, 0), (2, 1), (2, 2), (2, 3)];

/// Schema 2.3 is the first version that can prove a `CONDITIONAL` is only a
/// warning. Forward-compatible 2.x packs inherit that requirement; older packs
/// stay readable but cannot claim the strict warnings-only exception.
pub(crate) fn schema_requires_enforcement_disposition(field: Option<&serde_json::Value>) -> bool {
    field
        .and_then(serde_json::Value::as_str)
        .and_then(parse_major_minor)
        .is_some_and(|(major, minor)| major == 2 && minor >= 3)
}

/// Read the additive 2.3 disposition without treating absence in an older pack
/// as evidence. A required, mistyped, or unknown value becomes enforceable
/// `review_required`; it can never unlock the warnings-only exception.
pub(crate) fn read_enforcement_disposition(
    field: Option<&serde_json::Value>,
    required: bool,
    caveats: &mut Vec<String>,
) -> Option<EnforcementDisposition> {
    if !required {
        if field.is_some() {
            caveats.push(
                "legacy_enforcement_disposition_ignored: MERGE_GATE.json schema predates the 2.3 \
                 typed enforcement contract; the field cannot unlock warnings-only acceptance"
                    .to_string(),
            );
        }
        return None;
    }
    match field {
        Some(serde_json::Value::String(raw)) => match raw.as_str() {
            "clean" => Some(EnforcementDisposition::Clean),
            "warnings_only" => Some(EnforcementDisposition::WarningsOnly),
            "review_required" => Some(EnforcementDisposition::ReviewRequired),
            "block" => Some(EnforcementDisposition::Block),
            _ => {
                caveats.push(format!(
                    "unknown_enforcement_disposition: MERGE_GATE.json enforcement_disposition \
                     `{raw}` is outside clean/warnings_only/review_required/block; normalized to \
                     review_required"
                ));
                Some(EnforcementDisposition::ReviewRequired)
            }
        },
        Some(other) => {
            caveats.push(format!(
                "unreadable_enforcement_disposition: MERGE_GATE.json enforcement_disposition is \
                 {}, not a string; normalized to review_required",
                json_type_name(other)
            ));
            Some(EnforcementDisposition::ReviewRequired)
        }
        None => {
            caveats.push(
                "missing_enforcement_disposition: MERGE_GATE.json schema 2.3+ requires a typed \
                 enforcement_disposition; normalized to review_required"
                    .to_string(),
            );
            Some(EnforcementDisposition::ReviewRequired)
        }
    }
}

/// Warning evidence read from the pack's canonical root `checks[]` list.
/// Explicit warnings can unlock the typed warnings-only lane; unreadable
/// statuses still count for `--fail-on-warnings` but ratchet strict enforcement
/// to review-required instead of being mistaken for a harmless warning.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct PackWarningTally {
    pub warned: usize,
    pub has_explicit_warnings: bool,
    pub has_unreadable_signal: bool,
    pub has_review_signal: bool,
    pub has_blocking_signal: bool,
    pub has_new_quality_failure_signal: bool,
}

fn read_required_check_axis<'a>(
    entry: &'a serde_json::Value,
    index: usize,
    field: &str,
    allowed: &[&str],
    caveats: &mut Vec<String>,
) -> Option<&'a str> {
    match entry.get(field) {
        Some(serde_json::Value::String(value)) if allowed.contains(&value.as_str()) => Some(value),
        Some(serde_json::Value::String(value)) => {
            caveats.push(format!(
                "unknown_check_axis: MERGE_GATE.json checks[{index}].{field} `{value}` is outside {}",
                allowed.join("/")
            ));
            None
        }
        Some(other) => {
            caveats.push(format!(
                "unreadable_check_axis: MERGE_GATE.json checks[{index}].{field} is {}, not a string",
                json_type_name(other)
            ));
            None
        }
        None => {
            caveats.push(format!(
                "missing_check_axis: MERGE_GATE.json schema 2.3+ requires checks[{index}].{field}"
            ));
            None
        }
    }
}

fn read_required_inline_axis<'a>(
    inline: &'a serde_json::Map<String, serde_json::Value>,
    field: &str,
    allowed: &[&str],
    caveats: &mut Vec<String>,
) -> Option<&'a str> {
    match inline.get(field) {
        Some(serde_json::Value::String(value)) if allowed.contains(&value.as_str()) => Some(value),
        Some(serde_json::Value::String(value)) => {
            caveats.push(format!(
                "unknown_inline_axis: MERGE_GATE.json inline_findings.{field} `{value}` is outside {}",
                allowed.join("/")
            ));
            None
        }
        Some(other) => {
            caveats.push(format!(
                "unreadable_inline_axis: MERGE_GATE.json inline_findings.{field} is {}, not a string",
                json_type_name(other)
            ));
            None
        }
        None => {
            caveats.push(format!(
                "missing_inline_axis: MERGE_GATE.json schema 2.3+ requires inline_findings.{field}"
            ));
            None
        }
    }
}

fn read_required_inline_count(
    inline: &serde_json::Map<String, serde_json::Value>,
    field: &str,
    caveats: &mut Vec<String>,
) -> Option<u64> {
    match inline.get(field).and_then(serde_json::Value::as_u64) {
        Some(value) => Some(value),
        None => {
            caveats.push(format!(
                "unreadable_inline_count: MERGE_GATE.json schema 2.3+ requires a non-negative integer inline_findings.{field}"
            ));
            None
        }
    }
}

fn read_pack_policy_mode<'a>(
    policy: Option<&'a serde_json::Value>,
    required: bool,
    caveats: &mut Vec<String>,
) -> Option<&'a str> {
    match policy {
        Some(serde_json::Value::Object(policy)) => match policy.get("mode") {
            Some(serde_json::Value::String(mode))
                if matches!(mode.as_str(), "shadow" | "warn" | "block") =>
            {
                Some(mode.as_str())
            }
            Some(_) => {
                caveats.push(
                    "unreadable_policy_mode: MERGE_GATE.json policy.mode must be one of \
                     shadow/warn/block"
                        .to_string(),
                );
                None
            }
            None if required => {
                caveats.push(
                    "missing_policy_mode: MERGE_GATE.json schema 2.3+ requires policy.mode"
                        .to_string(),
                );
                None
            }
            None => None,
        },
        Some(_) => {
            caveats.push("unreadable_policy: MERGE_GATE.json policy must be an object".to_string());
            None
        }
        None if required => {
            caveats.push("missing_policy: MERGE_GATE.json schema 2.3+ requires policy".to_string());
            None
        }
        None => None,
    }
}

/// Whether an inline aggregate's raw status, typed effective class, and count
/// provenance can all have been emitted together.
///
/// This is a possibility check, not a class derivation. In particular, an
/// all-pre-existing aggregate may still be INFO/FAIL when baseline trust does
/// not apply; the counts only rule out classes that no assignment of finding
/// levels and trust provenance can produce.
fn inline_class_counts_coherent(
    status: &str,
    effective_class: &str,
    findings: u64,
    introduced: u64,
    preexisting: u64,
) -> bool {
    if introduced
        .checked_add(preexisting)
        .is_none_or(|classified| classified > findings)
    {
        return false;
    }

    match status {
        "passed" | "notrun" => findings == 0 && effective_class == "PASS",
        "warnings" if findings > 0 => match effective_class {
            "INFO" => true,
            "PASS" => introduced == 0 && findings == preexisting,
            _ => false,
        },
        "failed" if findings > 0 => match effective_class {
            "FAIL" => true,
            // Raw failure needs a pre-existing error while a distinct warning
            // remains effective; one finding cannot supply both facts.
            "INFO" => findings >= 2 && preexisting >= 1,
            "PASS" => introduced == 0 && findings == preexisting,
            _ => false,
        },
        _ => false,
    }
}

pub(crate) fn read_pack_warning_tally(
    checks: Option<&serde_json::Value>,
    inline_findings: Option<&serde_json::Value>,
    policy: Option<&serde_json::Value>,
    quality_failure_details: Option<&serde_json::Value>,
    required: bool,
    caveats: &mut Vec<String>,
) -> PackWarningTally {
    let policy_caveat_count = caveats.len();
    let policy_mode = read_pack_policy_mode(policy, required, caveats);
    let mut metadata_unreadable = caveats.len() != policy_caveat_count;
    let mut failure_details: std::collections::HashMap<String, Vec<String>> =
        std::collections::HashMap::new();
    let mut preexisting_warning_details: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();
    let mut warning_details: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();
    let mut failed_check_counts: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();
    let mut warning_check_counts: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();
    let mut has_new_quality_failure_signal = false;
    match quality_failure_details {
        Some(serde_json::Value::Array(details)) => {
            for (index, detail) in details.iter().enumerate() {
                let Some(detail) = detail.as_object() else {
                    metadata_unreadable = true;
                    caveats.push(format!(
                        "unreadable_quality_failure_detail: MERGE_GATE.json \
                         decision.quality_failure_details[{index}] is not an object"
                    ));
                    continue;
                };
                let name = detail.get("name").and_then(serde_json::Value::as_str);
                let classification = detail
                    .get("classification")
                    .and_then(serde_json::Value::as_str);
                let origin = detail.get("origin").and_then(serde_json::Value::as_str);
                let valid = name.is_some_and(|name| !name.trim().is_empty())
                    && matches!(
                        classification,
                        Some("introduced" | "pre-existing" | "mixed" | "unclassified")
                    )
                    && matches!(origin, Some("failure" | "warning"));
                if !valid {
                    metadata_unreadable = true;
                    caveats.push(format!(
                        "unreadable_quality_failure_detail: MERGE_GATE.json \
                         decision.quality_failure_details[{index}] is outside the typed contract"
                    ));
                    continue;
                }
                if origin == Some("failure") {
                    failure_details
                        .entry(name.unwrap().to_string())
                        .or_default()
                        .push(classification.unwrap().to_string());
                    if classification != Some("pre-existing") {
                        has_new_quality_failure_signal = true;
                    }
                } else if origin == Some("warning") {
                    let name = name.unwrap().to_string();
                    *warning_details.entry(name.clone()).or_default() += 1;
                    if classification == Some("pre-existing") {
                        *preexisting_warning_details.entry(name).or_default() += 1;
                    }
                }
            }
        }
        Some(_) => {
            metadata_unreadable = true;
            caveats.push(
                "unreadable_quality_failure_details: MERGE_GATE.json \
                 decision.quality_failure_details must be an array"
                    .to_string(),
            );
        }
        None if required => {
            metadata_unreadable = true;
            caveats.push(
                "missing_quality_failure_details: MERGE_GATE.json schema 2.3+ requires \
                 decision.quality_failure_details"
                    .to_string(),
            );
        }
        None => {}
    }
    let mut tally = match checks {
        Some(serde_json::Value::Array(entries)) => {
            let mut tally = PackWarningTally::default();
            let mut unreadable = Vec::new();
            for (index, entry) in entries.iter().enumerate() {
                let status = entry.get("status").and_then(serde_json::Value::as_str);
                match status {
                    Some("warnings") => {
                        tally.warned += 1;
                        tally.has_explicit_warnings = true;
                    }
                    Some(status) if crate::checks::CheckStatus::EMITTED.contains(&status) => {}
                    _ => {
                        tally.warned += 1;
                        tally.has_unreadable_signal = true;
                        unreadable.push(
                            entry
                                .get("id")
                                .and_then(serde_json::Value::as_str)
                                .map(str::to_string)
                                .unwrap_or_else(|| format!("checks[{index}]")),
                        );
                    }
                }
                let blocking = entry.get("blocking").and_then(serde_json::Value::as_bool);
                let check_name = entry.get("name").and_then(serde_json::Value::as_str);
                if status == Some("warnings")
                    && let Some(name) = check_name
                {
                    *warning_check_counts.entry(name.to_string()).or_default() += 1;
                }
                let failed_or_error = matches!(status, Some("failed" | "error"));
                if failed_or_error {
                    if let Some(name) = check_name {
                        *failed_check_counts.entry(name.to_string()).or_default() += 1;
                    } else {
                        tally.has_unreadable_signal = true;
                        tally.has_new_quality_failure_signal = true;
                        has_new_quality_failure_signal = true;
                        caveats.push(format!(
                            "missing_failed_check_name: MERGE_GATE.json checks[{index}] cannot be \
                             matched to decision.quality_failure_details"
                        ));
                    }
                }
                match entry.get("blocking") {
                    Some(serde_json::Value::Bool(true)) => tally.has_blocking_signal = true,
                    Some(serde_json::Value::Bool(false)) => {}
                    Some(_) => {
                        tally.has_unreadable_signal = true;
                        caveats.push(format!(
                            "unreadable_check_blocking: MERGE_GATE.json checks[{index}].blocking \
                             is not a boolean"
                        ));
                    }
                    None if required => {
                        tally.has_unreadable_signal = true;
                        caveats.push(format!(
                            "missing_check_blocking: MERGE_GATE.json schema 2.3+ requires \
                             checks[{index}].blocking"
                        ));
                    }
                    None => {}
                }
                if required {
                    let caveat_count = caveats.len();
                    let execution_state = read_required_check_axis(
                        entry,
                        index,
                        "execution_state",
                        &["executed", "skipped", "unavailable", "unknown"],
                        caveats,
                    );
                    let outcome = read_required_check_axis(
                        entry,
                        index,
                        "outcome",
                        &[
                            "passed",
                            "findings_failed",
                            "findings_warning",
                            "system_error",
                            "skipped",
                            "unavailable",
                            "unknown",
                        ],
                        caveats,
                    );
                    let gate_class = read_required_check_axis(
                        entry,
                        index,
                        "class",
                        &["PASS", "SKIP", "FAIL", "INFO"],
                        caveats,
                    );
                    let severity = read_required_check_axis(
                        entry,
                        index,
                        "severity",
                        &["block", "warn", "ignore"],
                        caveats,
                    );
                    let conclusion = read_required_check_axis(
                        entry,
                        index,
                        "policy_conclusion",
                        &["satisfied", "advisory", "blocked"],
                        caveats,
                    );
                    let confidence = read_required_check_axis(
                        entry,
                        index,
                        "confidence_impact",
                        &["complete", "degraded", "incomplete"],
                        caveats,
                    );
                    let merge_impact = read_required_check_axis(
                        entry,
                        index,
                        "merge_impact",
                        &["approve", "review_required", "block"],
                        caveats,
                    );
                    if caveats.len() != caveat_count {
                        tally.has_unreadable_signal = true;
                    }
                    if confidence.is_some_and(|value| value != "complete")
                        || (outcome != Some("findings_warning")
                            && merge_impact == Some("review_required"))
                    {
                        tally.has_review_signal = true;
                    }
                    if conclusion == Some("blocked") || merge_impact == Some("block") {
                        tally.has_blocking_signal = true;
                    }
                    let matched_failure_details = check_name
                        .and_then(|name| failure_details.get(name))
                        .filter(|details| details.len() == 1);
                    let preexisting_failure =
                        matched_failure_details.is_some_and(|details| details[0] == "pre-existing");
                    let preexisting_warning = status == Some("warnings")
                        && check_name
                            .is_some_and(|name| preexisting_warning_details.get(name) == Some(&1));
                    if failed_or_error && matched_failure_details.is_none() {
                        tally.has_unreadable_signal = true;
                        has_new_quality_failure_signal = true;
                        caveats.push(format!(
                            "unmatched_failed_check: MERGE_GATE.json checks[{index}] requires \
                             exactly one same-name origin=failure quality_failure_details entry"
                        ));
                    }
                    let preexisting_downgrade = ((failed_or_error && preexisting_failure)
                        || preexisting_warning)
                        && conclusion == Some("advisory")
                        && merge_impact == Some("approve")
                        && blocking == Some(false);
                    if failed_or_error && merge_impact == Some("approve") && !preexisting_downgrade
                    {
                        tally.has_unreadable_signal = true;
                        has_new_quality_failure_signal = true;
                        caveats.push(format!(
                            "unproven_preexisting_downgrade: MERGE_GATE.json checks[{index}] may \
                             approve a failed/error result only with typed pre-existing failure \
                             provenance and the advisory/approve/nonblocking tuple"
                        ));
                    }
                    let expected_check_blocking = if preexisting_downgrade {
                        Some(false)
                    } else if status != Some("skipped") {
                        match (policy_mode, severity, gate_class) {
                            (Some("shadow"), Some(_), Some(_)) => Some(false),
                            (Some("warn"), Some("block"), Some("FAIL")) => Some(true),
                            (Some("warn"), Some(_), Some(_)) => Some(false),
                            (Some("block"), Some("block" | "warn"), Some("FAIL")) => Some(true),
                            (Some("block"), Some(_), Some(_)) => Some(false),
                            _ => None,
                        }
                    } else {
                        None
                    };
                    if expected_check_blocking == Some(true) {
                        tally.has_blocking_signal = true;
                    }
                    if expected_check_blocking.is_some() && blocking != expected_check_blocking {
                        tally.has_unreadable_signal = true;
                        caveats.push(format!(
                            "inconsistent_check_policy_blocking: MERGE_GATE.json policy.mode, \
                             checks[{index}].severity, and class contradict checks[{index}].blocking"
                        ));
                    }
                    let status_outcome_coherent = matches!(
                        (status, outcome),
                        (Some("passed"), Some("passed"))
                            | (Some("failed"), Some("findings_failed"))
                            | (Some("warnings"), Some("findings_warning"))
                            | (Some("error"), Some("system_error"))
                            | (Some("skipped"), Some("skipped" | "unavailable" | "unknown"))
                    );
                    let execution_outcome_coherent = matches!(
                        (execution_state, outcome),
                        (
                            Some("executed"),
                            Some(
                                "passed" | "findings_failed" | "findings_warning" | "system_error"
                            )
                        ) | (Some("skipped"), Some("skipped"))
                            | (Some("unavailable"), Some("unavailable"))
                            | (Some("unknown"), Some("unknown"))
                    );
                    let system_error_coherent = outcome != Some("system_error")
                        || (confidence == Some("incomplete") && conclusion != Some("satisfied"));
                    let finding_conclusion_coherent = !matches!(
                        outcome,
                        Some("findings_failed" | "findings_warning" | "system_error")
                    ) || conclusion != Some("satisfied");
                    let conclusion_merge_coherent = matches!(
                        (conclusion, merge_impact),
                        (Some("blocked"), Some("block"))
                            | (Some("advisory"), Some("review_required"))
                            | (Some("satisfied"), Some("approve" | "review_required"))
                    ) || preexisting_downgrade;
                    let status_class_coherent = matches!(
                        (status, gate_class),
                        (Some("passed"), Some("PASS"))
                            | (Some("failed" | "error"), Some("FAIL"))
                            | (Some("warnings"), Some("INFO"))
                            | (Some("skipped"), Some("SKIP"))
                    );
                    let unavailable_skip_coherent = if status == Some("skipped")
                        && matches!(execution_state, Some("unavailable" | "unknown"))
                    {
                        matches!(
                            (severity, conclusion, confidence, merge_impact),
                            (
                                Some("warn"),
                                Some("advisory"),
                                Some("degraded"),
                                Some("review_required")
                            ) | (
                                Some("ignore"),
                                Some("satisfied"),
                                Some("complete"),
                                Some("approve")
                            ) | (
                                Some("block"),
                                Some("blocked"),
                                Some("incomplete"),
                                Some("block")
                            )
                        )
                    } else {
                        true
                    };
                    if !status_outcome_coherent
                        || !execution_outcome_coherent
                        || !system_error_coherent
                        || !finding_conclusion_coherent
                        || !conclusion_merge_coherent
                        || !status_class_coherent
                        || !unavailable_skip_coherent
                    {
                        tally.has_unreadable_signal = true;
                        tally.has_review_signal = true;
                        caveats.push(format!(
                            "inconsistent_check_tuple: MERGE_GATE.json checks[{index}] status, execution_state, outcome, confidence_impact, and policy_conclusion cannot be emitted together"
                        ));
                    }
                    let block_tuple = (
                        blocking == Some(true),
                        conclusion == Some("blocked"),
                        merge_impact == Some("block"),
                    );
                    if blocking.is_some()
                        && conclusion.is_some()
                        && merge_impact.is_some()
                        && block_tuple != (false, false, false)
                        && block_tuple != (true, true, true)
                    {
                        tally.has_unreadable_signal = true;
                        caveats.push(format!(
                            "inconsistent_check_blocking: MERGE_GATE.json checks[{index}] requires policy_conclusion=blocked iff merge_impact=block iff blocking=true"
                        ));
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
            for (name, details) in &failure_details {
                let check_count = failed_check_counts.get(name).copied().unwrap_or(0);
                if check_count != details.len() {
                    tally.has_unreadable_signal = true;
                    if details
                        .iter()
                        .any(|classification| classification != "pre-existing")
                    {
                        has_new_quality_failure_signal = true;
                    }
                    caveats.push(format!(
                        "unmatched_quality_failure_detail: MERGE_GATE.json has {} origin=failure \
                         detail(s) for `{name}` but {check_count} failed/error check row(s)",
                        details.len()
                    ));
                }
            }
            for (name, detail_count) in &warning_details {
                let check_count = warning_check_counts.get(name).copied().unwrap_or(0);
                if check_count != *detail_count {
                    tally.warned = tally.warned.max(1);
                    tally.has_unreadable_signal = true;
                    tally.has_review_signal = true;
                    caveats.push(format!(
                        "unmatched_warning_detail: MERGE_GATE.json has {detail_count} \
                         origin=warning detail(s) for `{name}` but {check_count} warning check row(s)"
                    ));
                }
            }
            tally
        }
        Some(other) => {
            caveats.push(format!(
                "unreadable_checks: MERGE_GATE.json checks is {}, not an array; the warning tally \
                 cannot be read and counts as at least one warning",
                json_type_name(other)
            ));
            PackWarningTally {
                warned: 1,
                has_explicit_warnings: false,
                has_unreadable_signal: true,
                has_review_signal: false,
                has_blocking_signal: false,
                has_new_quality_failure_signal: false,
            }
        }
        None if required => {
            caveats.push(
                "missing_checks: MERGE_GATE.json schema 2.3+ requires the canonical checks array; \
                 its warning state is unreadable"
                    .to_string(),
            );
            PackWarningTally {
                warned: 1,
                has_explicit_warnings: false,
                has_unreadable_signal: true,
                has_review_signal: false,
                has_blocking_signal: false,
                has_new_quality_failure_signal: false,
            }
        }
        None => PackWarningTally::default(),
    };

    tally.has_unreadable_signal |= metadata_unreadable;
    tally.has_new_quality_failure_signal = has_new_quality_failure_signal;
    tally.has_review_signal |= has_new_quality_failure_signal;

    match inline_findings {
        Some(serde_json::Value::Object(inline)) => {
            let blocking = match inline.get("blocking") {
                Some(serde_json::Value::Bool(value)) => Some(*value),
                Some(_) => {
                    tally.has_unreadable_signal = true;
                    caveats.push(
                        "unreadable_inline_blocking: MERGE_GATE.json inline_findings.blocking is \
                         not a boolean"
                            .to_string(),
                    );
                    None
                }
                None if required => {
                    tally.has_unreadable_signal = true;
                    caveats.push(
                        "missing_inline_blocking: MERGE_GATE.json schema 2.3+ requires \
                         inline_findings.blocking"
                            .to_string(),
                    );
                    None
                }
                None => None,
            };
            let normalized_status = match inline.get("status") {
                Some(serde_json::Value::String(status)) => {
                    let status = status.trim().replace('_', "").to_ascii_lowercase();
                    match status.as_str() {
                        "warnings" => {
                            tally.warned += 1;
                            tally.has_explicit_warnings = true;
                        }
                        "failed" | "passed" | "notrun" => {}
                        _ => {
                            tally.warned += 1;
                            tally.has_unreadable_signal = true;
                            caveats.push(
                            "unreadable_inline_status: MERGE_GATE.json inline_findings.status is \
                             outside passed/warnings/failed/not_run"
                                .to_string(),
                            );
                        }
                    }
                    Some(status)
                }
                Some(_) => {
                    tally.warned += 1;
                    tally.has_unreadable_signal = true;
                    caveats.push(
                        "unreadable_inline_status: MERGE_GATE.json inline_findings.status must be a \
                         string"
                            .to_string(),
                    );
                    None
                }
                None => {
                    tally.warned += 1;
                    tally.has_unreadable_signal = true;
                    caveats.push(
                        "missing_inline_status: present MERGE_GATE.json inline_findings requires a \
                         status field"
                            .to_string(),
                    );
                    None
                }
            };

            if required {
                let caveat_count = caveats.len();
                let effective_class = read_required_inline_axis(
                    inline,
                    "effective_class",
                    &["PASS", "INFO", "FAIL"],
                    caveats,
                );
                let inline_disposition = read_required_inline_axis(
                    inline,
                    "enforcement_disposition",
                    &["clean", "warnings_only", "review_required", "block"],
                    caveats,
                );
                let severity = read_required_inline_axis(
                    inline,
                    "severity",
                    &["block", "warn", "ignore"],
                    caveats,
                );
                let findings = read_required_inline_count(inline, "findings_count", caveats);
                let introduced = read_required_inline_count(inline, "introduced_count", caveats);
                let preexisting = read_required_inline_count(inline, "preexisting_count", caveats);
                if caveats.len() != caveat_count {
                    tally.has_unreadable_signal = true;
                }

                match inline_disposition {
                    Some("warnings_only") => {
                        tally.has_explicit_warnings = true;
                        if normalized_status.as_deref() != Some("warnings") {
                            tally.warned += 1;
                        }
                    }
                    Some("review_required") => tally.has_review_signal = true,
                    Some("block") => tally.has_blocking_signal = true,
                    Some("clean") | None => {}
                    Some(_) => unreachable!("closed inline disposition vocabulary"),
                }
                if blocking == Some(true) {
                    tally.has_blocking_signal = true;
                }

                let expected_blocking = match (policy_mode, severity, effective_class) {
                    (Some("shadow"), Some(_), Some(_)) => Some(false),
                    (Some("warn"), Some("block"), Some("FAIL")) => Some(true),
                    (Some("warn"), Some(_), Some(_)) => Some(false),
                    (Some("block"), Some("block" | "warn"), Some("FAIL")) => Some(true),
                    (Some("block"), Some(_), Some(_)) => Some(false),
                    _ => None,
                };
                if expected_blocking == Some(true) {
                    tally.has_blocking_signal = true;
                }
                if expected_blocking.is_some() && blocking != expected_blocking {
                    tally.has_unreadable_signal = true;
                    caveats.push(
                        "inconsistent_inline_blocking: MERGE_GATE.json policy.mode, inline \
                         severity, and effective_class contradict inline_findings.blocking"
                            .to_string(),
                    );
                }
                let expected_disposition = match (
                    effective_class,
                    expected_blocking,
                    normalized_status.as_deref(),
                ) {
                    (_, Some(true), _) => Some("block"),
                    (Some("FAIL"), Some(false), _) => Some("review_required"),
                    (Some("INFO"), Some(false), _) => Some("warnings_only"),
                    (Some("PASS"), Some(false), Some("warnings")) => Some("warnings_only"),
                    (Some("PASS"), Some(false), _) => Some("clean"),
                    (Some("FAIL"), None, _) => Some("review_required"),
                    (Some("INFO"), None, _) => Some("warnings_only"),
                    (Some("PASS"), None, Some("warnings")) => Some("warnings_only"),
                    (Some("PASS"), None, _) => Some("clean"),
                    _ => None,
                };
                if expected_disposition.is_some() && inline_disposition != expected_disposition {
                    tally.has_unreadable_signal = true;
                    caveats.push(
                        "inconsistent_inline_enforcement: MERGE_GATE.json inline_findings \
                         effective_class/blocking contradict enforcement_disposition"
                            .to_string(),
                    );
                }

                if let (Some(findings), Some(introduced), Some(preexisting)) =
                    (findings, introduced, preexisting)
                {
                    if introduced
                        .checked_add(preexisting)
                        .is_none_or(|classified| classified > findings)
                    {
                        tally.has_unreadable_signal = true;
                        caveats.push(
                            "inconsistent_inline_counts: introduced_count + preexisting_count \
                             exceeds findings_count"
                                .to_string(),
                        );
                    }
                    let class_counts_coherent = normalized_status
                        .as_deref()
                        .zip(effective_class)
                        .is_some_and(|(status, class)| {
                            inline_class_counts_coherent(
                                status,
                                class,
                                findings,
                                introduced,
                                preexisting,
                            )
                        });
                    if !class_counts_coherent {
                        tally.has_unreadable_signal = true;
                        tally.has_review_signal = true;
                        caveats.push(
                            "inconsistent_inline_class: MERGE_GATE.json inline status/counts \
                             cannot accompany its effective_class"
                                .to_string(),
                        );
                    }
                }
            } else if normalized_status.as_deref() == Some("warnings") {
                // Legacy packs have no typed effective inline class. Preserve
                // their warning tally without allowing a <=2.2 field injection
                // to unlock the new strict exception.
                tally.has_explicit_warnings = true;
            }
        }
        Some(_) => {
            tally.has_unreadable_signal = true;
            caveats.push(
                "unreadable_inline_findings: MERGE_GATE.json schema 2.3+ requires an inline_findings \
                 object"
                    .to_string(),
            );
        }
        None if required => {
            tally.has_unreadable_signal = true;
            caveats.push(
                "missing_inline_findings: MERGE_GATE.json schema 2.3+ requires inline_findings"
                    .to_string(),
            );
        }
        None => {}
    }
    if tally.has_unreadable_signal && tally.warned == 0 {
        // `--fail-on-warnings` is intentionally conservative when the pack's
        // warning state cannot be proved readable, while strict mode uses the
        // separate review-required disposition rather than laundering this
        // uncertainty into the warnings-only exception.
        tally.warned = 1;
    }
    tally
}

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
    Array,
}

impl JsonKind {
    fn matches(self, value: &serde_json::Value) -> bool {
        match self {
            Self::String => value.is_string(),
            Self::Boolean => value.is_boolean(),
            Self::Array => value.is_array(),
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::String => "a string",
            Self::Boolean => "a boolean",
            Self::Array => "an array",
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

/// Conservativeness rank of a stated `analysis_status`, or `None` when the
/// value states nothing this contract can rank.
///
/// `complete` is a PRECONDITION of `PASS`, not a grant of it: a complete
/// analysis still ends at `BLOCK` when policy blocks, so reading it as rank 1
/// would let one axis soften a verdict the others agree on — the same asymmetry
/// as `quality_pass: true`. `degraded` and `incomplete` rule `PASS` out, so both
/// rank 2. Anything else is outside the vocabulary and cannot rank at all;
/// callers name it with an `unknown_analysis_status:` caveat rather than
/// letting it vanish, exactly as they do for `merge_recommendation`.
pub(crate) fn rank_from_analysis_status(s: &str) -> Option<u8> {
    match s {
        "degraded" | "incomplete" => Some(2),
        _ => None,
    }
}

/// Whether a stated `analysis_status` is one this contract defines.
pub(crate) fn known_analysis_status(s: &str) -> bool {
    matches!(s, "complete" | "degraded" | "incomplete")
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
    pub fail_on_warnings: bool,
    pub enforcement_disposition: EnforcementDisposition,
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
    #[serde(default)]
    decision_reason: Option<String>,
    #[serde(default)]
    review_caveats: Vec<String>,
    #[serde(default)]
    blocking_issues: Vec<String>,
}

/// Conservative compatibility mapping for callers that only have the legacy
/// verdict and therefore cannot prove a CONDITIONAL is warnings-only.
pub fn gate_exit_code(verdict: GateVerdict, strict: bool) -> i32 {
    let disposition = match verdict {
        GateVerdict::Pass => EnforcementDisposition::Clean,
        GateVerdict::Conditional => EnforcementDisposition::ReviewRequired,
        GateVerdict::Block => EnforcementDisposition::Block,
    };
    gate_exit_code_for_disposition(disposition, EnforcementMode::from_gate_flags(strict, false))
}

pub fn gate_exit_code_for_disposition(
    disposition: EnforcementDisposition,
    mode: EnforcementMode,
) -> i32 {
    match disposition.action(mode) {
        EnforcementAction::Accept => 0,
        EnforcementAction::Reject => 2,
        EnforcementAction::Block => 1,
    }
}

pub fn build_gate_json_output(
    summary: &CliJsonSummary,
    merge_gate_path: &Path,
    mode: EnforcementMode,
) -> Result<GateJsonOutput> {
    let decision = read_merge_gate_decision(merge_gate_path)?;
    // `CliJsonSummary` is the shared normalized reader result. The gate adapter
    // consumes it instead of deserializing a second, stricter copy of the
    // decision axes that could disagree on additive/legacy fields.
    let verdict = GateVerdict::try_from(summary.verdict.as_str())?;
    let exit_code = gate_exit_code_for_disposition(summary.enforcement_disposition, mode);
    let mut caveats = summary.caveats.clone();
    for caveat in decision.review_caveats {
        if !caveats.contains(&caveat) {
            caveats.push(caveat);
        }
    }

    Ok(GateJsonOutput {
        schema_version: "gate-json/v1",
        verdict,
        exit_code,
        strict: mode.is_strict(),
        fail_on_warnings: mode.fails_on_warnings(),
        enforcement_disposition: summary.enforcement_disposition,
        status: summary.status.to_string(),
        analysis_status: summary.analysis_status,
        merge_recommendation: summary.merge_recommendation,
        allow_merge: summary.allow_merge,
        quality_pass: summary.quality_pass,
        output_dir: summary.output_dir.clone(),
        merge_gate_json: merge_gate_path.display().to_string(),
        caveats,
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
    fn operator_policy_exit_matrix() {
        use crate::output::cli_exit_code_for_disposition;
        use crate::policy::engine::{EnforcementDisposition as D, EnforcementMode as M};

        // API addition is deliberately a second clean row: it pins D2 in the
        // same operator table instead of manufacturing a special API verdict.
        let rows = [
            ("clean", D::Clean, true, 0, [0, 0, 0], [0, 0, 0]),
            (
                "warnings-only",
                D::WarningsOnly,
                true,
                1,
                [0, 0, 2],
                [0, 0, 1],
            ),
            ("api-addition-only", D::Clean, true, 0, [0, 0, 0], [0, 0, 0]),
            (
                "confirmed-breaking",
                D::ReviewRequired,
                true,
                0,
                [0, 2, 2],
                [0, 0, 0],
            ),
            (
                "potential-breaking",
                D::ReviewRequired,
                true,
                0,
                [0, 2, 2],
                [0, 0, 0],
            ),
            ("unknown", D::ReviewRequired, true, 0, [0, 2, 2], [0, 0, 0]),
            ("degraded", D::ReviewRequired, true, 0, [0, 2, 2], [0, 0, 0]),
            (
                "quality-failure",
                D::ReviewRequired,
                false,
                0,
                [0, 2, 2],
                [0, 1, 1],
            ),
            ("block", D::Block, false, 0, [1, 1, 1], [1, 1, 1]),
        ];
        let gate_modes = [M::Advisory, M::GateStrict, M::GateFailOnWarnings];
        let ci_modes = [M::Advisory, M::Ci, M::CiFailOnWarnings];

        for (name, disposition, quality_pass, warned, expected_gate, expected_cli) in rows {
            for (index, mode) in gate_modes.into_iter().enumerate() {
                assert_eq!(
                    gate_exit_code_for_disposition(disposition, mode),
                    expected_gate[index],
                    "gate row={name} mode={mode:?}"
                );
            }
            for (index, mode) in ci_modes.into_iter().enumerate() {
                assert_eq!(
                    cli_exit_code_for_disposition(disposition, mode, warned, quality_pass),
                    expected_cli[index],
                    "ci row={name} mode={mode:?}"
                );
            }
        }

        // The opt-in lane keys off the canonical pack tally even when a policy
        // ignored the warning and emitted a clean disposition.
        assert_eq!(
            cli_exit_code_for_disposition(D::Clean, M::CiFailOnWarnings, 1, true),
            1
        );
        assert_eq!(
            cli_exit_code_for_disposition(D::ReviewRequired, M::CiFailOnWarnings, 1, true,),
            1,
            "warnings-clean CI must reject a mixed warning plus review requirement"
        );
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
