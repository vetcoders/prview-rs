//! INLINE_FINDINGS generation and gate-class helpers.

use super::*;

#[derive(Debug)]
pub(super) struct InlineFindingsSummary {
    pub(super) status: String,
    pub(super) findings_count: usize,
    pub(super) dashboard_findings: Vec<DashboardFinding>,
    /// Cargo audit's baseline comparison, as structured data rather than the
    /// rendered `Cargo audit baseline: …` note.
    ///
    /// The gate needs the same numbers the caveat prints in order to SAY what it
    /// decided and why — the incident this exists for had the caveat reporting
    /// `new=0, pre-existing=2` beside a bare `Cargo audit (Failed)` blocker.
    /// Re-parsing the note would make the message layer depend on the wording of
    /// a human-facing string, so the counts travel here and the note is rendered
    /// from the same values.
    ///
    /// `None` when the run had no `Cargo audit` check at all.
    pub(super) cargo_audit: Option<CargoAuditGateEvidence>,
}

/// What cargo audit's baseline comparison concluded, for the surfaces that must
/// explain the verdict rather than merely reach it.
#[derive(Debug, Clone)]
pub(super) struct CargoAuditGateEvidence {
    /// `not-required` | `available` | `unavailable` | `current-unavailable`,
    /// verbatim from [`CargoAuditBaselineCounts`].
    pub(super) status: &'static str,
    pub(super) new: usize,
    pub(super) preexisting: usize,
    pub(super) unknown: usize,
    /// Whether the diff touched the effective `Cargo.lock`. Selects which proof
    /// the advisory reason cites: an untouched lock, or an unchanged comparison
    /// against the base audit.
    pub(super) lock_changed: bool,
    /// Advisory ids this diff introduced, in report order and deduplicated.
    /// Empty whenever `new == 0`.
    pub(super) new_advisory_ids: Vec<String>,
}

pub(super) fn is_operator_finding(finding: &DashboardFinding) -> bool {
    matches!(finding.level, "error" | "warning")
}

/// The operator-finding list: the canonical rows that are diagnostics rather
/// than evidence about the run.
///
/// This is the only place the predicate is applied to a whole summary, and the
/// rows it keeps are exactly the ones emitted as SARIF results — informational
/// notes stay in `InlineFindingsSummary::dashboard_findings` but never reach
/// the SARIF file, so counting them would make `quality.sarif.findings_count`
/// describe a file that does not contain them. Build
/// `DashboardContext::findings` through this function rather
/// than copying the unfiltered list, so the current count, the run history and
/// the previous-run delta cannot drift apart.
pub(super) fn operator_findings(all: &[DashboardFinding]) -> Vec<DashboardFinding> {
    all.iter()
        .filter(|finding| is_operator_finding(finding))
        .cloned()
        .collect()
}

/// The origin tri-state every SARIF result carries, per
/// `docs/contracts/merge_gate.md`.
///
/// `introduced` means the tool reported the finding in a file this diff
/// touches — a location signal, not proof the change created it. `preexisting`
/// means it reported it outside those files. `unclassified` means the origin
/// was not established; it is not a pass, and a consumer must not read it as
/// one. Every emitter uses this mapping so `properties.classification` cannot
/// disagree with `properties.in_diff`.
pub(super) fn origin_classification(in_diff: Option<bool>) -> &'static str {
    match in_diff {
        Some(true) => "introduced",
        Some(false) => "preexisting",
        None => "unclassified",
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CargoAuditBaselineCounts {
    new: usize,
    preexisting: usize,
    resolved: usize,
    unknown: usize,
    status: &'static str,
}

fn cargo_audit_baseline_counts(
    current: Option<&std::collections::HashSet<(String, String, String)>>,
    cargo_lock_changed: bool,
    base: Option<&std::collections::HashSet<(String, String, String)>>,
) -> CargoAuditBaselineCounts {
    let Some(current) = current else {
        return CargoAuditBaselineCounts {
            new: 0,
            preexisting: 0,
            resolved: 0,
            unknown: 0,
            status: "current-unavailable",
        };
    };

    if !cargo_lock_changed {
        CargoAuditBaselineCounts {
            new: 0,
            preexisting: current.len(),
            resolved: 0,
            unknown: 0,
            status: "not-required",
        }
    } else if let Some(base) = base {
        CargoAuditBaselineCounts {
            new: current.difference(base).count(),
            preexisting: current.intersection(base).count(),
            resolved: base.difference(current).count(),
            unknown: 0,
            status: "available",
        }
    } else {
        CargoAuditBaselineCounts {
            new: 0,
            preexisting: 0,
            resolved: 0,
            unknown: current.len(),
            status: "unavailable",
        }
    }
}

fn cargo_audit_finding_in_diff(
    key: &(String, String, String),
    current_report_valid: bool,
    cargo_lock_changed: bool,
    base: Option<&std::collections::HashSet<(String, String, String)>>,
) -> Option<bool> {
    if !current_report_valid {
        None
    } else if !cargo_lock_changed {
        Some(false)
    } else {
        base.map(|known| !known.contains(key))
    }
}

/// Effective gate class for the aggregate INLINE_FINDINGS gate.
///
/// The raw `status` field counts *every* SARIF row, so a scan whose findings
/// are all pre-existing (outside the diff) reports `failed` and — under
/// `--policy-mode block` — used to block the merge even though every per-check
/// evaluation classified those same findings as pre-existing and approved them.
///
/// Gate instead on the findings the PR is actually responsible for: an error or
/// warning counts only when it is introduced (`in_diff == Some(true)`) or
/// unclassified (`in_diff == None` — causation unknown, so treated as new).
/// Pre-existing rows (`in_diff == Some(false)`) never gate. This mirrors THREAD
/// 4's pre-existing semantics so the inline gate agrees with the per-check
/// downgrade.
///
/// An out-of-diff row is treated as pre-existing ONLY when the check's
/// locations are an exhaustive baseline signal (`check_id_is_baseline_signal`)
/// AND the clean-comparison gate trusts that check's out-of-diff rows as
/// pre-existing for this run (`clean.applies_to`). The latter is what the
/// per-check path already applies (R3-16): on a remote/snapshot target a
/// rustfmt/ruff/eslint row came from the local checkout — a different tree than
/// the target — so it must gate, not skip; a dirty local scan (R2-9) and a run
/// with no resolved base diff (R4-20) likewise gate. Without this condition the
/// aggregate gate skipped by check id alone and disagreed with the per-check
/// classification, letting an out-of-diff blocking row PASS.
///
/// For whole-project parsers (e.g. `cargo_test`) an out-of-diff location is
/// causation-unknown — the diff may have caused it — so it still gates, exactly
/// as `classify_quality_failure` keeps it Unclassified (R2-8).
pub(super) fn effective_inline_gate_class(
    inline: &InlineFindingsSummary,
    clean: &CleanComparison,
) -> GateClass {
    let mut new_errors = 0usize;
    let mut new_warnings = 0usize;
    for finding in &inline.dashboard_findings {
        let out_of_diff_preexisting = finding.in_diff == Some(false)
            && check_id_is_baseline_signal(&finding.check_id)
            && clean.applies_to(&finding.check_id);
        if out_of_diff_preexisting {
            continue;
        }
        match finding.level {
            "error" => new_errors += 1,
            "warning" => new_warnings += 1,
            _ => {}
        }
    }
    if new_errors > 0 {
        GateClass::Fail
    } else if new_warnings > 0 {
        GateClass::Info
    } else {
        GateClass::Pass
    }
}

pub(super) struct InlineGateOutcome {
    pub severity: crate::policy::PolicySeverity,
    pub blocking: bool,
    pub class: GateClass,
}

pub(super) fn record_blocking_issue(
    blocking_issues: &mut Vec<String>,
    worst_merge: &mut crate::policy::engine::MergeRecommendation,
    issue: impl Into<String>,
) {
    blocking_issues.push(issue.into());
    *worst_merge = crate::policy::engine::MergeRecommendation::Block;
}

pub(super) fn apply_inline_gate_outcome(
    config: &Config,
    inline: &InlineFindingsSummary,
    clean: &CleanComparison,
    blocking_issues: &mut Vec<String>,
    worst_merge: &mut crate::policy::engine::MergeRecommendation,
) -> InlineGateOutcome {
    let severity = config.policy.severity_for("inline_findings");
    let class = effective_inline_gate_class(inline, clean);
    let blocking = config.policy.is_blocking(severity, class);
    if blocking {
        record_blocking_issue(
            blocking_issues,
            worst_merge,
            format!("INLINE_FINDINGS ({})", inline.status),
        );
    }
    InlineGateOutcome {
        severity,
        blocking,
        class,
    }
}

pub(super) fn gate_class_for_check(status: crate::checks::CheckStatus) -> GateClass {
    match status {
        crate::checks::CheckStatus::Passed => GateClass::Pass,
        crate::checks::CheckStatus::Skipped => GateClass::Skip,
        crate::checks::CheckStatus::Failed | crate::checks::CheckStatus::Error => GateClass::Fail,
        crate::checks::CheckStatus::Warnings => GateClass::Info,
    }
}

pub(super) fn gate_class_to_str(class: GateClass) -> &'static str {
    match class {
        GateClass::Pass => "PASS",
        GateClass::Skip => "SKIP",
        GateClass::Fail => "FAIL",
        GateClass::Info => "INFO",
    }
}

pub(super) fn coverage_has_rust_inline_test_blind_spot(coverage: &CoverageDelta) -> bool {
    coverage
        .uncovered
        .iter()
        .any(|file| file.path.ends_with(".rs"))
}

pub(super) fn skipped_requested_security_review_caveats(
    config: &Config,
    checks: &[CheckResult],
    skipped_checks: &[crate::checks::SkippedCheck],
) -> Vec<String> {
    if !config.run_security {
        return Vec::new();
    }

    let mut caveats: Vec<String> = skipped_checks
        .iter()
        .filter(|check| {
            check.id == "cargo_geiger" || check.name.eq_ignore_ascii_case("cargo geiger")
        })
        .map(|check| format!("cargo geiger skipped for this run ({})", check.reason))
        .collect();

    // A runtime Skipped (a timeout or a virtual-manifest workspace) lands in
    // `checks`, not `skipped_checks` — only the pre-run `can_run()==false` path
    // populates `skipped_checks`. Surface it too so the gate explains the skip
    // instead of silently dropping the requested security advisory.
    for check in checks {
        if check.name.eq_ignore_ascii_case("cargo geiger")
            && matches!(check.status, CheckStatus::Skipped)
        {
            let caveat = format!(
                "cargo geiger skipped for this run ({})",
                runtime_skip_reason(&check.output)
            );
            if !caveats.contains(&caveat) {
                caveats.push(caveat);
            }
        }
    }

    caveats
}

