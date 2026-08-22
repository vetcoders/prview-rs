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
use std::path::{Path, PathBuf};
use std::time::Duration;

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
    let checks_summary = CliJsonChecksSummary::from_checks(&report.checks);

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
///   wants a warnings-clean trunk asks for that exit explicitly.
pub fn compute_exit_code(summary: &CliJsonSummary, fail_on_warnings: bool) -> i32 {
    use crate::policy::engine::MergeRecommendation;

    if summary.merge_recommendation == MergeRecommendation::Block {
        return 1;
    }
    let strict = summary.mode.execution_mode == "ci";
    if strict && !summary.quality_pass {
        return 1;
    }
    if strict && fail_on_warnings && summary.checks_summary.warned > 0 {
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

    let decision = value.get("decision").unwrap_or(&value);
    // Canonical verdict vocabulary (PV-03/04): PASS / CONDITIONAL / BLOCK. Legacy
    // ALLOW/HOLD tokens from pre-2.1 runs are folded onto the unified set so the
    // CLI `--json` surface speaks the same language as MERGE_GATE.json.
    let raw_verdict = decision.get("verdict").and_then(Value::as_str);
    let verdict = match raw_verdict {
        Some("PASS") | Some("ALLOW") => "PASS",
        Some("CONDITIONAL") | Some("HOLD") => "CONDITIONAL",
        Some("BLOCK") => "BLOCK",
        // Collapsing an unreadable verdict to BLOCK is the safe default, but it
        // is a normalization, not a reading — say so instead of letting the
        // caller mistake it for what the pack claimed.
        Some(other) => {
            caveats.push(format!(
                "unknown_verdict: MERGE_GATE.json verdict `{other}` is not in the \
                 PASS/CONDITIONAL/BLOCK vocabulary; normalized to BLOCK"
            ));
            "BLOCK"
        }
        None => {
            caveats.push(
                "unknown_verdict: MERGE_GATE.json decision carries no `verdict`; \
                 normalized to BLOCK"
                    .to_string(),
            );
            "BLOCK"
        }
    };

    let reason = decision
        .get("reason")
        .or_else(|| decision.get("decision_reason"))
        .and_then(Value::as_str)
        .map(|s| s.to_string());

    let allow_merge = decision
        .get("allow_merge")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let quality_pass = decision
        .get("quality_pass")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let analysis_status = match decision.get("analysis_status").and_then(Value::as_str) {
        Some("complete") => crate::policy::engine::AnalysisStatus::Complete,
        Some("degraded") => crate::policy::engine::AnalysisStatus::Degraded,
        Some("incomplete") => crate::policy::engine::AnalysisStatus::Incomplete,
        _ if allow_merge && quality_pass => crate::policy::engine::AnalysisStatus::Complete,
        _ => crate::policy::engine::AnalysisStatus::Incomplete,
    };
    let merge_recommendation = match decision.get("merge_recommendation").and_then(Value::as_str) {
        Some("approve") => crate::policy::engine::MergeRecommendation::Approve,
        Some("review_required") => crate::policy::engine::MergeRecommendation::ReviewRequired,
        _ if allow_merge => crate::policy::engine::MergeRecommendation::Approve,
        _ if verdict == "CONDITIONAL" => crate::policy::engine::MergeRecommendation::ReviewRequired,
        _ => crate::policy::engine::MergeRecommendation::Block,
    };
    Ok(MergeGateSummary {
        verdict: verdict.to_string(),
        analysis_status,
        merge_recommendation,
        allow_merge,
        quality_pass,
        reason,
        caveats,
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

/// One row of the config box: a horizontal rule, or a content line carrying
/// both its plain text (for width) and its coloured rendering (for display).
enum ConfigRow {
    Rule,
    Line { plain: String, colored: String },
}

const CONFIG_BOX_TITLE: &str = "PRVIEW CONFIG";
const CONFIG_BOX_MIN_INNER: usize = 64;

/// Inner width (columns between the two walls) for the config box: wide enough
/// for the longest content line and the title, with a one-column right margin,
/// never narrower than the historical minimum. All content is plain ASCII or
/// single-width symbols, so `chars().count()` is the display width.
fn config_box_inner_width(plain_lines: &[String], title: &str) -> usize {
    let widest = plain_lines
        .iter()
        .map(|s| s.chars().count())
        .max()
        .unwrap_or(0)
        .max(title.chars().count());
    widest.max(CONFIG_BOX_MIN_INNER - 1) + 1
}

/// Print configuration block
pub fn print_config(config: &Config, target: &ResolvedRef, bases: &[ResolvedRef]) {
    let mut rows: Vec<ConfigRow> = Vec::new();

    rows.push(ConfigRow::Line {
        plain: format!(" Target: {}", target.name),
        colored: format!(" {}: {}", "Target".bold(), target.name),
    });
    let target_sha = crate::git::short_sha(&target.commit_id);
    rows.push(ConfigRow::Line {
        plain: format!("    commit: {}", target_sha),
        colored: format!("    commit: {}", target_sha),
    });
    let target_src = if target.is_remote { "remote" } else { "local" };
    rows.push(ConfigRow::Line {
        plain: format!("    source: {}", target_src),
        colored: format!("    source: {}", target_src),
    });

    rows.push(ConfigRow::Rule);
    let mode = describe_run_mode(config);
    rows.push(ConfigRow::Line {
        plain: format!(" Mode: {}", mode),
        colored: format!(" {}: {}", "Mode".bold(), mode),
    });
    let checks = describe_enabled_steps(config);
    rows.push(ConfigRow::Line {
        plain: format!(" Checks: {}", checks),
        colored: format!(" {}: {}", "Checks".bold(), checks),
    });
    if config.is_fast_remote_only_standard() {
        let note = "fast remote-only preset skips tests and heuristics; use --with-tests, --with-lint, or --deep for a heavier pass";
        rows.push(ConfigRow::Line {
            plain: format!("    note: {}", note),
            colored: format!("    note: {}", note.dimmed()),
        });
    }

    rows.push(ConfigRow::Rule);
    rows.push(ConfigRow::Line {
        plain: " Bases:".to_string(),
        colored: format!(" {}:", "Bases".bold()),
    });
    for base in bases {
        let sha = crate::git::short_sha(&base.commit_id);
        let src = if base.is_remote { "remote" } else { "local" };
        rows.push(ConfigRow::Line {
            plain: format!("    ✓ {} → {} [{}]", base.name, sha, src),
            colored: format!(
                "    {} {} → {} [{}]",
                "✓".green(),
                base.name,
                sha,
                src.dimmed()
            ),
        });
    }

    let plain_lines: Vec<String> = rows
        .iter()
        .filter_map(|r| match r {
            ConfigRow::Line { plain, .. } => Some(plain.clone()),
            ConfigRow::Rule => None,
        })
        .collect();
    let inner = config_box_inner_width(&plain_lines, CONFIG_BOX_TITLE);

    let heavy = "═".repeat(inner);
    let light = "─".repeat(inner);

    println!("{}", format!("╔{heavy}╗").cyan());

    let title_w = CONFIG_BOX_TITLE.chars().count();
    let left = (inner - title_w) / 2;
    let right = inner - title_w - left;
    println!(
        "{}",
        format!(
            "║{}{}{}║",
            " ".repeat(left),
            CONFIG_BOX_TITLE,
            " ".repeat(right)
        )
        .cyan()
        .bold()
    );
    println!("{}", format!("╠{heavy}╣").cyan());

    for row in &rows {
        match row {
            ConfigRow::Rule => println!("{}", format!("╟{light}╢").cyan()),
            ConfigRow::Line { plain, colored } => {
                let pad = " ".repeat(inner.saturating_sub(plain.chars().count()));
                println!("{}{}{}{}", "║".cyan(), colored, pad, "║".cyan());
            }
        }
    }

    println!("{}", format!("╚{heavy}╝").cyan());
    println!();
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
        let inner = config_box_inner_width(&rows, CONFIG_BOX_TITLE);
        let widest = rows.iter().map(|r| r.chars().count()).max().unwrap();

        // Every content line fits with a right margin, and the box never shrinks
        // below the historical minimum or the title width.
        assert!(inner > widest, "longest line must fit with a right margin");
        assert!(inner >= CONFIG_BOX_MIN_INNER);
        assert!(inner >= CONFIG_BOX_TITLE.chars().count());

        // Padding makes every content row reach exactly `inner` columns, so the
        // right wall lines up with the borders (the bug in the screenshot).
        for r in &rows {
            let pad = inner - r.chars().count();
            assert_eq!(r.chars().count() + pad, inner);
        }
        // Centered title also fills the row exactly.
        let tw = CONFIG_BOX_TITLE.chars().count();
        let left = (inner - tw) / 2;
        let right = inner - tw - left;
        assert_eq!(left + tw + right, inner);
    }

    #[test]
    fn config_box_inner_width_floors_at_minimum() {
        let rows = vec![" Mode: standard".to_string()];
        assert_eq!(
            config_box_inner_width(&rows, CONFIG_BOX_TITLE),
            CONFIG_BOX_MIN_INNER
        );
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
        assert_eq!(compute_exit_code(&summary, false), 0);
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
        assert_eq!(compute_exit_code(&summary, false), 0);
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
        assert_eq!(compute_exit_code(&summary, false), 1);
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
        assert_eq!(compute_exit_code(&summary, false), 0);
        assert_eq!(compute_exit_code(&summary, true), 1);
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
        assert_eq!(compute_exit_code(&summary, true), 0);
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
        assert_eq!(compute_exit_code(&summary, false), 1);
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
        for (legacy, unified) in [("ALLOW", "PASS"), ("HOLD", "CONDITIONAL")] {
            let pack = pack_with_gate(&format!(r#"{{"verdict":"{legacy}","allow_merge":false}}"#));
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
