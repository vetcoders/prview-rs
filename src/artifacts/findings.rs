//! INLINE_FINDINGS generation and gate-class helpers.

use super::*;

#[derive(Debug)]
pub(super) struct InlineFindingsSummary {
    pub(super) status: String,
    pub(super) findings_count: usize,
    pub(super) dashboard_findings: Vec<DashboardFinding>,
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
/// locations are an exhaustive baseline signal (`check_id_is_baseline_signal`).
/// For whole-project parsers (e.g. `cargo_test`) an out-of-diff location is
/// causation-unknown — the diff may have caused it — so it still gates, exactly
/// as `classify_quality_failure` keeps it Unclassified (R2-8).
pub(super) fn effective_inline_gate_class(inline: &InlineFindingsSummary) -> GateClass {
    let mut new_errors = 0usize;
    let mut new_warnings = 0usize;
    for finding in &inline.dashboard_findings {
        let out_of_diff_preexisting =
            finding.in_diff == Some(false) && check_id_is_baseline_signal(&finding.check_id);
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
) -> (serde_json::Value, Option<String>) {
    use serde_json::json;

    let severity = config.policy.severity_for("heuristics_loctree");
    let (status, class, dead, cycles) = if !config.run_heuristics {
        ("skipped", GateClass::Skip, 0usize, 0usize)
    } else if let Some(h) = heuristics {
        let dead = h.summary.dead_exports;
        let cycles = h.summary.circular_imports;
        if h.summary.total_files == 0 {
            // Loctree ran but scanned no files — treat as SKIP, not PASS
            ("skipped", GateClass::Skip, dead, cycles)
        } else if dead > 0 || cycles > 0 {
            ("warnings", GateClass::Info, dead, cycles)
        } else {
            ("passed", GateClass::Pass, dead, cycles)
        }
    } else {
        ("skipped", GateClass::Skip, 0usize, 0usize)
    };

    let blocking = config.policy.is_blocking(severity, class);
    let blocking_issue = if blocking {
        Some(format!(
            "Loctree heuristics (dead_exports={}, cycles={})",
            dead, cycles
        ))
    } else {
        None
    };

    let check = json!({
        "id": "heuristics_loctree",
        "name": "Loctree Heuristics",
        "status": status,
        "class": gate_class_to_str(class),
        "severity": policy_severity_to_str(severity),
        "blocking": blocking,
        "duration_secs": 0.0,
        "cached": false,
        "evidence": "20_quality/heuristics_loctree.result.json",
        "log": "20_quality/heuristics_loctree.log",
    });

    (check, blocking_issue)
}

pub(super) fn generate_inline_findings(
    dir: &Path,
    checks: &[CheckResult],
    diffs: &[crate::git::Diff],
    deps_delta: Option<&signal::DepsDelta>,
    repo: Option<&crate::git::Repository>,
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

    for check in checks {
        let class = gate_class_for_check(check.status);
        if !matches!(class, GateClass::Fail | GateClass::Info) {
            continue;
        }

        let check_id = check_id_from_name(&check.name);

        // Dispatch to structured parsers first.
        match check_id.as_str() {
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

        // Cargo audit: keep existing structured parsing in its own run.
        if check.name.eq_ignore_ascii_case("cargo audit") {
            let audit_findings = parse_cargo_audit_findings(&check.output);
            if !audit_findings.is_empty() {
                let location = cargo_audit_location_for_check(check);
                let _audit_in_diff_fallback = is_in_diff("Cargo.lock");
                let base_audit_cache = get_base_cargo_audit_findings(repo, diffs);

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

                    let current_audit_in_diff = if let Some(cache) = &base_audit_cache {
                        !cache
                            .contains(&(finding.advisory_id.clone(), finding.package_name.clone()))
                    } else if let Some(deps) = deps_delta {
                        deps.added.contains(&finding.package_name)
                            || deps.changed.contains(&finding.package_name)
                    } else {
                        _audit_in_diff_fallback
                    };

                    dashboard_findings.push(DashboardFinding {
                        level: finding.sarif_level,
                        check_name: check.name.clone(),
                        check_id: check_id.clone(),
                        message: finding.sarif_message(),
                        in_diff: Some(current_audit_in_diff),
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
                            "package": finding.package_display(),
                            "severity": finding.severity,
                        }
                    }));
                }
                continue;
            }
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
        let sarif_location = if let Some((ref file, line_num)) = extracted {
            let in_diff_val = is_in_diff(file);
            dashboard_findings.push(DashboardFinding {
                level,
                check_name: check.name.clone(),
                check_id: check_id.clone(),
                message: first_line.to_string(),
                in_diff: Some(in_diff_val),
            });
            json!({
                "physicalLocation": {
                    "artifactLocation": { "uri": file },
                    "region": { "startLine": line_num }
                }
            })
        } else {
            dashboard_findings.push(DashboardFinding {
                level,
                check_name: check.name.clone(),
                check_id: check_id.clone(),
                message: first_line.to_string(),
                in_diff: None,
            });
            json!({
                "physicalLocation": {
                    "artifactLocation": { "uri": "20_quality/full-checks.log" }
                }
            })
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
            "locations": [sarif_location]
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

            let in_diff = is_in_diff(&finding.file);
            if in_diff {
                in_diff_count += 1;
            } else {
                preexisting_count += 1;
            }
            let classification = if in_diff { "introduced" } else { "preexisting" };

            dashboard_findings.push(DashboardFinding {
                level: finding.level,
                check_name: tool_set.tool_name.to_string(),
                check_id: tool_set.check_id.clone(),
                message: finding.message.clone(),
                in_diff: Some(in_diff),
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
    use super::*;

    fn err(in_diff: Option<bool>) -> DashboardFinding {
        DashboardFinding {
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
        }
    }

    #[test]
    fn inline_gate_ignores_preexisting_only_errors() {
        // THREAD 7: raw status is "failed" but every error is pre-existing, so
        // the aggregate gate must not fire (it would block under policy-mode
        // block despite every per-check evaluation approving these findings).
        let inline = summary(vec![err(Some(false)), err(Some(false))]);
        assert_eq!(effective_inline_gate_class(&inline), GateClass::Pass);
    }

    #[test]
    fn inline_gate_blocks_on_introduced_errors() {
        let inline = summary(vec![err(Some(false)), err(Some(true))]);
        assert_eq!(effective_inline_gate_class(&inline), GateClass::Fail);
    }

    #[test]
    fn inline_gate_blocks_on_unclassified_errors() {
        // Causation unknown (in_diff == None) is treated as new, never downgraded.
        let inline = summary(vec![err(None)]);
        assert_eq!(effective_inline_gate_class(&inline), GateClass::Fail);
    }

    #[test]
    fn inline_gate_counts_out_of_diff_whole_project_parser_rows() {
        // R2-8: a cargo_test out-of-diff row is causation-unknown (the diff may
        // have broken a test in an unchanged file), not pre-existing, so it must
        // still gate — unlike a baseline-signal semgrep out-of-diff row, which
        // does not (proven by inline_gate_ignores_preexisting_only_errors).
        let cargo_test_row = DashboardFinding {
            level: "error",
            check_name: "Cargo Test".to_string(),
            check_id: "cargo_test".to_string(),
            message: "test failed".to_string(),
            in_diff: Some(false),
        };
        assert_eq!(
            effective_inline_gate_class(&summary(vec![cargo_test_row])),
            GateClass::Fail
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
    fn inline_gate_warns_on_new_warnings_only() {
        let warn = DashboardFinding {
            level: "warning",
            check_name: "Semgrep".to_string(),
            check_id: "semgrep_scan".to_string(),
            message: "w".to_string(),
            in_diff: Some(true),
        };
        assert_eq!(
            effective_inline_gate_class(&summary(vec![warn])),
            GateClass::Info
        );
    }
}