/// Concise reason for a runtime `cargo geiger` Skipped, derived from its output.
pub(super) fn runtime_skip_reason(output: &str) -> String {
    if output.contains("timed out") {
        "timed out".to_string()
    } else if output.contains("virtual manifest") {
        "virtual manifest — configure -p <pkg>".to_string()
    } else {
        "skipped at runtime".to_string()
    }
}

pub(super) fn policy_severity_to_str(level: PolicySeverity) -> &'static str {
    match level {
        PolicySeverity::Block => "block",
        PolicySeverity::Warn => "warn",
        PolicySeverity::Ignore => "ignore",
    }
}

/// Classify a commit message into a Conventional Commits type.
pub(super) fn classify_commit_type(message: &str) -> &'static str {
    let lower = message.to_lowercase();
    let first = lower.split(':').next().unwrap_or(&lower).trim();
    // Strip scope: "feat(cli)" → "feat"
    let prefix = first.split('(').next().unwrap_or(first);
    match prefix {
        "feat" | "feature" => "feat",
        "fix" | "bugfix" | "hotfix" => "fix",
        "refactor" => "refactor",
        "docs" | "doc" => "docs",
        "test" | "tests" => "test",
        "chore" | "build" | "ci" => "chore",
        "style" | "fmt" => "style",
        "perf" => "perf",
        _ => "other",
    }
}

pub(super) use crate::check_id::check_id_from_name;

pub(super) fn build_heuristics_gate_check(
    config: &Config,
    heuristics: Option<&HeuristicsResult>,
) -> (
    serde_json::Value,
    Option<String>,
    crate::policy::engine::EnforcementDisposition,
) {
    use serde_json::json;
    let result = super::build_heuristics_check(heuristics, config);
    let evaluation = crate::policy::engine::PolicyEngine::new(config).evaluate_run(&result);
    let blocking = matches!(
        evaluation.merge_impact,
        crate::policy::engine::MergeRecommendation::Block
    );
    let blocking_issue = if blocking {
        Some(format!("Loctree heuristics ({})", result.output))
    } else {
        None
    };
    let disposition = crate::policy::engine::EnforcementDisposition::from_evaluations(
        std::slice::from_ref(&evaluation),
    );

    let check = json!({
        "id": evaluation.check_id,
        "name": "Loctree Heuristics",
        "status": evaluation.raw_status,
        "execution_state": evaluation.execution_state,
        "outcome": evaluation.outcome,
        "class": gate_class_to_str(evaluation.gate_class),
        "severity": policy_severity_to_str(evaluation.severity),
        "policy_conclusion": evaluation.conclusion,
        "confidence_impact": evaluation.confidence_impact,
        "merge_impact": evaluation.merge_impact,
        "blocking": blocking,
        "duration_secs": 0.0,
        "cached": false,
        "reason": evaluation.reason,
        "evidence": "20_quality/heuristics_loctree.result.json",
        "log": "20_quality/heuristics_loctree.log",
    });

    (check, blocking_issue, disposition)
}

pub(super) fn generate_inline_findings(
    dir: &Path,
    checks: &[CheckResult],
    diffs: &[crate::git::Diff],
    repo: Option<&crate::git::Repository>,
    cargo_root: Option<&Path>,
) -> Result<InlineFindingsSummary> {
    use serde_json::json;
    use sha2::{Digest, Sha256};
    use std::collections::HashSet;

    let sarif_path = dir.join("INLINE_FINDINGS.sarif");
    let mut sarif_rules: Vec<serde_json::Value> = Vec::new();
    let mut sarif_results: Vec<serde_json::Value> = Vec::new();
    let mut known_sarif_rules = HashSet::new();
    let mut dashboard_findings = Vec::new();
    let mut error_count = 0usize;
    let mut warning_count = 0usize;

    // Build set of changed file paths from diffs for in_diff marking.
    let changed_files: HashSet<&str> = diffs
        .iter()
        .flat_map(|d| d.files.iter().map(|f| f.path.as_str()))
        .collect();

    // Compute partial fingerprint for deduplication.
    let fingerprint = |rule_id: &str, file: &str, line: u32| -> String {
        let mut hasher = Sha256::new();
        hasher.update(format!("{}:{}:{}", rule_id, file, line));
        format!("{:x}", hasher.finalize())
    };

    // Check if file path is in the diff (handles path format differences).
    let is_in_diff = |file: &str| -> bool {
        let stripped = file.trim_start_matches('/');
        changed_files.contains(file)
            || changed_files.contains(stripped)
            || changed_files
                .iter()
                .any(|cf| file.ends_with(cf) || cf.ends_with(stripped))
    };

    // Per-tool parsed findings accumulator.
    struct ToolFindings {
        source: &'static str,
        tool_name: &'static str,
        check_id: String,
        findings: Vec<parsers::LintFinding>,
    }

    let mut tool_findings_sets: Vec<ToolFindings> = Vec::new();
    let mut cargo_audit_evidence: Option<CargoAuditGateEvidence> = None;

    for check in checks {
        let check_id = check_id_from_name(&check.name);

        // Cargo audit has its own structured contract. Process it independently
        // of the raw check status so a passing head can still report advisories
        // resolved relative to the base, and never let its JSON fall through to
        // the generic file:line scraper.
        if check.name.eq_ignore_ascii_case("cargo audit") {
            let audit_findings = parse_cargo_audit_findings(&check.output);
            let current_advisories = cargo_audit_report_advisory_keys(&check.output);
            let cargo_lock_changed = cargo_audit_lock_changed(repo, diffs, cargo_root);
            let base_audit_cache = if cargo_lock_changed && current_advisories.is_some() {
                get_base_cargo_audit_findings(repo, diffs, cargo_root)?
            } else {
                None
            };
            let baseline = cargo_audit_baseline_counts(
                current_advisories.as_ref(),
                cargo_lock_changed,
                base_audit_cache.as_ref(),
            );

            dashboard_findings.push(DashboardFinding {
                file: None,
                line: None,
                level: "note",
                check_name: "Cargo audit baseline".to_string(),
                check_id: "cargo_audit_baseline".to_string(),
                message: format!(
                    "Cargo audit baseline: status={}, new={}, pre-existing={}, resolved={}, unknown-baseline={}",
                    baseline.status,
                    baseline.new,
                    baseline.preexisting,
                    baseline.resolved,
                    baseline.unknown,
                ),
                in_diff: Some(false),
            });

            // The same values the note above renders, kept structured for the
            // gate. Built before the per-finding loop so `new_advisory_ids` can
            // be filled from the identical `in_diff` decision the SARIF rows
            // carry — one classification, not two.
            let mut evidence = CargoAuditGateEvidence {
                status: baseline.status,
                new: baseline.new,
                preexisting: baseline.preexisting,
                unknown: baseline.unknown,
                lock_changed: cargo_lock_changed,
                new_advisory_ids: Vec::new(),
            };

            let location = cargo_audit_location_for_check(check);
            for finding in &audit_findings {
                match finding.sarif_level {
                    "error" => error_count += 1,
                    "warning" => warning_count += 1,
                    _ => {}
                }

                if known_sarif_rules.insert(finding.advisory_id.clone()) {
                    sarif_rules.push(json!({
                        "id": finding.advisory_id,
                        "name": "cargo audit advisory",
                        "shortDescription": { "text": finding.title },
                        "helpUri": finding.help_url,
                        "defaultConfiguration": {
                            "level": finding.sarif_level
                        },
                        "properties": {
                            "package": finding.package_display(),
                            "severity": finding.severity,
                            "patched_versions": finding.patched_versions,
                        }
                    }));
                }

                let current_audit_in_diff = cargo_audit_finding_in_diff(
                    &cargo_audit_finding_key(finding),
                    current_advisories.is_some(),
                    cargo_lock_changed,
                    base_audit_cache.as_ref(),
                );
                if current_audit_in_diff == Some(true)
                    && !evidence
                        .new_advisory_ids
                        .iter()
                        .any(|id| id == &finding.advisory_id)
                {
                    evidence.new_advisory_ids.push(finding.advisory_id.clone());
                }

                dashboard_findings.push(DashboardFinding {
                    file: None,
                    line: None,
                    level: finding.sarif_level,
                    check_name: check.name.clone(),
                    check_id: check_id.clone(),
                    message: finding.sarif_message(),
                    in_diff: current_audit_in_diff,
                });

                sarif_results.push(json!({
                    "ruleId": finding.advisory_id,
                    "level": finding.sarif_level,
                    "message": { "text": finding.sarif_message() },
                    "locations": [{
                        "physicalLocation": {
                            "artifactLocation": { "uri": &location },
                            "region": { "startLine": 1 }
                        }
                    }],
                    "partialFingerprints": {
                        "primaryLocationLineHash": fingerprint(
                            &finding.advisory_id,
                            &location,
                            1,
                        )
                    },
                    "properties": {
                        "check": "cargo_audit",
                        "in_diff": current_audit_in_diff,
                        "classification": origin_classification(current_audit_in_diff),
                        "package": finding.package_display(),
                        "severity": finding.severity,
                    }
                }));
            }
            cargo_audit_evidence = Some(evidence);
            continue;
        }

        let class = gate_class_for_check(check.status);
        if !matches!(class, GateClass::Fail | GateClass::Info) {
            continue;
        }

        // Dispatch to structured parsers first.
        match check_id.as_str() {
            "heuristics_loctree" => {
                // A repository-wide summary is not a diagnostic at a source line.
                // Preserve it as context without inventing an inline location.
                dashboard_findings.push(DashboardFinding {
                    file: None,
                    line: None,
                    level: "note",
                    check_name: check.name.clone(),
                    check_id: check_id.clone(),
                    message: check.output.trim().to_string(),
                    in_diff: None,
                });
                continue;
            }
            "pytest" => {
                let parsed = parse_pytest_failures(&check.output);
                if parsed.is_empty() {
                    dashboard_findings.push(DashboardFinding {
                        file: None,
                        line: None,
                        level: "note",
                        check_name: check.name.clone(),
                        check_id: check_id.clone(),
                        message: check_failure_excerpt(check),
                        in_diff: None,
                    });
                } else {
                    tool_findings_sets.push(ToolFindings {
                        source: "pytest",
                        tool_name: "Pytest",
                        check_id: check_id.clone(),
                        findings: parsed,
                    });
                }
                // Never attach an arbitrary path found in startup output to a
                // test failure whose traceback did not provide a location.
                continue;
            }
            "eslint" => {
                let parsed = parsers::eslint::parse_eslint_output(&check.output);
                if !parsed.is_empty() {
                    tool_findings_sets.push(ToolFindings {
                        source: "eslint",
                        tool_name: "ESLint",
                        check_id: check_id.clone(),
                        findings: parsed,
                    });
                    continue;
                }
            }
            "stylelint" => {
                let parsed = parsers::stylelint::parse_stylelint_output(&check.output);
                if !parsed.is_empty() {
                    tool_findings_sets.push(ToolFindings {
                        source: "stylelint",
                        tool_name: "Stylelint",
                        check_id: check_id.clone(),
                        findings: parsed,
                    });
                    continue;
                }
            }
            "clippy" => {
                let parsed = parsers::clippy::parse_clippy_short_output(&check.output);
                if !parsed.is_empty() {
                    tool_findings_sets.push(ToolFindings {
                        source: "clippy",
                        tool_name: "Clippy",
                        check_id: check_id.clone(),
                        findings: parsed,
                    });
                    continue;
                }
            }
            "cargo_test" => {
                let parsed = parsers::cargo_test::parse_cargo_test_output(&check.output);
                if !parsed.is_empty() {
                    tool_findings_sets.push(ToolFindings {
                        source: "cargo_test",
                        tool_name: "Cargo Test",
                        check_id: check_id.clone(),
                        findings: parsed,
                    });
                    continue;
                }
            }
            "semgrep_scan" => {
                let mut parsed = parsers::semgrep::parse_semgrep_json_output(&check.output);
                if parsed.is_empty() {
                    parsed = parsers::semgrep::parse_semgrep_text(&check.output);
                }
                if !parsed.is_empty() {
                    tool_findings_sets.push(ToolFindings {
                        source: "semgrep",
                        tool_name: "Semgrep",
                        check_id: check_id.clone(),
                        findings: parsed,
                    });
                }
                // Semgrep's pretty output embeds source snippets (including
                // minified vendored JS). Never let it reach the generic
                // file:line scraper, which would mis-read a code fragment as a
                // SARIF artifact location. Always continue, even with 0 findings.
                continue;
            }
            _ => {}
        }

        // Fallback: generic single-result for checks without a parser.
        // Try to extract actual source file:line from output before falling back to log.
        let level = if matches!(class, GateClass::Fail) {
            error_count += 1;
            "error"
        } else {
            warning_count += 1;
            "warning"
        };

        let is_geiger = check_id == "cargo_geiger";
        let first_line = check
            .output
            .lines()
            .find(|line| !should_skip_inline_fallback_line(is_geiger, line))
            .unwrap_or("No details provided");

        // Extract file:line from output for proper SARIF locations.
        let extracted = extract_file_line_from_output(&check.output);
        let (sarif_location, generic_in_diff) = if let Some((ref file, line_num)) = extracted {
            let in_diff_val = is_in_diff(file);
            dashboard_findings.push(DashboardFinding {
                file: Some(file.clone()),
                line: Some(line_num),
                level,
                check_name: check.name.clone(),
                check_id: check_id.clone(),
                message: first_line.to_string(),
                in_diff: Some(in_diff_val),
            });
            (
                json!({
                    "physicalLocation": {
                        "artifactLocation": { "uri": file },
                        "region": { "startLine": line_num }
                    }
                }),
                Some(in_diff_val),
            )
        } else {
            dashboard_findings.push(DashboardFinding {
                file: None,
                line: None,
                level,
                check_name: check.name.clone(),
                check_id: check_id.clone(),
                message: first_line.to_string(),
                in_diff: None,
            });
            (
                json!({
                    "physicalLocation": {
                        "artifactLocation": { "uri": "20_quality/full-checks.log" }
                    }
                }),
                None,
            )
        };

        let rule_id = format!("prview.{}", check_id_from_name(&check.name));
        if known_sarif_rules.insert(rule_id.clone()) {
            sarif_rules.push(json!({
                "id": rule_id,
                "shortDescription": { "text": check.name },
                "defaultConfiguration": { "level": level }
            }));
        }
        sarif_results.push(json!({
            "ruleId": rule_id,
            "level": level,
            "message": { "text": format!("{}: {}", check.name, first_line) },
            "locations": [sarif_location],
            // Unparsed checks carry the same origin tri-state as parsed tool
            // findings. A row that fell back to the combined log has no
            // established location, so it reports `null`/`unclassified` rather
            // than silently omitting the properties a consumer reads.
            "properties": {
                "in_diff": generic_in_diff,
                "classification": origin_classification(generic_in_diff),
                "source": &check_id,
            }
        }));
    }

    // Build one aggregate SARIF run from parsed findings. Per-source details
    // live in `properties.source` to keep GitHub/VS Code viewers in one stream.
    for tool_set in &tool_findings_sets {
        let mut filtered_generated = 0usize;
        let mut in_diff_count = 0usize;
        // TOOLING-08: explicit introduced (touched by this PR) vs preexisting
        // (inherited) split over the *reported* findings.
        let mut preexisting_count = 0usize;
        let mut unclassified_count = 0usize;
        let mut emitted_count = 0usize;

        for finding in &tool_set.findings {
            if parsers::is_generated_path(&finding.file) {
                filtered_generated += 1;
                continue;
            }

            match finding.level {
                "error" => error_count += 1,
                "warning" => warning_count += 1,
                _ => {}
            }

            let rule_id = finding
                .rule_id
                .clone()
                .unwrap_or_else(|| format!("prview.{}", tool_set.source));

            if known_sarif_rules.insert(rule_id.clone()) {
                sarif_rules.push(json!({
                    "id": rule_id,
                    "shortDescription": { "text": tool_set.tool_name },
                    "defaultConfiguration": { "level": finding.level }
                }));
            }

            // A test can fail because of inputs or its environment. Its
            // traceback location alone cannot classify the failure's origin.
            let in_diff = (tool_set.source != "pytest").then(|| is_in_diff(&finding.file));
            let classification = origin_classification(in_diff);
            match in_diff {
                Some(true) => in_diff_count += 1,
                Some(false) => preexisting_count += 1,
                None => unclassified_count += 1,
            }

            dashboard_findings.push(DashboardFinding {
                file: Some(finding.file.clone()),
                line: Some(finding.line),
                level: finding.level,
                check_name: tool_set.tool_name.to_string(),
                check_id: tool_set.check_id.clone(),
                message: finding.message.clone(),
                in_diff,
            });

            let mut location = json!({
                "physicalLocation": {
                    "artifactLocation": { "uri": &finding.file },
                    "region": { "startLine": finding.line }
                }
            });
            if let Some(col) = finding.column {
                location["physicalLocation"]["region"]["startColumn"] = json!(col);
            }

            sarif_results.push(json!({
                "ruleId": rule_id,
                "level": finding.level,
                "message": { "text": &finding.message },
                "locations": [location],
                "partialFingerprints": {
                    "primaryLocationLineHash": fingerprint(
                        &rule_id,
                        &finding.file,
                        finding.line,
                    )
                },
                "properties": {
                    "in_diff": in_diff,
                    "classification": classification,
                    "source": tool_set.source,
                }
            }));
            emitted_count += 1;
        }

        if emitted_count > 0 {
            sarif_rules.push(json!({
                "id": format!("prview.summary.{}", tool_set.source),
                "shortDescription": { "text": format!("{} summary", tool_set.tool_name) },
                "properties": {
                    "total_findings": tool_set.findings.len(),
                    "filtered_generated": filtered_generated,
                    "in_diff_count": in_diff_count,
                    "introduced_count": in_diff_count,
                    "preexisting_count": preexisting_count,
                    "unclassified_count": unclassified_count,
                }
            }));
        }
    }

    let runs = if sarif_results.is_empty() {
        Vec::new()
    } else {
        vec![json!({
            "tool": {
                "driver": {
                    "name": "prview-inline",
                    "version": "1.0.0",
                    "informationUri": "https://github.com/vetcoders/prview",
                    "rules": sarif_rules
                }
            },
            "invocations": [{
                "executionSuccessful": true,
                "properties": {
                    "total_findings": sarif_results.len(),
                }
            }],
            "results": sarif_results
        })]
    };

    let sarif = json!({
        "version": "2.1.0",
        "$schema": "https://json.schemastore.org/sarif-2.1.0.json",
        "runs": runs
    });

    // Only write SARIF file when there are actual findings.
    // Empty SARIF (runs: []) adds noise without value.
    if !runs.is_empty() {
        fs::write(&sarif_path, serde_json::to_string_pretty(&sarif)?)?;
    }

    let status = if error_count > 0 {
        "failed"
    } else if warning_count > 0 {
        "warnings"
    } else {
        "passed"
    }
    .to_string();

    Ok(InlineFindingsSummary {
        status,
        findings_count: error_count + warning_count,
        dashboard_findings,
        cargo_audit: cargo_audit_evidence,
    })
}

/// Does `candidate` (the text before a `:line` token) plausibly name a source
/// file rather than a code fragment?
///
/// Tool output that embeds source snippets (notably Semgrep over minified JS)
/// can contain a fragment like `},{"./_assignValue":75` whose `/` would
/// otherwise pass a naive path check and leak a code fragment into a SARIF
/// artifact location.
pub(super) fn is_pathish_candidate(candidate: &str) -> bool {
    if !(candidate.contains('/') || candidate.contains('\\')) {
        return false;
    }
    const CODE_CHARS: &[char] = &[
        '{', '}', '"', '=', '(', ')', ';', ',', '\'', '`', '*', '<', '>', '[', ']',
    ];
    if candidate.contains(CODE_CHARS) || candidate.chars().any(char::is_whitespace) {
        return false;
    }
    true
}

/// Does `candidate` (the text before a `:line:` token in Pytest output) name a
/// file a collector reported?
///
/// Two shapes used to be rejected for reasons Pytest does not share. A
/// repository path may contain spaces (`tests with space/test_bad.py:2: in
/// test_bad`), and a location need not be Python at all: doctest and plugin
/// collectors report `.rst`, `.txt` or `.md` files. What a location never is,
/// is a fragment of source text, so the candidate is rejected when it carries
/// characters that only occur in code and must end in a file extension.
fn is_pytest_location_candidate(candidate: &str) -> bool {
    const CODE_CHARS: &[char] = &[
        '{', '}', '"', '=', '(', ')', ';', ',', '\'', '`', '*', '<', '>', '[', ']', '#',
    ];
    if candidate.is_empty() || candidate.contains(CODE_CHARS) {
        return false;
    }
    let name = candidate.rsplit(['/', '\\']).next().unwrap_or(candidate);
    name.rsplit_once('.').is_some_and(|(stem, extension)| {
        !stem.is_empty()
            && !extension.is_empty()
            && extension.chars().all(|c| c.is_ascii_alphanumeric())
    })
}

/// Extract located Pytest diagnostics only from its failure/error sections.
/// A traceback location says where the failure was reported, not what caused it.
pub(super) fn parse_pytest_failures(output: &str) -> Vec<parsers::LintFinding> {
    use regex::Regex;
    use std::sync::LazyLock;

    // The path is captured lazily up to the numeric `:line:` suffix rather
    // than as a run of non-whitespace with a `.py` extension; see
    // `is_pytest_location_candidate` for what is then accepted as a path.
    static LOCATION: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"^(.+?):([1-9][0-9]*):\s+(.+)$").expect("pytest location regex")
    });
    let mut findings = Vec::new();
    let mut in_failures = false;
    let mut test_name = String::new();
    let mut evidence = Vec::new();
    let mut pending_frame: Option<(String, u32)> = None;
    let flush_frame = |findings: &mut Vec<parsers::LintFinding>,
                       pending: &mut Option<(String, u32)>,
                       evidence: &[String],
                       name: &str| {
        if !evidence.is_empty()
            && let Some((file, line)) = pending.take()
        {
            findings.push(parsers::LintFinding {
                file,
                line,
                column: None,
                level: "error",
                message: format!(
                    "Pytest reported a failure in {name}:\n{}",
                    evidence.join("\n")
                ),
                rule_id: None,
                source: "pytest",
            });
        }
    };
    for line in output.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('=') && trimmed.ends_with('=') {
            flush_frame(&mut findings, &mut pending_frame, &evidence, &test_name);
            pending_frame = None;
            let section = trimmed.trim_matches('=').trim();
            in_failures = matches!(section, "FAILURES" | "ERRORS");
            evidence.clear();
            continue;
        }
        if !in_failures {
            continue;
        }
        if trimmed.starts_with("___") && trimmed.ends_with("___") {
            flush_frame(&mut findings, &mut pending_frame, &evidence, &test_name);
            pending_frame = None;
            test_name = trimmed.trim_matches('_').trim().to_string();
            evidence.clear();
            continue;
        }
        if let Some(detail) = trimmed.strip_prefix("E ") {
            if evidence.len() < 6 && !detail.trim().is_empty() {
                evidence.push(detail.trim().to_string());
            }
            continue;
        }
        let Some(caps) = LOCATION.captures(trimmed) else {
            continue;
        };
        if !is_pytest_location_candidate(&caps[1]) {
            continue;
        }
        if caps[3].starts_with("in ") {
            // Retain the last frame until an error corroborates it. A later
            // terminal location takes precedence over abbreviated frames.
            if let Ok(line_number) = caps[2].parse::<u32>() {
                pending_frame = Some((caps[1].to_string(), line_number));
            }
            continue;
        }
        let Ok(line_number) = caps[2].parse::<u32>() else {
            continue;
        };
        let detail = if evidence.is_empty() {
            caps[3].to_string()
        } else {
            evidence.join("\n")
        };
        let context = if test_name.is_empty() {
            "Pytest reported a failure".to_string()
        } else {
            format!("Pytest reported a failure in {test_name}")
        };
        findings.push(parsers::LintFinding {
            file: caps[1].to_string(),
            line: line_number,
            column: None,
            level: "error",
            message: format!("{context}:\n{detail}"),
            rule_id: None,
            source: "pytest",
        });
        pending_frame = None;
        evidence.clear();
    }
    flush_frame(&mut findings, &mut pending_frame, &evidence, &test_name);
    findings
}

/// A bounded diagnostic excerpt for human-facing test output. Startup and
/// per-test progress are deliberately excluded; the complete log remains evidence.
pub(super) fn pytest_failure_excerpt(output: &str) -> Option<String> {
    static FAILED_PROGRESS: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"^\S+\.py::.+\s(?:FAILED|ERROR)(?:\s+\[[^\]]+\])?\s*$")
            .expect("pytest failed progress regex")
    });
    let parsed = parse_pytest_failures(output);
    if !parsed.is_empty() {
        return Some(
            parsed
                .iter()
                .take(3)
                .map(|finding| format!("{}\n{}:{}", finding.message, finding.file, finding.line))
                .collect::<Vec<_>>()
                .join("\n\n"),
        );
    }
    let summary: Vec<_> = output
        .lines()
        .map(str::trim)
        .filter(|line| {
            line.starts_with("FAILED ")
                || line.starts_with("ERROR ")
                || FAILED_PROGRESS.is_match(line)
        })
        .take(3)
        .collect();
    if !summary.is_empty() {
        return Some(summary.join("\n"));
    }

    // Pytest can capture exception details without a terminal path:line (for
    // example a collection error or an abbreviated traceback). Keep that real
    // diagnostic as unlocated evidence rather than replacing it with startup.
    let mut in_failures = false;
    let mut traceback = Vec::new();
    for line in output.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('=') && trimmed.ends_with('=') {
            in_failures = matches!(trimmed.trim_matches('=').trim(), "FAILURES" | "ERRORS");
            continue;
        }
        if in_failures && trimmed.starts_with("E ") && traceback.len() < 6 {
            traceback.push(trimmed.to_string());
        }
    }
    (!traceback.is_empty()).then(|| traceback.join("\n"))
}

/// Shared human and machine excerpt. A failed process is not evidence that a
/// named test failed; Pytest startup and passing rows never replace diagnostics.
pub(super) fn check_failure_excerpt(check: &CheckResult) -> String {
    let semgrep = check
        .name
        .to_ascii_lowercase()
        .contains("semgrep")
        .then(|| semgrep_check_excerpt(&check.output))
        .flatten();
    let mut excerpt = if let Some(summary) = semgrep {
        summary
    } else if !matches!(check.status, CheckStatus::Failed | CheckStatus::Error) {
        // The checks panel also calls this helper for successful/skipped rows.
        // Preserve their neutral output without inventing a failed process.
        let mut lines: Vec<_> = check.output.lines().rev().take(12).collect();
        lines.reverse();
        lines.join("\n")
    } else if check.name.to_ascii_lowercase().contains("pytest") {
        super::root_cause::extract_pytest_root_cause(check)
            .map(|diagnostic| {
                if diagnostic.evidence.is_empty() {
                    diagnostic.cause
                } else {
                    diagnostic.evidence
                }
            })
            .unwrap_or_default()
    } else {
        let is_vitest = check.name.to_ascii_lowercase().contains("vitest");
        let lines: Vec<_> = check
            .output
            .lines()
            .filter(|line| {
                let trimmed = line.trim();
                if is_vitest && trimmed.starts_with(['✓', '✔', '√']) {
                    return false;
                }
                !(trimmed.starts_with("test ") && trimmed.ends_with(" ... ok")
                    || trimmed.contains(".py::")
                        && trimmed.split_whitespace().any(|part| part == "PASSED"))
            })
            .collect();
        let failure = is_vitest
            .then(|| {
                lines.iter().position(|line| {
                    let line = line.trim();
                    line.starts_with("FAIL ") || line.starts_with(['×', '✗'])
                })
            })
            .flatten()
            .or_else(|| {
                lines.iter().position(|line| {
                    let lower = line.to_ascii_lowercase();
                    lower.contains("error:")
                        || lower.contains("error[")
                        || lower.contains("failed")
                        || lower.contains("panicked at")
                        || lower.contains("caused by:")
                })
            });
        let start = failure.unwrap_or_else(|| lines.len().saturating_sub(12));
        lines
            .iter()
            .skip(start)
            .take(12)
            .copied()
            .collect::<Vec<_>>()
            .join("\n")
    };
    const MAX_EXCERPT_BYTES: usize = 8 * 1024;
    const TRUNCATED: &str = "\n... (truncated; see the full check log)";
    if excerpt.len() > MAX_EXCERPT_BYTES {
        let end = excerpt.floor_char_boundary(MAX_EXCERPT_BYTES - TRUNCATED.len());
        excerpt.truncate(end);
        excerpt.push_str(TRUNCATED);
    }
    excerpt
}

/// Summarize scan findings separately from parser/tool diagnostics. Preserve
/// the complete JSON and stderr in the raw log linked by the checks panel.
fn semgrep_check_excerpt(output: &str) -> Option<String> {
    let start = output.find('{')?;
    let payload = &output[start..];
    let mut stream = serde_json::Deserializer::from_str(payload).into_iter::<serde_json::Value>();
    let json = stream.next()?.ok()?;
    let results = json.get("results")?.as_array()?;
    let errors = json
        .get("errors")
        .and_then(|value| value.as_array())
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    let scan_errors = errors
        .iter()
        .filter(|error| {
            error
                .get("level")
                .and_then(|value| value.as_str())
                .is_some_and(|level| level.eq_ignore_ascii_case("error"))
        })
        .count();
    let mut summary = format!(
        "{} findings; {} scan warnings; {} scan errors",
        results.len(),
        errors.len() - scan_errors,
        scan_errors
    );
    if !errors.is_empty() {
        summary.push_str(". Scan diagnostics may limit coverage.");
    }
    let compact = |text: &str| {
        text.split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .chars()
            .take(240)
            .collect::<String>()
    };
    for finding in parsers::semgrep::parse_semgrep_json_output(&payload[..stream.byte_offset()])
        .iter()
        .take(3)
    {
        summary.push_str(&format!(
            "\nFinding: {}:{} — {}",
            finding.file,
            finding.line,
            compact(&finding.message)
        ));
    }
    for error in errors.iter().take(3) {
        let kind = error
            .get("type")
            .and_then(|value| value.as_str())
            .unwrap_or("Scan diagnostic");
        let message = ["message", "long_msg", "short_msg"]
            .iter()
            .find_map(|field| error.get(field).and_then(|value| value.as_str()))
            .unwrap_or(kind);
        summary.push_str(&format!("\nScan diagnostic ({kind}): {}", compact(message)));
    }
    Some(summary)
}

/// Extract the first file:line reference from check output.
///
/// Tries common patterns:
/// - `path/file.py:27: error` (Python mypy/pylint)
/// - `path/file.rs:27:5: error` (Rust)
/// - `path/file.ts(27,5): error` (TypeScript tsc)
/// - `  --> path/file.rs:42:5` (Rust compiler)
pub(super) fn extract_file_line_from_output(output: &str) -> Option<(String, u32)> {
    for line in output.lines() {
        let trimmed = line.trim();

        // rustfmt `--check`: `Diff in <path>:<line>:`. The path is emitted before
        // a generic `<path>:<line>` token, but the `Diff in ` prefix contains
        // whitespace, so the generic path check below rejects the whole line as a
        // code fragment and the finding stays unclassified (in_diff = None). Parse
        // the header explicitly so R2-13's out-of-diff downgrade can actually fire
        // (R3-17).
        if let Some(rest) = trimmed.strip_prefix("Diff in ") {
            // `<path>:<line>:` — drop the trailing colon, then split off the line.
            let rest = rest.trim_end_matches(':');
            if let Some((file, line_str)) = rest.rsplit_once(':')
                && let Ok(ln) = line_str.parse::<u32>()
                && !file.is_empty()
                && ln > 0
            {
                return Some((file.to_string(), ln));
            }
            continue;
        }

        // Rust compiler: `  --> path/file.rs:42:5`
        if let Some(rest) = trimmed.strip_prefix("-->") {
            let rest = rest.trim();
            if let Some((file, line_col)) = rest.rsplit_once(':') {
                // Could be file:line:col or file:line
                if let Some((file2, line_str)) = file.rsplit_once(':')
                    && let Ok(ln) = line_str.parse::<u32>()
                    && !file2.is_empty()
                    && ln > 0
                {
                    return Some((file2.to_string(), ln));
                }
                if let Ok(ln) = line_col.parse::<u32>()
                    && !file.is_empty()
                    && ln > 0
                {
                    return Some((file.to_string(), ln));
                }
            }
            continue;
        }

        // Generic: `path/file.ext:LINE:` or `path/file.ext:LINE:COL:`
        // Must contain a `/` or `\` to be a path (avoid false positives on bare words)
        // Handle Windows drive letters: skip `C:` prefix when present
        let search_start = if trimmed.len() >= 3
            && trimmed.as_bytes()[0].is_ascii_alphabetic()
            && trimmed.as_bytes()[1] == b':'
            && (trimmed.as_bytes()[2] == b'\\' || trimmed.as_bytes()[2] == b'/')
        {
            2 // skip drive letter "C:" prefix
        } else {
            0
        };
        if let Some(rel_idx) = trimmed[search_start..].find(':') {
            let colon_idx = search_start + rel_idx;
            let candidate = &trimmed[..colon_idx];
            if is_pathish_candidate(candidate) {
                let rest = &trimmed[colon_idx + 1..];
                let line_str: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
                if let Ok(ln) = line_str.parse::<u32>()
                    && ln > 0
                {
                    return Some((candidate.to_string(), ln));
                }
            }
        }

        // TypeScript tsc: `path/file.ts(27,5): error`
        if let Some(paren_idx) = trimmed.find('(') {
            let candidate = &trimmed[..paren_idx];
            if candidate.contains('/') || candidate.contains('\\') {
                let rest = &trimmed[paren_idx + 1..];
                let line_str: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
                if let Ok(ln) = line_str.parse::<u32>()
                    && ln > 0
                {
                    return Some((candidate.to_string(), ln));
                }
            }
        }
    }
    None
}

pub(super) fn should_skip_inline_fallback_line(is_geiger: bool, line: &str) -> bool {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return true;
    }

    if !is_geiger {
        return false;
    }

    trimmed.starts_with("Metric output format:")
        || trimmed.contains("WARNING: Dependency file was never scanned")
        || (trimmed.chars().next().is_some_and(|c| c.is_ascii_digit())
            && trimmed.contains('/')
            && trimmed.contains("unsafe"))
}

#[cfg(test)]
mod tests {
    const PYTEST_FAILURE: &str = "================ test session starts ================\n\
plugins: unrelated\n\
tests/test_parser.py::test_roundtrip FAILED [100%]\n\
================ FAILURES ================\n\
________________ test_roundtrip ________________\n\
>       assert refusals == []\n\
E       AssertionError: unsupported message\n\
E       assert ['AgentMessage'] == []\n\
tests/test_parser.py:42: AssertionError\n\
================ short test summary info ================\n\
FAILED tests/test_parser.py::test_roundtrip\n\
================ 1 failed, 12 passed ================\n";

    #[test]
    fn pytest_failure_uses_diagnostic_and_preserves_location() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let checks = [super::CheckResult {
            name: "Pytest".to_string(),
            status: crate::checks::CheckStatus::Failed,
            duration: std::time::Duration::from_secs(1),
            output: PYTEST_FAILURE.to_string(),
            cached: false,
            provenance: None,
        }];
        let summary = super::generate_inline_findings(
            tmp.path(),
            &checks,
            &[one_file_diff("tests/test_parser.py")],
            None,
            None,
        )
        .expect("findings");
        assert_eq!(summary.findings_count, 1);
        let finding = &summary.dashboard_findings[0];
        assert_eq!(finding.file.as_deref(), Some("tests/test_parser.py"));
        assert_eq!(finding.line, Some(42));
        assert_eq!(
            finding.in_diff, None,
            "a changed test file is not proof of origin"
        );
        assert!(
            finding
                .message
                .contains("AssertionError: unsupported message")
        );
        assert!(!finding.message.contains("test session starts"));
        assert!(!finding.message.contains("caused by"));
        let sarif: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(tmp.path().join("INLINE_FINDINGS.sarif")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            sarif["runs"][0]["results"][0]["locations"][0]["physicalLocation"]["region"]["startLine"],
            42
        );
        assert_eq!(
            sarif["runs"][0]["results"][0]["properties"]["classification"],
            "unclassified"
        );
        let excerpt = super::pytest_failure_excerpt(PYTEST_FAILURE).expect("excerpt");
        assert!(excerpt.contains("tests/test_parser.py:42"));
        assert!(!excerpt.contains("plugins:"));
    }

    #[test]
    fn pytest_never_borrows_startup_location_for_unlocated_failure() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let checks = [super::CheckResult {
            name: "Pytest".to_string(),
            status: crate::checks::CheckStatus::Failed,
            duration: std::time::Duration::ZERO,
            output: "plugins/helper.py:19: loaded\nFAILED tests/test_parser.py::test_roundtrip"
                .to_string(),
            cached: false,
            provenance: None,
        }];
        let summary = super::generate_inline_findings(tmp.path(), &checks, &[], None, None)
            .expect("findings");
        assert_eq!(summary.findings_count, 0);
        assert_eq!(summary.dashboard_findings[0].file, None);
        assert_eq!(summary.dashboard_findings[0].line, None);
        assert!(summary.dashboard_findings[0].message.starts_with("FAILED "));
        assert!(!tmp.path().join("INLINE_FINDINGS.sarif").exists());
    }

    #[test]
    fn pytest_short_tracebacks_pair_final_frames_with_error_evidence() {
        let output = "===== FAILURES =====\n_____ test_one _____\ntests/test_parser.py:42: in test_parser\n    helper()\nsrc/parser.py:9: in helper\nE   ValueError: invalid input\n_____ test_two _____\ntests/test_other.py:17: in test_other\nE   AssertionError: mismatch\n===== short test summary info =====";
        let located = super::parse_pytest_failures(output);
        assert_eq!(located.len(), 2);
        assert_eq!((&*located[0].file, located[0].line), ("src/parser.py", 9));
        assert!(located[0].message.contains("invalid input"));
        assert_eq!(
            (&*located[1].file, located[1].line),
            ("tests/test_other.py", 17)
        );
        assert!(!located[1].message.contains("invalid input"));
        let without_summary = output.split("===== short").next().unwrap();
        assert_eq!(super::parse_pytest_failures(without_summary).len(), 2);
        assert!(
            super::parse_pytest_failures("===== FAILURES =====\ntests/x.py:42: in test_x")
                .is_empty()
        );
        let terminal = "===== FAILURES =====\ntests/x.py:42: in test_x\nE   ValueError: detail\nsrc/y.py:8: ValueError";
        let located = super::parse_pytest_failures(terminal);
        assert_eq!(located.len(), 1);
        assert_eq!((&*located[0].file, located[0].line), ("src/y.py", 8));
    }

    #[test]
    fn pytest_locations_keep_paths_with_spaces() {
        let output = "===== FAILURES =====\n\
            _____ test_bad _____\n\
            tests with space/test_bad.py:2: in test_bad\n\
            E   AssertionError: mismatch\n";
        let located = super::parse_pytest_failures(output);
        assert_eq!(located.len(), 1);
        assert_eq!(located[0].file, "tests with space/test_bad.py");
        assert_eq!(located[0].line, 2);
    }

    #[test]
    fn pytest_locations_accept_non_python_collectors() {
        let output = "===== FAILURES =====\n\
            _____ [doctest] docs/example.rst _____\n\
            E   Expected 2, got 3\n\
            docs/example.rst:4: DocTestFailure\n";
        let located = super::parse_pytest_failures(output);
        assert_eq!(located.len(), 1);
        assert_eq!(located[0].file, "docs/example.rst");
        assert_eq!(located[0].line, 4);
        assert!(located[0].message.contains("Expected 2, got 3"));
    }

    #[test]
    fn pytest_location_candidate_rejects_code_fragments() {
        assert!(super::is_pytest_location_candidate("tests/a b/test_x.py"));
        assert!(super::is_pytest_location_candidate("docs/example.rst"));
        assert!(super::is_pytest_location_candidate("test_bad.py"));
        assert!(!super::is_pytest_location_candidate(
            "raise ValueError(\"x\""
        ));
        assert!(!super::is_pytest_location_candidate("no_extension_here"));
        assert!(!super::is_pytest_location_candidate(""));
    }

    #[test]
    fn pytest_keeps_each_failure_paired_with_its_own_evidence() {
        let output = "===== FAILURES =====\n\
            _____ test_one _____\n\
            E   AssertionError: first failure\n\
            tests/test_one.py:12: AssertionError\n\
            _____ test_two _____\n\
            E   ValueError: second failure\n\
            tests/test_two.py:34: ValueError\n\
            ===== short test summary info =====";
        let findings = super::parse_pytest_failures(output);
        assert_eq!(findings.len(), 2);
        assert_eq!(findings[0].file, "tests/test_one.py");
        assert_eq!(findings[0].line, 12);
        assert!(findings[0].message.contains("first failure"));
        assert!(!findings[0].message.contains("second failure"));
        assert_eq!(findings[1].file, "tests/test_two.py");
        assert_eq!(findings[1].line, 34);
        assert!(findings[1].message.contains("second failure"));
        assert!(!findings[1].message.contains("first failure"));
    }

    #[test]
    fn pytest_aborted_progress_is_not_an_inline_finding() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let checks = [super::CheckResult {
            name: "Pytest".to_string(),
            status: crate::checks::CheckStatus::Failed,
            duration: std::time::Duration::ZERO,
            output: "===== test session starts =====\n\
                plugins/helper.py:19: loaded\n\
                tests/test_parser.py::test_failed_setup_is_reported PASSED [ 25%]\n\
                tests/test_parser.py::test_error[2026-02-30T00:00:00Z] PASSED [ 25%]\n"
                .to_string(),
            cached: false,
            provenance: None,
        }];
        assert_eq!(super::pytest_failure_excerpt(&checks[0].output), None);
        let summary = super::generate_inline_findings(tmp.path(), &checks, &[], None, None)
            .expect("findings");
        assert_eq!(summary.findings_count, 0);
        let note = &summary.dashboard_findings[0];
        assert_eq!(note.level, "note");
        assert_eq!(note.file, None);
        assert_eq!(note.line, None);
        assert!(note.message.contains("cause is unknown"));
        assert!(!note.message.contains("PASSED"));
        assert!(!tmp.path().join("INLINE_FINDINGS.sarif").exists());
    }

    #[test]
    fn pytest_unlocated_exception_remains_diagnostic_evidence() {
        let output = "===== ERRORS =====\n\
            _____ ERROR collecting tests/test_import.py _____\n\
            E   ImportError: cannot import name 'missing'\n";
        assert!(super::parse_pytest_failures(output).is_empty());
        let excerpt = super::pytest_failure_excerpt(output).expect("exception evidence");
        assert!(excerpt.contains("ImportError: cannot import name 'missing'"));
    }

    #[test]
    fn pytest_progress_requires_an_actual_failed_status() {
        let passed = "tests/test_parser.py::test_error[FAILED input] PASSED [ 25%]";
        assert_eq!(super::pytest_failure_excerpt(passed), None);
        let failed = "tests/test_parser.py::test_real_failure FAILED [ 26%]";
        let output = format!("{passed}\n{failed}");
        assert!(super::parse_pytest_failures(&output).is_empty());
        assert_eq!(
            super::pytest_failure_excerpt(&output).as_deref(),
            Some(failed)
        );
    }

    #[test]
    fn shared_failure_excerpt_skips_passing_names_and_keeps_rust_panic_location() {
        let check = super::CheckResult {
            name: "Cargo test".to_string(),
            status: crate::checks::CheckStatus::Failed,
            duration: std::time::Duration::ZERO,
            output: "Compiling fixture\n\
                test test_failed_setup_is_reported ... ok\n\
                test test_error ... ok\n\
                thread 'tests::real_failure' panicked at src/lib.rs:42:5:\n\
                assertion failed: false\n\
                stack backtrace:\n\
                0: fixture::real_failure\n"
                .to_string(),
            cached: false,
            provenance: None,
        };
        let excerpt = super::check_failure_excerpt(&check);
        assert!(excerpt.contains("src/lib.rs:42:5"));
        assert!(excerpt.contains("stack backtrace:"));
        assert!(!excerpt.contains(" ... ok"));
        assert!(!excerpt.contains("Compiling fixture"));
    }

    #[test]
    fn vitest_excerpt_starts_at_failure_not_a_passing_test_name() {
        let mut output = "✓ handles failed requests\n".repeat(20);
        output.push_str("× rejects malformed payload\nAssertionError: expected false to be true\n");
        let check = super::CheckResult {
            name: "Vitest".into(),
            status: crate::checks::CheckStatus::Failed,
            duration: std::time::Duration::ZERO,
            output,
            cached: false,
            provenance: None,
        };
        let excerpt = super::check_failure_excerpt(&check);
        assert!(excerpt.starts_with("× rejects malformed payload"));
        assert!(excerpt.contains("AssertionError"));
        assert!(!excerpt.contains("handles failed requests"));
    }

    #[test]
    fn pytest_nonfailure_excerpt_never_invents_noncompletion() {
        for status in [
            crate::checks::CheckStatus::Passed,
            crate::checks::CheckStatus::Skipped,
            crate::checks::CheckStatus::Warnings,
        ] {
            let check = super::CheckResult {
                name: "Pytest".to_string(),
                status,
                duration: std::time::Duration::ZERO,
                output: "tests/test_parser.py::test_error PASSED\n===== 1 passed in 0.1s ====="
                    .to_string(),
                cached: false,
                provenance: None,
            };
            let excerpt = super::check_failure_excerpt(&check);
            assert!(excerpt.contains("1 passed in 0.1s"));
            assert!(!excerpt.contains("did not complete"));
            assert!(!excerpt.contains("cause is unknown"));
            assert!(super::super::root_cause::extract_pytest_root_cause(&check).is_none());
        }
    }

    #[test]
    fn shared_failure_excerpt_bounds_a_huge_unicode_line() {
        let check = super::CheckResult {
            name: "Cargo check".to_string(),
            status: crate::checks::CheckStatus::Failed,
            duration: std::time::Duration::ZERO,
            output: format!("error[E0001]: invalid value {}", "ą🧪".repeat(4096)),
            cached: false,
            provenance: None,
        };
        let excerpt = super::check_failure_excerpt(&check);
        assert!(excerpt.len() <= 8 * 1024);
        assert!(excerpt.starts_with("error[E0001]: invalid value "));
        assert!(excerpt.ends_with("... (truncated; see the full check log)"));
        assert!(excerpt.contains("ą🧪"));
    }

    #[test]
    fn semgrep_warning_excerpt_separates_scan_diagnostics_from_findings() {
        let json = serde_json::json!({
            "results": [],
            "errors": [{"type": "PartialParsing", "level": "warn", "message": format!("Could not parse src/ui.js: {}", "ą🧪".repeat(4096))}]
        });
        let check = super::CheckResult {
            name: "Semgrep".into(),
            status: crate::checks::CheckStatus::Warnings,
            duration: std::time::Duration::ZERO,
            output: format!("{json}\npkg_resources is deprecated"),
            cached: false,
            provenance: None,
        };
        let excerpt = super::check_failure_excerpt(&check);
        assert!(excerpt.starts_with("0 findings; 1 scan warnings; 0 scan errors"));
        assert!(excerpt.contains("Scan diagnostic (PartialParsing): Could not parse src/ui.js:"));
        assert!(!excerpt.contains("pkg_resources"));
        assert!(!excerpt.contains("\"errors\""));
        assert!(!excerpt.contains("Finding:"));
        assert!(excerpt.len() < 2048);
    }

    #[test]
    fn semgrep_excerpt_preserves_actual_findings_and_scan_errors() {
        let output = serde_json::json!({
            "results": [{"path": "src/api.py", "start": {"line": 7}, "extra": {"severity": "ERROR", "message": "Unsafe eval"}}],
            "errors": [{"type": "Timeout", "level": "error", "short_msg": "Timed out parsing src/large.py"}]
        }).to_string();
        let excerpt = super::semgrep_check_excerpt(&output).unwrap();
        assert!(excerpt.starts_with("1 findings; 0 scan warnings; 1 scan errors"));
        assert!(excerpt.contains("Finding: src/api.py:7 — Unsafe eval"));
        assert!(excerpt.contains("Scan diagnostic (Timeout): Timed out parsing src/large.py"));
        assert_eq!(super::semgrep_check_excerpt("semgrep unavailable"), None);
        assert_eq!(super::semgrep_check_excerpt("{invalid json}"), None);
    }

    #[test]
    fn loctree_summary_is_general_context_not_an_inline_finding() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let checks = [super::CheckResult {
            name: "heuristics_loctree".to_string(),
            status: crate::checks::CheckStatus::Warnings,
            duration: std::time::Duration::ZERO,
            output: "9 dead exports; 8 unused symbols".to_string(),
            cached: false,
            provenance: None,
        }];
        let summary = super::generate_inline_findings(tmp.path(), &checks, &[], None, None)
            .expect("findings");
        assert_eq!(summary.findings_count, 0);
        assert_eq!(summary.dashboard_findings.len(), 1);
        assert_eq!(summary.dashboard_findings[0].level, "note");
        assert_eq!(summary.dashboard_findings[0].file, None);
        assert_eq!(summary.dashboard_findings[0].message, checks[0].output);
        assert!(!tmp.path().join("INLINE_FINDINGS.sarif").exists());
    }

    use super::*;

    fn err(in_diff: Option<bool>) -> DashboardFinding {
        DashboardFinding {
            file: None,
            line: None,
            level: "error",
            check_name: "Semgrep".to_string(),
            check_id: "semgrep_scan".to_string(),
            message: "finding".to_string(),
            in_diff,
        }
    }

    fn summary(dashboard_findings: Vec<DashboardFinding>) -> InlineFindingsSummary {
        InlineFindingsSummary {
            status: "failed".to_string(),
            findings_count: dashboard_findings.len(),
            dashboard_findings,
            cargo_audit: None,
        }
    }

    /// A clean local-checkout comparison with a resolved base diff: baseline
    /// signals downgrade, matching the default shape the pre-R4-18 gate assumed.
    fn clean() -> CleanComparison {
        CleanComparison::for_test(true, true)
    }

    fn baseline_row(
        check_id: &str,
        level: &'static str,
        in_diff: Option<bool>,
    ) -> DashboardFinding {
        DashboardFinding {
            file: None,
            line: None,
            level,
            check_name: check_id.to_string(),
            check_id: check_id.to_string(),
            message: "finding".to_string(),
            in_diff,
        }
    }

    #[test]
    fn baseline_metadata_is_not_an_operator_finding() {
        let metadata = DashboardFinding {
            file: None,
            line: None,
            level: "note",
            check_name: "Cargo audit baseline".to_string(),
            check_id: "cargo_audit_baseline".to_string(),
            message: "Cargo audit baseline: new=0, pre-existing=1".to_string(),
            in_diff: Some(false),
        };
        assert!(!is_operator_finding(&metadata));
        assert!(is_operator_finding(&err(Some(true))));
    }

    #[test]
    fn cargo_audit_baseline_counts_all_resolved_when_head_is_clean() {
        let current = std::collections::HashSet::new();
        let base = std::iter::once((
            "RUSTSEC-2024-0001".to_string(),
            "demo".to_string(),
            "1.2.3".to_string(),
        ))
        .collect();

        assert_eq!(
            cargo_audit_baseline_counts(Some(&current), true, Some(&base)),
            CargoAuditBaselineCounts {
                new: 0,
                preexisting: 0,
                resolved: 1,
                unknown: 0,
                status: "available",
            }
        );
    }

    #[test]
    fn cargo_audit_unknown_empty_baseline_remains_explicit() {
        assert_eq!(
            cargo_audit_baseline_counts(Some(&Default::default()), true, None),
            CargoAuditBaselineCounts {
                new: 0,
                preexisting: 0,
                resolved: 0,
                unknown: 0,
                status: "unavailable",
            }
        );
    }

    #[test]
    fn cargo_audit_invalid_current_report_cannot_mark_a_finding_preexisting() {
        let key = (
            "RUSTSEC-2024-0001".to_string(),
            "demo".to_string(),
            "1.2.3".to_string(),
        );
        assert_eq!(cargo_audit_finding_in_diff(&key, false, false, None), None);
    }

    #[test]
    fn cargo_audit_invalid_current_report_never_masquerades_as_clean() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let checks = [cargo_audit_check(
            crate::checks::CheckStatus::Warnings,
            "cargo audit failed before producing JSON",
        )];
        let summary =
            generate_inline_findings(tmp.path(), &checks, &[], None, None).expect("findings");

        assert_eq!(summary.findings_count, 0);
        assert_eq!(summary.dashboard_findings.len(), 1);
        assert!(
            summary.dashboard_findings[0]
                .message
                .contains("status=current-unavailable")
        );
        assert!(!tmp.path().join("INLINE_FINDINGS.sarif").exists());
    }

    #[test]
    fn inline_gate_ignores_preexisting_only_errors() {
        // THREAD 7: raw status is "failed" but every error is pre-existing, so
        // the aggregate gate must not fire (it would block under policy-mode
        // block despite every per-check evaluation approving these findings).
        let inline = summary(vec![err(Some(false)), err(Some(false))]);
        assert_eq!(
            effective_inline_gate_class(&inline, &clean()),
            GateClass::Pass
        );
    }

    #[test]
    fn inline_gate_blocks_on_introduced_errors() {
        let inline = summary(vec![err(Some(false)), err(Some(true))]);
        assert_eq!(
            effective_inline_gate_class(&inline, &clean()),
            GateClass::Fail
        );
    }

    #[test]
    fn inline_gate_blocks_on_unclassified_errors() {
        // Causation unknown (in_diff == None) is treated as new, never downgraded.
        let inline = summary(vec![err(None)]);
        assert_eq!(
            effective_inline_gate_class(&inline, &clean()),
            GateClass::Fail
        );
    }

    #[test]
    fn inline_gate_counts_out_of_diff_whole_project_parser_rows() {
        // R2-8: a cargo_test out-of-diff row is causation-unknown (the diff may
        // have broken a test in an unchanged file), not pre-existing, so it must
        // still gate — unlike a baseline-signal semgrep out-of-diff row, which
        // does not (proven by inline_gate_ignores_preexisting_only_errors).
        let cargo_test_row = DashboardFinding {
            file: None,
            line: None,
            level: "error",
            check_name: "Cargo Test".to_string(),
            check_id: "cargo_test".to_string(),
            message: "test failed".to_string(),
            in_diff: Some(false),
        };
        assert_eq!(
            effective_inline_gate_class(&summary(vec![cargo_test_row]), &clean()),
            GateClass::Fail
        );
    }

    #[test]
    fn inline_gate_gates_remote_target_local_checkout_baseline_row() {
        // R4-18: on a remote/snapshot target a rustfmt out-of-diff row came from
        // the local checkout — a different tree than the target — so the
        // clean-comparison gate refuses its downgrade. The aggregate gate must
        // agree with the per-check path and NOT skip it.
        let remote = CleanComparison::for_test(false, true);
        let row = baseline_row("rustfmt", "error", Some(false));
        assert_eq!(
            effective_inline_gate_class(&summary(vec![row]), &remote),
            GateClass::Fail,
            "a local-checkout rustfmt row on a remote target must gate, not skip"
        );
    }

    #[test]
    fn inline_gate_skips_remote_target_snapshot_scanned_baseline_row() {
        // R4-18: semgrep scans the target snapshot, so on a remote target its
        // out-of-diff row IS trusted as pre-existing and skips — unlike rustfmt.
        let remote = CleanComparison::for_test(false, true);
        let row = baseline_row("semgrep_scan", "error", Some(false));
        assert_eq!(
            effective_inline_gate_class(&summary(vec![row]), &remote),
            GateClass::Pass,
            "a snapshot-scanned semgrep row is pre-existing and skips"
        );
    }

    #[test]
    fn inline_gate_gates_dirty_scan_baseline_row() {
        // R4-18 / R2-9: a dirty local scan cannot trust an out-of-diff row as
        // pre-existing (it may be an uncommitted finding), so the aggregate gate
        // must gate even a baseline signal.
        let dirty = CleanComparison::for_test(true, false);
        let row = baseline_row("semgrep_scan", "error", Some(false));
        assert_eq!(
            effective_inline_gate_class(&summary(vec![row]), &dirty),
            GateClass::Fail,
            "a dirty-scan out-of-diff row must gate"
        );
    }

    #[test]
    fn extract_parses_rustfmt_diff_header_absolute_path() {
        // Real `cargo fmt --check` output (rustfmt 1.8): `Diff in <abs>:<line>:`.
        let output =
            "Diff in /home/u/proj/src/main.rs:1:\n fn main() {\n-let x=1;\n+    let x = 1;\n }\n";
        let (file, line) = extract_file_line_from_output(output).expect("rustfmt header parses");
        assert_eq!(file, "/home/u/proj/src/main.rs");
        assert_eq!(line, 1);
    }

    #[test]
    fn extract_parses_rustfmt_diff_header_relative_path() {
        let output = "Diff in src/foo.rs:42:\n-old\n+new\n";
        assert_eq!(
            extract_file_line_from_output(output),
            Some(("src/foo.rs".to_string(), 42))
        );
    }

    fn rustfmt_check(output: &str) -> crate::checks::CheckResult {
        crate::checks::CheckResult {
            name: "Rustfmt".to_string(),
            status: crate::checks::CheckStatus::Warnings,
            duration: std::time::Duration::from_millis(1),
            output: output.to_string(),
            cached: false,
            provenance: None,
        }
    }

    fn cargo_audit_check(status: crate::checks::CheckStatus, output: &str) -> CheckResult {
        CheckResult {
            name: "Cargo audit".to_string(),
            status,
            duration: std::time::Duration::from_millis(1),
            output: output.to_string(),
            cached: false,
            provenance: None,
        }
    }

    const CLEAN_CARGO_AUDIT: &str =
        r#"{"vulnerabilities":{"found":false,"count":0,"list":[]},"warnings":{}}"#;
    const VULNERABLE_CARGO_AUDIT: &str = r#"{
        "vulnerabilities": {
            "found": true,
            "count": 1,
            "list": [{
                "advisory": {"id": "RUSTSEC-2024-0001", "title": "demo advisory"},
                "package": {"name": "demo", "version": "1.2.3"},
                "versions": {"patched": [">=1.2.4"]}
            }]
        },
        "warnings": {}
    }"#;
    const INFORMATIONAL_CARGO_AUDIT: &str = r#"{
        "vulnerabilities": {"found": false, "count": 0, "list": []},
        "warnings": {
            "unmaintained": [{
                "advisory": {"id": "RUSTSEC-2024-9999"},
                "package": {"name": "demo", "version": "1.2.3"}
            }]
        }
    }"#;

    fn one_file_diff(path: &str) -> crate::git::Diff {
        crate::git::Diff {
            base: "main".to_string(),
            target: "feature".to_string(),
            base_commit_id: "def456".to_string(),
            target_commit_id: "abc123".to_string(),
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
    fn rustfmt_in_diff_finding_is_classified_introduced() {
        // R3-17: a rustfmt warning whose file is inside the diff resolves to
        // in_diff = Some(true), not the old None that stayed unclassified.
        let tmp = tempfile::tempdir().expect("tempdir");
        let checks = vec![rustfmt_check("Diff in src/changed.rs:3:\n-old\n+new\n")];
        let diffs = vec![one_file_diff("src/changed.rs")];
        let summary =
            generate_inline_findings(tmp.path(), &checks, &diffs, None, None).expect("findings");
        assert_eq!(
            summary.dashboard_findings[0].in_diff,
            Some(true),
            "rustfmt header must parse so an in-diff file is classified introduced"
        );
    }

    #[test]
    fn rustfmt_out_of_diff_finding_enables_preexisting_downgrade() {
        // R3-17: a rustfmt warning whose file is OUTSIDE the diff resolves to
        // in_diff = Some(false) — the signal R2-13's out-of-diff downgrade needs.
        let tmp = tempfile::tempdir().expect("tempdir");
        let checks = vec![rustfmt_check("Diff in src/untouched.rs:9:\n-old\n+new\n")];
        let diffs = vec![one_file_diff("src/changed.rs")];
        let summary =
            generate_inline_findings(tmp.path(), &checks, &diffs, None, None).expect("findings");
        assert_eq!(
            summary.dashboard_findings[0].in_diff,
            Some(false),
            "rustfmt header must parse so an out-of-diff file can be downgraded"
        );
    }

    #[test]
    fn cargo_audit_informational_only_never_falls_through_to_generic_sarif() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let checks = [cargo_audit_check(
            crate::checks::CheckStatus::Warnings,
            INFORMATIONAL_CARGO_AUDIT,
        )];
        let summary =
            generate_inline_findings(tmp.path(), &checks, &[], None, None).expect("findings");

        assert_eq!(summary.findings_count, 0);
        assert_eq!(summary.dashboard_findings.len(), 1);
        assert_eq!(
            summary.dashboard_findings[0].check_id,
            "cargo_audit_baseline"
        );
        assert!(
            summary.dashboard_findings[0]
                .message
                .contains("pre-existing=1")
        );
        assert!(!tmp.path().join("INLINE_FINDINGS.sarif").exists());
    }

    #[test]
    fn cargo_audit_passed_still_emits_a_clean_baseline_summary() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let checks = [cargo_audit_check(
            crate::checks::CheckStatus::Passed,
            CLEAN_CARGO_AUDIT,
        )];
        let summary =
            generate_inline_findings(tmp.path(), &checks, &[], None, None).expect("findings");

        assert_eq!(summary.findings_count, 0);
        assert_eq!(summary.dashboard_findings.len(), 1);
        assert!(
            summary.dashboard_findings[0]
                .message
                .contains("status=not-required")
        );
    }

    #[test]
    fn cargo_audit_lock_only_unknown_baseline_stays_unclassified() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let checks = [cargo_audit_check(
            crate::checks::CheckStatus::Failed,
            VULNERABLE_CARGO_AUDIT,
        )];
        let diffs = [one_file_diff("Cargo.lock")];
        let summary =
            generate_inline_findings(tmp.path(), &checks, &diffs, None, None).expect("findings");

        assert!(
            summary.dashboard_findings[0]
                .message
                .contains("status=unavailable")
        );
        let advisory = summary
            .dashboard_findings
            .iter()
            .find(|finding| finding.check_id == "cargo_audit")
            .expect("cargo-audit advisory");
        assert_eq!(advisory.in_diff, None);

        let sarif: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(tmp.path().join("INLINE_FINDINGS.sarif")).expect("sarif"),
        )
        .expect("parse sarif");
        assert!(sarif["runs"][0]["results"][0]["properties"]["in_diff"].is_null());
    }

    #[test]
    fn cargo_audit_manifest_only_change_keeps_locked_advisory_preexisting() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let checks = [cargo_audit_check(
            crate::checks::CheckStatus::Failed,
            VULNERABLE_CARGO_AUDIT,
        )];
        let diffs = [one_file_diff("Cargo.toml")];
        let summary =
            generate_inline_findings(tmp.path(), &checks, &diffs, None, None).expect("findings");

        assert!(
            summary.dashboard_findings[0]
                .message
                .contains("pre-existing=1")
        );
        let advisory = summary
            .dashboard_findings
            .iter()
            .find(|finding| finding.check_id == "cargo_audit")
            .expect("cargo-audit advisory");
        assert_eq!(advisory.in_diff, Some(false));
    }

    /// The gate's numbers and the caveat's numbers are the same numbers. The
    /// incident pack could state `new=0, pre-existing=2` in one artifact and
    /// block without explanation in another precisely because the decision path
    /// had no access to these counts.
    #[test]
    fn cargo_audit_evidence_matches_the_rendered_baseline_note() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let checks = [cargo_audit_check(
            crate::checks::CheckStatus::Failed,
            VULNERABLE_CARGO_AUDIT,
        )];
        let diffs = [one_file_diff("src/lib.rs")];
        let summary =
            generate_inline_findings(tmp.path(), &checks, &diffs, None, None).expect("findings");

        let evidence = summary
            .cargo_audit
            .as_ref()
            .expect("a run with a cargo audit check carries its baseline evidence");
        assert_eq!(evidence.status, "not-required");
        assert_eq!(evidence.new, 0);
        assert_eq!(evidence.preexisting, 1);
        assert_eq!(evidence.unknown, 0);
        assert!(!evidence.lock_changed);
        assert!(evidence.new_advisory_ids.is_empty());

        let note = &summary.dashboard_findings[0].message;
        assert_eq!(
            note,
            "Cargo audit baseline: status=not-required, new=0, pre-existing=1, \
             resolved=0, unknown-baseline=0",
            "the note and the evidence are one value rendered twice"
        );
    }

    /// No base audit is no proof, and the evidence says so rather than
    /// reporting a comfortable zero: `unknown`, not `new=0, pre-existing=0`.
    #[test]
    fn cargo_audit_evidence_keeps_an_unknown_baseline_unknown() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let checks = [cargo_audit_check(
            crate::checks::CheckStatus::Failed,
            VULNERABLE_CARGO_AUDIT,
        )];
        let diffs = [one_file_diff("Cargo.lock")];
        let summary =
            generate_inline_findings(tmp.path(), &checks, &diffs, None, None).expect("findings");

        let evidence = summary.cargo_audit.as_ref().expect("evidence");
        assert_eq!(evidence.status, "unavailable");
        assert_eq!(evidence.new, 0);
        assert_eq!(evidence.preexisting, 0);
        assert_eq!(evidence.unknown, 1);
        assert!(evidence.lock_changed);
        assert!(
            evidence.new_advisory_ids.is_empty(),
            "an advisory with no base comparison was not shown to be new"
        );
    }

    /// A run with no cargo audit check has nothing to say about lockfiles, and
    /// says nothing — rather than an all-zero record a reader would take as a
    /// clean audit.
    #[test]
    fn a_run_without_cargo_audit_carries_no_audit_evidence() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let checks = vec![rustfmt_check("Diff in src/changed.rs:3:\n-old\n+new\n")];
        let summary =
            generate_inline_findings(tmp.path(), &checks, &[], None, None).expect("findings");
        assert!(summary.cargo_audit.is_none());
    }

    /// The introduced-advisory list is read off the SAME `in_diff` decision the
    /// SARIF rows carry, so the gate sentence cannot name an advisory the pack
    /// classified as pre-existing (or miss one it classified as new).
    #[test]
    fn introduced_advisory_ids_track_the_in_diff_decision() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let checks = [cargo_audit_check(
            crate::checks::CheckStatus::Failed,
            VULNERABLE_CARGO_AUDIT,
        )];
        let summary =
            generate_inline_findings(tmp.path(), &checks, &[], None, None).expect("findings");
        let evidence = summary.cargo_audit.as_ref().expect("evidence");
        let advisory_rows: Vec<_> = summary
            .dashboard_findings
            .iter()
            .filter(|finding| finding.check_id == "cargo_audit")
            .collect();
        let introduced: Vec<_> = advisory_rows
            .iter()
            .filter(|finding| finding.in_diff == Some(true))
            .collect();
        assert_eq!(
            evidence.new_advisory_ids.len(),
            introduced.len(),
            "the named ids and the in-diff rows are one classification"
        );
    }

    fn sarif_results(dir: &Path) -> Vec<serde_json::Value> {
        let sarif: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(dir.join("INLINE_FINDINGS.sarif")).expect("sarif"),
        )
        .expect("parse sarif");
        sarif["runs"][0]["results"]
            .as_array()
            .expect("sarif results")
            .clone()
    }

    #[test]
    fn generic_check_sarif_rows_carry_the_origin_tri_state() {
        // A check without a dedicated parser is still a SARIF result, and the
        // contract says every result carries `in_diff` + `classification`.
        let tmp = tempfile::tempdir().expect("tempdir");
        let checks = vec![rustfmt_check("Diff in src/changed.rs:3:\n-old\n+new\n")];
        let diffs = vec![one_file_diff("src/changed.rs")];
        generate_inline_findings(tmp.path(), &checks, &diffs, None, None).expect("findings");

        let results = sarif_results(tmp.path());
        assert_eq!(results.len(), 1);
        assert_eq!(results[0]["properties"]["in_diff"].as_bool(), Some(true));
        assert_eq!(
            results[0]["properties"]["classification"].as_str(),
            Some("introduced")
        );
    }

    #[test]
    fn unlocated_generic_sarif_row_reports_unclassified_not_absent() {
        // Falling back to the combined log establishes no origin. That is the
        // `null` / `unclassified` state, not a missing property a consumer
        // has to guess about.
        let tmp = tempfile::tempdir().expect("tempdir");
        let checks = vec![rustfmt_check("some formatting problem\n")];
        let diffs = vec![one_file_diff("src/changed.rs")];
        generate_inline_findings(tmp.path(), &checks, &diffs, None, None).expect("findings");

        let results = sarif_results(tmp.path());
        assert_eq!(results.len(), 1);
        assert!(results[0]["properties"]["in_diff"].is_null());
        assert_eq!(
            results[0]["properties"]["classification"].as_str(),
            Some("unclassified")
        );
    }

    #[test]
    fn cargo_audit_sarif_rows_classify_their_origin() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let checks = [cargo_audit_check(
            crate::checks::CheckStatus::Failed,
            VULNERABLE_CARGO_AUDIT,
        )];
        let diffs = [one_file_diff("Cargo.toml")];
        generate_inline_findings(tmp.path(), &checks, &diffs, None, None).expect("findings");

        for result in sarif_results(tmp.path()) {
            let in_diff = result["properties"]["in_diff"].as_bool();
            assert_eq!(
                result["properties"]["classification"].as_str(),
                Some(origin_classification(in_diff)),
                "classification must agree with in_diff on every advisory row"
            );
        }
    }

    #[test]
    fn origin_classification_covers_the_documented_tri_state() {
        assert_eq!(origin_classification(Some(true)), "introduced");
        assert_eq!(origin_classification(Some(false)), "preexisting");
        assert_eq!(origin_classification(None), "unclassified");
    }

    #[test]
    fn inline_gate_warns_on_new_warnings_only() {
        let warn = DashboardFinding {
            file: None,
            line: None,
            level: "warning",
            check_name: "Semgrep".to_string(),
            check_id: "semgrep_scan".to_string(),
            message: "w".to_string(),
            in_diff: Some(true),
        };
        assert_eq!(
            effective_inline_gate_class(&summary(vec![warn]), &clean()),
            GateClass::Info
        );
    }
}
