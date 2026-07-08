//! report.json v1 generator
//!
//! Single source of truth for the dashboard and external tooling.
//! All data the dashboard needs is serialized here; the HTML renderer
//! reads from embedded JSON rather than parsing multiple text files.

use super::parsers::cargo_test::extract_failed_test_names;
use super::signal::BreakingKind;
use super::{
    DashboardContext, breaking_change_breakdown, build_merge_decision_view, build_review_caveats,
    cargo_audit_review_caveats,
};
use crate::checks::{CheckResult, CheckStatus};
use crate::config::Config;
use crate::git::{Diff, FileStatus, ResolvedRef};
use anyhow::Result;
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use super::signal::{
    ArtifactCounters, HOTSPOT_THRESHOLD, compute_risk_heatmap, detect_orphaned_resource_delete,
    find_per_file_patch, read_disk_artifact_counters,
};

const GENERATED_PATH_PREFIXES: &[&str] = &[
    "target/",
    "dist/",
    "build/",
    "coverage/",
    "node_modules/",
    ".next/",
    ".nuxt/",
    "__pycache__/",
    ".tox/",
    "vendor/",
    ".venv/",
    "venv/",
];

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Input bundle for report generation (avoids long parameter lists).
pub struct ReportInput<'a> {
    pub dir: &'a Path,
    pub config: &'a Config,
    pub diffs: &'a [Diff],
    pub checks: &'a [CheckResult],
    pub resolved_target: &'a ResolvedRef,
    pub resolved_bases: &'a [ResolvedRef],
    pub ctx: &'a DashboardContext,
    pub run_started_at: &'a str,
    pub heuristics: Option<&'a crate::heuristics::HeuristicsResult>,
    pub regression: Option<&'a crate::regression::RegressionReport>,
}

/// Generate `report.json` in the artifact root directory.
pub fn generate(input: &ReportInput<'_>) -> Result<()> {
    let report = build_report(input);
    let json = serde_json::to_string_pretty(&report)?;
    std::fs::write(input.dir.join("report.json"), json)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Schema structures (Serialize-only — not deserialized in this crate)
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct Report {
    schema_version: &'static str,
    meta: Meta,
    gate: Gate,
    checks: Vec<CheckEntry>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    checks_skipped: Vec<SkippedCheckEntry>,
    diff: DiffSection,
    quality: Quality,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    ownership: Vec<OwnershipMapEntry>,
    #[serde(skip_serializing_if = "Option::is_none")]
    regression: Option<crate::regression::RegressionReport>,
    artifacts: Artifacts,
    #[serde(skip_serializing_if = "Option::is_none")]
    ui_hints: Option<UiHints>,
}

#[derive(Serialize)]
struct OwnershipMapEntry {
    path: String,
    owner: String,
}

#[derive(Serialize)]
struct SkippedCheckEntry {
    id: String,
    name: String,
    reason: String,
}

#[derive(Serialize)]
struct Meta {
    generated_at: String,
    tool: ToolMeta,
    #[serde(skip_serializing_if = "Option::is_none")]
    repo: Option<RepoMeta>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pr: Option<PrMeta>,
    range: RangeMeta,
}

#[derive(Serialize)]
struct ToolMeta {
    name: &'static str,
    version: String,
}

#[derive(Serialize)]
struct RepoMeta {
    full_name: String,
}

#[derive(Serialize)]
struct PrMeta {
    number: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    url: Option<String>,
}

#[derive(Serialize)]
struct RangeMeta {
    #[serde(skip_serializing_if = "Option::is_none")]
    base: Option<String>,
    head: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    merge_base: Option<String>,
}

#[derive(Serialize)]
struct Gate {
    verdict: &'static str,
    analysis_status: crate::policy::engine::AnalysisStatus,
    merge_recommendation: crate::policy::engine::MergeRecommendation,
    allow_merge: bool,
    policy_allow_merge: bool,
    quality_pass: bool,
    recommended_merge: bool,
    recommended_label: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    quality_failures: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    introduced_quality_failures: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    preexisting_quality_failures: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    mixed_quality_failures: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    unclassified_quality_failures: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    quality_failure_details: Vec<GateQualityFailureDetail>,
    policy_mode: String,
    status: &'static str,
    summary: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    review_caveats: Vec<String>,
    reasons: Vec<GateReason>,
    source: &'static str,
}

#[derive(Serialize)]
struct GateReason {
    kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    check_id: Option<String>,
    blocking: bool,
    message: String,
}

#[derive(Serialize)]
struct GateQualityFailureDetail {
    name: String,
    classification: &'static str,
}

#[derive(Serialize)]
struct CheckEntry {
    id: String,
    name: String,
    status: &'static str,
    blocking: bool,
    cached: bool,
    duration_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    command: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cwd: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error_excerpt: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    signatures: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    finding_stats: Option<FindingStats>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    failed_tests: Vec<String>,
    artifacts: CheckArtifacts,
}

#[derive(Serialize, Debug, Clone, PartialEq, Eq)]
struct FindingStats {
    total_findings: usize,
    real_findings: usize,
    generated_path_findings: usize,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    generated_paths_matched: Vec<String>,
}

#[derive(Serialize)]
struct CheckArtifacts {
    #[serde(skip_serializing_if = "Option::is_none")]
    log_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    result_json_path: Option<String>,
}

#[derive(Serialize)]
struct DiffSection {
    stats: ReportDiffStats,
    full_patch_path: &'static str,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    directories: Vec<DirEntry>,
    files: Vec<FileEntry>,
}

#[derive(Serialize, Debug, Clone, PartialEq, Eq)]
struct DirEntry {
    path: String,
    files_changed: usize,
    insertions: usize,
    deletions: usize,
    churn: usize,
}

#[derive(Serialize)]
struct ReportDiffStats {
    files_changed: usize,
    insertions: usize,
    deletions: usize,
    commits: usize,
    renames: usize,
    added: usize,
    modified: usize,
    deleted: usize,
    copied: usize,
}

#[derive(Serialize)]
struct FileEntry {
    path: String,
    status: &'static str,
    additions: usize,
    deletions: usize,
    churn: usize,
    is_hotspot: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    patch_path: Option<String>,
}

fn aggregate_directory_entries(file_entries: &[FileEntry]) -> Vec<DirEntry> {
    let mut directories: BTreeMap<String, DirEntry> = BTreeMap::new();

    for file_entry in file_entries {
        let dir = file_entry
            .path
            .rsplit_once('/')
            .map(|(parent, _)| parent)
            .unwrap_or(".")
            .to_string();

        let entry = directories.entry(dir.clone()).or_insert_with(|| DirEntry {
            path: dir,
            files_changed: 0,
            insertions: 0,
            deletions: 0,
            churn: 0,
        });
        entry.files_changed += 1;
        entry.insertions += file_entry.additions;
        entry.deletions += file_entry.deletions;
        entry.churn += file_entry.churn;
    }

    let mut entries: Vec<DirEntry> = directories.into_values().collect();
    entries.sort_by(|a, b| b.churn.cmp(&a.churn).then_with(|| a.path.cmp(&b.path)));
    entries
}

fn finding_stats_from_output(output: &str) -> Option<FindingStats> {
    let mut total_findings = 0;
    let mut generated_path_findings = 0;
    let mut generated_paths_matched = BTreeSet::new();

    for line in output.lines() {
        let trimmed = line.trim();
        if !looks_like_finding_line(trimmed) {
            continue;
        }

        total_findings += 1;
        for prefix in GENERATED_PATH_PREFIXES {
            if trimmed.contains(prefix) {
                generated_path_findings += 1;
                generated_paths_matched.insert((*prefix).to_string());
                break;
            }
        }
    }

    if total_findings == 0 {
        None
    } else {
        Some(FindingStats {
            total_findings,
            real_findings: total_findings.saturating_sub(generated_path_findings),
            generated_path_findings,
            generated_paths_matched: {
                let mut v: Vec<String> = generated_paths_matched.into_iter().collect();
                v.sort();
                v
            },
        })
    }
}

/// Compute finding stats for the semgrep check directly from its JSON output.
///
/// The generic line-based [`finding_stats_from_output`] heuristic mis-counts
/// semgrep's single-line JSON: paths and `line`/`col` offsets embedded in the
/// blob trip the `path:line` detector, so `report.json` reported phantom
/// findings (`total_findings: 2`) even when the scan log clearly held
/// `results: []`. The authoritative count is the length of the JSON `results`
/// array, so parse it directly and let the report agree with the log it links
/// to. `errors[]` (PartialParsing) are surfaced via the check status, not here.
fn semgrep_finding_stats_from_output(output: &str) -> Option<FindingStats> {
    let start = output.find('{')?;
    // Callers hand us stdout+stderr combined, and semgrep prints its JSON to
    // stdout but diagnostics (deprecation notices, scan warnings) to stderr. A
    // trailing stderr line after the closing brace makes a plain `from_str`
    // reject the whole blob, silently dropping every finding. A streaming
    // deserializer reads the first complete JSON value and ignores whatever
    // non-JSON trails it (PR #12 review #19).
    let parsed = serde_json::Deserializer::from_str(&output[start..])
        .into_iter::<serde_json::Value>()
        .next()?
        .ok()?;
    let results = parsed.get("results")?.as_array()?;
    if results.is_empty() {
        return None;
    }

    let mut generated_path_findings = 0;
    let mut generated_paths_matched = BTreeSet::new();
    for result in results {
        if let Some(path) = result.get("path").and_then(|p| p.as_str()) {
            for prefix in GENERATED_PATH_PREFIXES {
                if path.contains(prefix) {
                    generated_path_findings += 1;
                    generated_paths_matched.insert((*prefix).to_string());
                    break;
                }
            }
        }
    }

    let total_findings = results.len();
    Some(FindingStats {
        total_findings,
        real_findings: total_findings.saturating_sub(generated_path_findings),
        generated_path_findings,
        generated_paths_matched: {
            let mut v: Vec<String> = generated_paths_matched.into_iter().collect();
            v.sort();
            v
        },
    })
}

fn looks_like_finding_line(line: &str) -> bool {
    if line.is_empty()
        || line.starts_with('✓')
        || line.starts_with('─')
        || line.starts_with('=')
        || line.starts_with("note:")
    {
        return false;
    }

    let lower = line.to_ascii_lowercase();
    let noisy_prefixes = [
        "running ",
        "checking ",
        "compiling ",
        "finished ",
        "downloading ",
        "updating ",
        "warning: ",
        "error: could not compile ",
    ];
    if noisy_prefixes
        .iter()
        .any(|prefix| lower.starts_with(prefix))
    {
        return false;
    }

    if !line.contains('/') {
        return false;
    }

    lower.contains(" warning ")
        || lower.contains(" error ")
        || lower.contains(" failed ")
        || lower.contains(" fail ")
        || lower.contains(" panic")
        || lower.contains(" panicked")
        || has_path_with_line_number(line)
        || lower.contains("line ") && lower.contains("col ")
}

fn has_path_with_line_number(line: &str) -> bool {
    line.match_indices(':').any(|(idx, _)| {
        line[idx + 1..]
            .chars()
            .next()
            .is_some_and(|ch| ch.is_ascii_digit())
    })
}

#[derive(Serialize)]
struct Quality {
    breaking_changes: BreakingSection,
    coverage: CoverageSection,
    sarif: SarifSection,
    heuristics: HeuristicsSection,
    risk_heatmap: RiskHeatmapSection,
    semantic_findings: SemanticSection,
    consistency: ConsistencySection,
}

#[derive(Serialize)]
struct HeuristicsSection {
    available: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    dead_exports: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cycles: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    twins: Option<usize>,
    #[serde(rename = "unused_symbols")]
    #[serde(skip_serializing_if = "Option::is_none")]
    dead_parrots: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    log_path: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    analysis_root: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    regression: Option<crate::heuristics::HeuristicsRegression>,
}

#[derive(Serialize)]
struct BreakingSection {
    has_breaking: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    summary: Option<String>,
    md_path: &'static str,
    removed_public_symbols_count: usize,
    signature_changes_count: usize,
    new_env_vars_count: usize,
}

#[derive(Serialize)]
struct CoverageSection {
    heuristic_ratio: f64,
    matched: usize,
    total: usize,
    txt_path: &'static str,
    changed_tests_path: &'static str,
}

#[derive(Serialize)]
struct RiskHeatmapSection {
    risk_level: &'static str,
    total_risk_score: u32,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    hot_zones: Vec<RiskZoneEntry>,
}

#[derive(Serialize)]
struct RiskZoneEntry {
    name: String,
    files_touched: usize,
    total_churn: usize,
    max_file_risk: u32,
}

#[derive(Serialize)]
struct SarifSection {
    findings_count: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    sarif_path: Option<&'static str>,
}

#[derive(Serialize)]
struct SemanticSection {
    findings_count: usize,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    findings: Vec<SemanticFindingEntry>,
}

#[derive(Serialize)]
struct SemanticFindingEntry {
    rule_id: &'static str,
    severity: &'static str,
    confidence: &'static str,
    title: String,
    description: String,
    evidence_count: usize,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    evidence: Vec<SemanticEvidenceEntry>,
}

#[derive(Serialize)]
struct SemanticEvidenceEntry {
    file: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    line: Option<usize>,
    role: &'static str,
    snippet: String,
}

#[derive(Serialize)]
struct ConsistencySection {
    consistent: bool,
    checked_fields: usize,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    warnings: Vec<ConsistencyWarningEntry>,
}

#[derive(Serialize)]
struct ConsistencyWarningEntry {
    field: String,
    message: String,
    sources: Vec<ConsistencySourceEntry>,
}

#[derive(Serialize)]
struct ConsistencySourceEntry {
    artifact: String,
    value: String,
}

#[derive(Serialize)]
struct Artifacts {
    zip_path: &'static str,
    manifest_path: &'static str,
}

#[derive(Serialize)]
struct UiHints {
    default_hide_globs: Vec<&'static str>,
    default_sort: &'static str,
}

// ---------------------------------------------------------------------------
// Builder
// ---------------------------------------------------------------------------

fn build_report(input: &ReportInput<'_>) -> Report {
    let ReportInput {
        config,
        diffs,
        checks,
        resolved_target,
        resolved_bases,
        ctx,
        run_started_at,
        ..
    } = input;

    let diff_merge_base = diffs
        .first()
        .map(|diff| diff.base_commit_id.clone())
        .or_else(|| resolved_bases.first().map(|base| base.commit_id.clone()));

    // -- meta --
    let meta = Meta {
        generated_at: run_started_at.to_string(),
        tool: ToolMeta {
            name: env!("CARGO_PKG_NAME"),
            version: env!("CARGO_PKG_VERSION").to_string(),
        },
        repo: config.gh_repo.as_ref().map(|r| RepoMeta {
            full_name: r.clone(),
        }),
        pr: config.pr_number.map(|n| PrMeta {
            number: n,
            url: config.pr_url.clone(),
        }),
        range: RangeMeta {
            base: resolved_bases.first().map(|b| b.name.clone()),
            head: resolved_target.name.clone(),
            merge_base: diff_merge_base,
        },
    };

    // -- gate --
    let review_caveats = if ctx.review_caveats.is_empty() {
        let mut generated = build_review_caveats(&ctx.breaking, &ctx.coverage, ctx.findings.len());
        generated.extend(cargo_audit_review_caveats(input.checks));
        generated
    } else {
        ctx.review_caveats.clone()
    };
    let decision = build_merge_decision_view(
        ctx.policy_allow_merge,
        ctx.quality_pass,
        ctx.recommended_merge,
        &ctx.quality_failures,
        &ctx.quality_failure_details,
        &ctx.blocking_issues,
        review_caveats.clone(),
    );
    let status = if ctx.allow_merge { "ALLOW" } else { "BLOCK" };
    let summary = decision.reason.clone();

    let mut reasons: Vec<GateReason> = Vec::new();
    for entry in &ctx.check_gates {
        if entry.class == "FAIL" || entry.class == "INFO" {
            let kind = if entry.class == "FAIL" {
                "CHECK_FAIL"
            } else {
                "CHECK_WARN"
            };
            reasons.push(GateReason {
                kind,
                check_id: Some(entry.id.clone()),
                blocking: entry.blocking,
                message: format!("{} ({})", entry.name, entry.class),
            });
        }
    }
    for caveat in &decision.review_caveats {
        reasons.push(GateReason {
            kind: "REVIEW_SIGNAL",
            check_id: None,
            blocking: false,
            message: caveat.clone(),
        });
    }

    let gate = Gate {
        verdict: ctx.verdict,
        analysis_status: ctx.analysis_status,
        merge_recommendation: ctx.merge_recommendation,
        allow_merge: ctx.allow_merge,
        policy_allow_merge: ctx.policy_allow_merge,
        quality_pass: ctx.quality_pass,
        recommended_merge: ctx.recommended_merge,
        recommended_label: decision.state.gate_label().to_string(),
        quality_failures: ctx.quality_failures.clone(),
        introduced_quality_failures: ctx.introduced_quality_failures.clone(),
        preexisting_quality_failures: ctx.preexisting_quality_failures.clone(),
        mixed_quality_failures: ctx.mixed_quality_failures.clone(),
        unclassified_quality_failures: ctx.unclassified_quality_failures.clone(),
        quality_failure_details: ctx
            .quality_failure_details
            .iter()
            .map(|detail| GateQualityFailureDetail {
                name: detail.name.clone(),
                classification: detail.classification.as_str(),
            })
            .collect(),
        policy_mode: ctx.policy_mode.to_string(),
        status,
        summary,
        review_caveats,
        reasons,
        source: "00_summary/MERGE_GATE.json",
    };

    // -- checks --
    let check_entries: Vec<CheckEntry> = checks
        .iter()
        .map(|c| {
            let gate_entry = ctx.check_gates.iter().find(|g| g.name == c.name);
            let id = gate_entry
                .map(|g| g.id.clone())
                .unwrap_or_else(|| c.name.to_lowercase().replace(' ', "_"));
            let blocking = gate_entry.map(|g| g.blocking).unwrap_or(false);

            let status_str = match c.status {
                CheckStatus::Passed => "PASS",
                CheckStatus::Failed => "FAIL",
                CheckStatus::Error => "ERROR",
                CheckStatus::Skipped => "SKIP",
                CheckStatus::Warnings => "WARN",
            };

            let error_excerpt = if matches!(c.status, CheckStatus::Failed | CheckStatus::Error) {
                c.output.lines().find(|l| !l.trim().is_empty()).map(|l| {
                    let end = l.floor_char_boundary(200);
                    l[..end].to_string()
                })
            } else {
                None
            };

            let (command, cwd, signatures) = if let Some(ref prov) = c.provenance {
                (
                    Some(prov.command.clone()),
                    Some(prov.cwd.clone()),
                    if prov.hard_fail_signatures.is_empty() {
                        None
                    } else {
                        Some(prov.hard_fail_signatures.clone())
                    },
                )
            } else {
                (None, None, None)
            };

            let failed_tests = if id == "cargo_test"
                && matches!(c.status, CheckStatus::Failed | CheckStatus::Error)
            {
                extract_failed_test_names(&c.output)
            } else {
                Vec::new()
            };

            CheckEntry {
                id: id.clone(),
                name: c.name.clone(),
                status: status_str,
                blocking,
                cached: c.cached,
                duration_ms: c.duration.as_millis() as u64,
                command,
                cwd,
                error_excerpt,
                signatures,
                finding_stats: if id == "semgrep_scan" {
                    // Semgrep emits JSON; count the `results` array directly so
                    // report.json cannot disagree with the scan log it links to.
                    semgrep_finding_stats_from_output(&c.output)
                } else {
                    finding_stats_from_output(&c.output)
                },
                failed_tests,
                artifacts: CheckArtifacts {
                    log_path: Some(format!("20_quality/{}.log", id)),
                    result_json_path: Some(format!("20_quality/{}.result.json", id)),
                },
            }
        })
        .collect();

    // -- diff --
    let all_files: Vec<_> = diffs.iter().flat_map(|d| &d.files).collect();
    let total_commits: usize = diffs.iter().map(|d| d.commits.len()).sum();
    let total_adds: usize = all_files.iter().map(|f| f.additions).sum();
    let total_dels: usize = all_files.iter().map(|f| f.deletions).sum();
    let count_renames = all_files
        .iter()
        .filter(|f| f.status == FileStatus::Renamed)
        .count();
    let count_added = all_files
        .iter()
        .filter(|f| f.status == FileStatus::Added)
        .count();
    let count_modified = all_files
        .iter()
        .filter(|f| f.status == FileStatus::Modified)
        .count();
    let count_deleted = all_files
        .iter()
        .filter(|f| f.status == FileStatus::Deleted)
        .count();
    let count_copied = all_files
        .iter()
        .filter(|f| f.status == FileStatus::Copied)
        .count();

    let file_entries: Vec<FileEntry> = all_files
        .iter()
        .map(|f| {
            let churn = f.additions + f.deletions;
            let is_hotspot = churn >= HOTSPOT_THRESHOLD;
            let matched_patch = find_per_file_patch(&f.path, &ctx.per_file_diff_files);

            FileEntry {
                path: f.path.clone(),
                status: match f.status {
                    FileStatus::Added => "A",
                    FileStatus::Modified => "M",
                    FileStatus::Deleted => "D",
                    FileStatus::Renamed => "R",
                    FileStatus::Copied => "C",
                },
                additions: f.additions,
                deletions: f.deletions,
                churn,
                is_hotspot,
                patch_path: matched_patch.map(|name| format!("10_diff/per-file-diffs/{name}")),
            }
        })
        .collect();

    let diff_section = DiffSection {
        stats: ReportDiffStats {
            files_changed: all_files.len(),
            insertions: total_adds,
            deletions: total_dels,
            commits: total_commits,
            renames: count_renames,
            added: count_added,
            modified: count_modified,
            deleted: count_deleted,
            copied: count_copied,
        },
        full_patch_path: "10_diff/full.patch",
        directories: aggregate_directory_entries(&file_entries),
        files: file_entries,
    };

    // -- quality --
    let removed_symbols = ctx
        .breaking
        .iter()
        .filter(|b| matches!(b.kind, BreakingKind::RemovedSymbol { .. }))
        .count();
    let new_env_vars = ctx
        .breaking
        .iter()
        .filter(|b| matches!(b.kind, BreakingKind::NewEnvRequirement { .. }))
        .count();
    let signature_changes = ctx
        .breaking
        .iter()
        .filter(|b| matches!(b.kind, BreakingKind::ChangedSignature { .. }))
        .count();

    let heuristics_section = match input.heuristics {
        Some(h) => {
            let loctree = h.loctree.as_ref();
            HeuristicsSection {
                available: loctree.map(|l| l.available).unwrap_or(false),
                dead_exports: loctree.map(|l| l.dead_exports.len()),
                cycles: loctree.map(|l| l.cycles.len()),
                twins: loctree.map(|l| l.twins.exact_twins.len()),
                dead_parrots: loctree.map(|l| l.twins.dead_parrots.len()),
                log_path: Some("20_quality/heuristics_loctree.log"),
                analysis_root: h.analysis_root.clone(),
                regression: h.regression.clone(),
            }
        }
        None => HeuristicsSection {
            available: false,
            dead_exports: None,
            cycles: None,
            twins: None,
            dead_parrots: None,
            log_path: None,
            analysis_root: None,
            regression: None,
        },
    };

    let breaking_breakdown = breaking_change_breakdown(&ctx.breaking);

    let quality = Quality {
        breaking_changes: BreakingSection {
            has_breaking: !ctx.breaking.is_empty(),
            summary: breaking_breakdown.summary().or_else(|| {
                (!ctx.breaking.is_empty())
                    .then(|| format!("{} breaking findings", ctx.breaking.len()))
            }),
            md_path: "20_quality/BREAKING_CHANGES.md",
            removed_public_symbols_count: removed_symbols,
            signature_changes_count: signature_changes,
            new_env_vars_count: new_env_vars,
        },
        coverage: CoverageSection {
            heuristic_ratio: if ctx.coverage.total_source > 0 {
                ctx.coverage.covered_count as f64 / ctx.coverage.total_source as f64
            } else {
                1.0
            },
            matched: ctx.coverage.covered_count,
            total: ctx.coverage.total_source,
            txt_path: "20_quality/coverage-delta.txt",
            changed_tests_path: "30_context/changed-tests.txt",
        },
        sarif: SarifSection {
            findings_count: ctx.findings.len(),
            sarif_path: if ctx.findings.is_empty() {
                None
            } else {
                Some("30_context/INLINE_FINDINGS.sarif")
            },
        },
        heuristics: heuristics_section,
        risk_heatmap: {
            let heatmap = compute_risk_heatmap(diffs, &ctx.risk_scores);
            RiskHeatmapSection {
                risk_level: heatmap.risk_level,
                total_risk_score: heatmap.total_risk_score,
                hot_zones: heatmap
                    .zones
                    .into_iter()
                    .map(|zone| RiskZoneEntry {
                        name: zone.name.to_string(),
                        files_touched: zone.files_touched,
                        total_churn: zone.total_churn,
                        max_file_risk: zone.max_file_risk,
                    })
                    .collect(),
            }
        },
        semantic_findings: {
            let findings = detect_orphaned_resource_delete(diffs);
            SemanticSection {
                findings_count: findings.len(),
                findings: findings
                    .into_iter()
                    .map(|finding| SemanticFindingEntry {
                        rule_id: finding.rule_id,
                        severity: finding.severity,
                        confidence: finding.confidence,
                        title: finding.title,
                        description: finding.description,
                        evidence_count: finding.evidence.len(),
                        evidence: finding
                            .evidence
                            .into_iter()
                            .map(|item| SemanticEvidenceEntry {
                                file: item.file,
                                line: item.line,
                                role: item.role,
                                snippet: item.snippet,
                            })
                            .collect(),
                    })
                    .collect(),
            }
        },
        consistency: {
            // report.json is not on disk yet (it is being built here), but
            // MERGE_GATE.json and INLINE_FINDINGS.sarif already are. Compare the
            // serialized gate/SARIF counters against the in-memory values this
            // report is about to write — a mismatch means MERGE_GATE diverged
            // from the report (the b1697d4 class). Pairing a value with itself,
            // as before, could never catch a mismatch.
            let disk = read_disk_artifact_counters(input.dir);
            let consistency = ArtifactCounters {
                files_changed_diff: Some(all_files.len()),
                files_changed_report: None,
                findings_count_sarif: disk.findings_count_sarif,
                findings_count_gate: disk.findings_count_gate,
                findings_count_report: Some(ctx.findings.len()),
                breaking_count_signal: None,
                breaking_count_report: None,
                skipped_checks_gate: None,
                skipped_checks_report: None,
                commit_count_diff: None,
                commit_count_report: None,
                coverage_pct_signal: None,
                coverage_pct_report: None,
                verdict_gate: disk.verdict_gate,
                verdict_report: Some(ctx.verdict.to_string()),
            }
            .check_consistency();

            ConsistencySection {
                consistent: consistency.consistent,
                checked_fields: consistency.checked_fields,
                warnings: consistency
                    .warnings
                    .into_iter()
                    .map(|warning| ConsistencyWarningEntry {
                        field: warning.field,
                        message: warning.message,
                        sources: warning
                            .sources
                            .into_iter()
                            .map(|source| ConsistencySourceEntry {
                                artifact: source.artifact,
                                value: source.value,
                            })
                            .collect(),
                    })
                    .collect(),
            }
        },
    };

    // -- artifacts --
    let artifacts = Artifacts {
        zip_path: "artifacts.zip",
        manifest_path: "00_summary/MANIFEST.json",
    };

    // -- ui_hints --
    let ui_hints = Some(UiHints {
        default_hide_globs: vec![
            "**/Cargo.lock",
            "**/package-lock.json",
            "**/yarn.lock",
            "**/pnpm-lock.yaml",
        ],
        default_sort: "CHURN_DESC",
    });

    let checks_skipped: Vec<SkippedCheckEntry> = ctx
        .skipped_checks
        .iter()
        .map(|s| SkippedCheckEntry {
            id: s.id.clone(),
            name: s.name.clone(),
            reason: s.reason.clone(),
        })
        .collect();

    let ownership: Vec<OwnershipMapEntry> = ctx
        .ownership_map
        .iter()
        .map(|(path, owner)| OwnershipMapEntry {
            path: path.clone(),
            owner: owner.clone(),
        })
        .collect();

    Report {
        schema_version: "1.0",
        meta,
        gate,
        checks: check_entries,
        checks_skipped,
        diff: diff_section,
        quality,
        ownership,
        regression: input.regression.cloned(),
        artifacts,
        ui_hints,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::artifacts::signal::{BreakingFinding, BreakingKind, BreakingRisk};

    fn file_entry(path: &str, additions: usize, deletions: usize) -> FileEntry {
        FileEntry {
            path: path.to_string(),
            status: "M",
            additions,
            deletions,
            churn: additions + deletions,
            is_hotspot: additions + deletions >= HOTSPOT_THRESHOLD,
            patch_path: None,
        }
    }

    #[test]
    fn aggregates_diff_stats_per_directory_and_sorts_by_churn() {
        let entries = vec![
            file_entry("src/services/audio/chunker.rs", 10, 3),
            file_entry("src/services/audio/pipeline.rs", 4, 2),
            file_entry("src/ui/mod.rs", 6, 1),
            file_entry("Cargo.toml", 2, 0),
        ];

        let directories = aggregate_directory_entries(&entries);

        assert_eq!(
            directories,
            vec![
                DirEntry {
                    path: "src/services/audio".to_string(),
                    files_changed: 2,
                    insertions: 14,
                    deletions: 5,
                    churn: 19,
                },
                DirEntry {
                    path: "src/ui".to_string(),
                    files_changed: 1,
                    insertions: 6,
                    deletions: 1,
                    churn: 7,
                },
                DirEntry {
                    path: ".".to_string(),
                    files_changed: 1,
                    insertions: 2,
                    deletions: 0,
                    churn: 2,
                },
            ]
        );
    }

    #[test]
    fn annotates_generated_path_findings_without_filtering_real_ones() {
        let output = "\
src/lib.rs:10:5 warning real finding\n\
target/debug/build/foo.rs:1:1 warning generated\n\
dist/app.js:3:1 error bundled\n\
Compiling prview v0.1.0\n";

        let stats = finding_stats_from_output(output).expect("expected finding stats");

        assert_eq!(
            stats,
            FindingStats {
                total_findings: 3,
                real_findings: 1,
                generated_path_findings: 2,
                generated_paths_matched: vec!["dist/".to_string(), "target/".to_string()],
            }
        );
    }

    #[test]
    fn semgrep_stats_count_results_array_not_json_noise() {
        // results:[] with PartialParsing errors must report ZERO findings, not
        // phantom counts scraped from `line`/`col` offsets in the error spans.
        let output = r#"{"version":"1.135.0","results":[],"errors":[{"code":3,"level":"warn","type":["PartialParsing",[{"path":"app/x.rs","start":{"line":394,"col":19,"offset":0}}]],"message":"Syntax error"}]}
"#;
        assert_eq!(semgrep_finding_stats_from_output(output), None);
        // The generic heuristic over-counts the same blob, proving the bug the
        // dedicated parser fixes.
        assert!(finding_stats_from_output(output).is_some());
    }

    #[test]
    fn semgrep_stats_count_real_results() {
        let output = r#"{"results":[{"check_id":"r1","path":"src/a.rs"},{"check_id":"r2","path":"target/gen.rs"}],"errors":[]}"#;
        let stats = semgrep_finding_stats_from_output(output).expect("expected stats");
        assert_eq!(stats.total_findings, 2);
        assert_eq!(stats.real_findings, 1);
        assert_eq!(stats.generated_path_findings, 1);
    }

    #[test]
    fn semgrep_stats_survive_trailing_stderr_after_json() {
        // PR #12 review #19: stdout JSON followed by a stderr warning (the two
        // streams are concatenated before we see them) must not drop findings.
        // A plain from_str over the whole blob would reject the trailing line
        // and silently report zero.
        let output = "{\"results\":[{\"check_id\":\"r1\",\"path\":\"src/a.rs\"}],\"errors\":[]}\n\
A new version of Semgrep is available. See https://semgrep.dev for details.\n";
        let stats = semgrep_finding_stats_from_output(output).expect("expected stats");
        assert_eq!(stats.total_findings, 1);
        assert_eq!(stats.real_findings, 1);
    }

    #[test]
    fn skips_annotation_when_output_has_no_path_like_findings() {
        let output = "\
Running cargo check\n\
Finished dev [unoptimized + debuginfo] target(s) in 0.34s\n";

        assert_eq!(finding_stats_from_output(output), None);
    }

    #[test]
    fn ignores_path_lines_without_finding_signal() {
        let output = "\
cwd: /Users/dev/Git/myapp\n\
log file: /tmp/eslint.log\n";

        assert_eq!(finding_stats_from_output(output), None);
    }

    #[test]
    fn report_includes_failed_tests_for_cargo_test_entries() {
        use crate::artifacts::signal::CoverageDelta;
        use crate::artifacts::{CheckGateEntry, DashboardContext};
        use crate::checks::CheckResult;
        use crate::cli::ExecutionMode;
        use crate::config::test_config;
        use crate::git::ResolvedRef;
        use std::time::Duration;

        let mut config = test_config();
        config.execution_mode = ExecutionMode::Standard;

        let checks = vec![CheckResult {
            name: "Cargo test".to_string(),
            status: CheckStatus::Failed,
            duration: Duration::from_secs(1),
            output: "\
thread 'tests::bad' panicked at src/lib.rs:42:5:
assertion failed
test tests::bad ... FAILED

failures:

---- tests::bad stdout ----

failures:
    tests::bad

test result: FAILED. 0 passed; 1 failed
"
            .to_string(),
            cached: false,
            provenance: None,
        }];

        let ctx = DashboardContext {
            verdict: "BLOCK",
            analysis_status: crate::policy::engine::AnalysisStatus::Complete,
            merge_recommendation: crate::policy::engine::MergeRecommendation::Block,
            allow_merge: false,
            quality_pass: false,
            policy_allow_merge: true,
            recommended_merge: false,
            review_caveats: vec![],
            quality_failures: vec!["Cargo test".to_string()],
            introduced_quality_failures: vec![],
            preexisting_quality_failures: vec![],
            mixed_quality_failures: vec![],
            unclassified_quality_failures: vec![],
            quality_failure_details: vec![],
            policy_mode: "warn",
            blocking_issues: vec![],
            check_gates: vec![CheckGateEntry {
                name: "Cargo test".to_string(),
                id: "cargo_test".to_string(),
                blocking: false,
                class: "FAIL",
                severity: "warn",
            }],
            breaking: vec![],
            coverage: CoverageDelta {
                total_source: 0,
                covered_count: 0,
                pct: 0,
                uncovered: vec![],
                covered: vec![],
                non_code_count: 0,
                ghost_tests: vec![],
            },
            findings: vec![],
            per_file_diff_files: vec![],
            skipped_checks: vec![],
            previous_run: None,
            run_history: vec![],
            flaky_scores: vec![],
            lint_metrics: vec![],
            ownership_map: vec![],
            risk_scores: vec![],
            i18n_delta: None,
        };
        let target = ResolvedRef {
            name: "feature/report".to_string(),
            commit_id: "deadbeef".to_string(),
            is_remote: false,
        };
        let base = ResolvedRef {
            name: "main".to_string(),
            commit_id: "cafebabe".to_string(),
            is_remote: false,
        };
        let bases = vec![base];

        let tmp = tempfile::tempdir().expect("tempdir");
        let input = ReportInput {
            dir: tmp.path(),
            config: &config,
            diffs: &[],
            checks: &checks,
            resolved_target: &target,
            resolved_bases: &bases,
            ctx: &ctx,
            run_started_at: "2026-03-09T00:00:00Z",
            heuristics: None,
            regression: None,
        };

        let report = build_report(&input);
        let json = serde_json::to_value(&report).expect("serialize report");
        let entry = json["checks"]
            .as_array()
            .and_then(|checks| checks.first())
            .expect("first check");

        assert_eq!(
            entry["failed_tests"].as_array(),
            Some(&vec![serde_json::Value::String("tests::bad".to_string())])
        );
    }

    #[test]
    fn report_gate_serializes_preexisting_quality_failures() {
        use crate::artifacts::signal::CoverageDelta;
        use crate::artifacts::{
            CheckGateEntry, DashboardContext, QualityFailureClass, QualityFailureDetail,
        };
        use crate::cli::ExecutionMode;
        use crate::config::test_config;
        use crate::git::ResolvedRef;

        let mut config = test_config();
        config.execution_mode = ExecutionMode::Standard;

        // Pre-existing-only failures: quality_pass=true because no new issues
        let ctx = DashboardContext {
            verdict: "PASS",
            analysis_status: crate::policy::engine::AnalysisStatus::Complete,
            merge_recommendation: crate::policy::engine::MergeRecommendation::Approve,
            allow_merge: true,
            quality_pass: true,
            policy_allow_merge: true,
            recommended_merge: true,
            review_caveats: vec![
                "Pre-existing quality failures (not from this diff): ESLint".to_string(),
            ],
            quality_failures: vec!["ESLint".to_string()],
            introduced_quality_failures: vec![],
            preexisting_quality_failures: vec!["ESLint".to_string()],
            mixed_quality_failures: vec![],
            unclassified_quality_failures: vec![],
            quality_failure_details: vec![QualityFailureDetail {
                name: "ESLint".to_string(),
                classification: QualityFailureClass::Preexisting,
            }],
            policy_mode: "warn",
            blocking_issues: vec![],
            check_gates: vec![CheckGateEntry {
                name: "ESLint".to_string(),
                id: "eslint".to_string(),
                blocking: false,
                class: "FAIL",
                severity: "warn",
            }],
            breaking: vec![],
            coverage: CoverageDelta {
                total_source: 0,
                covered_count: 0,
                pct: 0,
                uncovered: vec![],
                covered: vec![],
                non_code_count: 0,
                ghost_tests: vec![],
            },
            findings: vec![],
            per_file_diff_files: vec![],
            skipped_checks: vec![],
            previous_run: None,
            run_history: vec![],
            flaky_scores: vec![],
            lint_metrics: vec![],
            ownership_map: vec![],
            risk_scores: vec![],
            i18n_delta: None,
        };
        let target = ResolvedRef {
            name: "feature/report".to_string(),
            commit_id: "deadbeef".to_string(),
            is_remote: false,
        };
        let base = ResolvedRef {
            name: "main".to_string(),
            commit_id: "cafebabe".to_string(),
            is_remote: false,
        };
        let bases = vec![base];

        let tmp = tempfile::tempdir().expect("tempdir");
        let input = ReportInput {
            dir: tmp.path(),
            config: &config,
            diffs: &[],
            checks: &[],
            resolved_target: &target,
            resolved_bases: &bases,
            ctx: &ctx,
            run_started_at: "2026-03-09T00:00:00Z",
            heuristics: None,
            regression: None,
        };

        let report = build_report(&input);
        let json = serde_json::to_value(&report).expect("serialize report");

        assert_eq!(
            json["gate"]["preexisting_quality_failures"],
            serde_json::json!(["ESLint"])
        );
        assert_eq!(
            json["gate"]["quality_failure_details"][0]["classification"].as_str(),
            Some("pre-existing")
        );
        // quality_pass is true (pre-existing failures don't block)
        assert_eq!(json["gate"]["quality_pass"].as_bool(), Some(true));
        // Summary mentions pre-existing classification
        assert!(
            json["gate"]["summary"]
                .as_str()
                .is_some_and(|summary| summary.contains("1 pre-existing"))
        );
    }

    #[test]
    fn report_tracks_signature_changes_in_breaking_summary_and_counts() {
        use crate::artifacts::signal::CoverageDelta;
        use crate::artifacts::{CheckGateEntry, DashboardContext};
        use crate::cli::ExecutionMode;
        use crate::config::test_config;
        use crate::git::ResolvedRef;

        let mut config = test_config();
        config.execution_mode = ExecutionMode::Standard;

        let ctx = DashboardContext {
            verdict: "BLOCK",
            analysis_status: crate::policy::engine::AnalysisStatus::Complete,
            merge_recommendation: crate::policy::engine::MergeRecommendation::Block,
            allow_merge: false,
            quality_pass: false,
            policy_allow_merge: true,
            recommended_merge: false,
            review_caveats: vec![],
            quality_failures: vec![],
            introduced_quality_failures: vec![],
            preexisting_quality_failures: vec![],
            mixed_quality_failures: vec![],
            unclassified_quality_failures: vec![],
            quality_failure_details: vec![],
            policy_mode: "warn",
            blocking_issues: vec![],
            check_gates: vec![CheckGateEntry {
                name: "cargo test".to_string(),
                id: "cargo_test".to_string(),
                blocking: false,
                class: "PASS",
                severity: "warn",
            }],
            breaking: vec![
                BreakingFinding {
                    file: "src/lib.rs".to_string(),
                    kind: BreakingKind::RemovedSymbol {
                        symbol_type: "function".to_string(),
                    },
                    line: "- pub fn old_api()".to_string(),
                    risk_level: BreakingRisk::High,
                },
                BreakingFinding {
                    file: "src/lib.rs".to_string(),
                    kind: BreakingKind::ChangedSignature {
                        before: "fn parse(input: &str)".to_string(),
                        after: "fn parse(input: &str, strict: bool)".to_string(),
                    },
                    line: "- fn parse(input: &str)".to_string(),
                    risk_level: BreakingRisk::High,
                },
                BreakingFinding {
                    file: ".env.example".to_string(),
                    kind: BreakingKind::NewEnvRequirement {
                        variable: "NEW_TOKEN".to_string(),
                    },
                    line: "+ NEW_TOKEN=".to_string(),
                    risk_level: BreakingRisk::Medium,
                },
            ],
            coverage: CoverageDelta {
                total_source: 0,
                covered_count: 0,
                pct: 0,
                uncovered: vec![],
                covered: vec![],
                non_code_count: 0,
                ghost_tests: vec![],
            },
            findings: vec![],
            per_file_diff_files: vec![],
            skipped_checks: vec![],
            previous_run: None,
            run_history: vec![],
            flaky_scores: vec![],
            lint_metrics: vec![],
            ownership_map: vec![],
            risk_scores: vec![],
            i18n_delta: None,
        };
        let target = ResolvedRef {
            name: "feature/report".to_string(),
            commit_id: "deadbeef".to_string(),
            is_remote: false,
        };
        let base = ResolvedRef {
            name: "main".to_string(),
            commit_id: "cafebabe".to_string(),
            is_remote: false,
        };
        let bases = vec![base];
        let tmp = tempfile::tempdir().expect("tempdir");
        let input = ReportInput {
            dir: tmp.path(),
            config: &config,
            diffs: &[],
            checks: &[],
            resolved_target: &target,
            resolved_bases: &bases,
            ctx: &ctx,
            run_started_at: "2026-03-12T00:00:00Z",
            heuristics: None,
            regression: None,
        };

        let report = build_report(&input);
        let json = serde_json::to_value(&report).expect("serialize report");
        let breaking = &json["quality"]["breaking_changes"];

        assert_eq!(breaking["removed_public_symbols_count"].as_u64(), Some(1));
        assert_eq!(breaking["signature_changes_count"].as_u64(), Some(1));
        assert_eq!(breaking["new_env_vars_count"].as_u64(), Some(1));
        assert_eq!(
            breaking["summary"].as_str(),
            Some("1 removed public symbol, 1 signature change, 1 new env requirement")
        );
    }

    #[test]
    fn report_uses_unused_symbols_key_for_heuristics_summary() {
        use crate::artifacts::{CheckGateEntry, DashboardContext};
        use crate::cli::ExecutionMode;
        use crate::config::test_config;
        use crate::git::ResolvedRef;
        use crate::heuristics::{
            DeadParrot, HeuristicsResult, HeuristicsSummary, LoctreeAnalysis, TwinsAnalysis,
        };

        let mut config = test_config();
        config.execution_mode = ExecutionMode::Standard;

        let ctx = DashboardContext {
            verdict: "PASS",
            analysis_status: crate::policy::engine::AnalysisStatus::Complete,
            merge_recommendation: crate::policy::engine::MergeRecommendation::Approve,
            allow_merge: true,
            quality_pass: true,
            policy_allow_merge: true,
            recommended_merge: true,
            review_caveats: vec![],
            quality_failures: vec![],
            introduced_quality_failures: vec![],
            preexisting_quality_failures: vec![],
            mixed_quality_failures: vec![],
            unclassified_quality_failures: vec![],
            quality_failure_details: vec![],
            policy_mode: "warn",
            blocking_issues: vec![],
            check_gates: vec![CheckGateEntry {
                name: "cargo check".to_string(),
                id: "cargo_check".to_string(),
                blocking: false,
                class: "PASS",
                severity: "warn",
            }],
            breaking: vec![],
            coverage: crate::artifacts::signal::CoverageDelta {
                total_source: 0,
                covered_count: 0,
                pct: 0,
                uncovered: vec![],
                covered: vec![],
                non_code_count: 0,
                ghost_tests: vec![],
            },
            findings: vec![],
            per_file_diff_files: vec![],
            skipped_checks: vec![],
            previous_run: None,
            run_history: vec![],
            flaky_scores: vec![],
            lint_metrics: vec![],
            ownership_map: vec![],
            risk_scores: vec![],
            i18n_delta: None,
        };
        let heuristics = HeuristicsResult {
            loctree: Some(LoctreeAnalysis {
                available: true,
                twins: TwinsAnalysis {
                    dead_parrots: vec![
                        DeadParrot {
                            file: "src/lib.rs".to_string(),
                            symbol: "unused_one".to_string(),
                            kind: "function".to_string(),
                            line: 10,
                        },
                        DeadParrot {
                            file: "src/lib.rs".to_string(),
                            symbol: "unused_two".to_string(),
                            kind: "function".to_string(),
                            line: 20,
                        },
                        DeadParrot {
                            file: "src/lib.rs".to_string(),
                            symbol: "unused_three".to_string(),
                            kind: "function".to_string(),
                            line: 30,
                        },
                    ],
                    exact_twins: vec![],
                    total_symbols: 3,
                },
                ..Default::default()
            }),
            summary: HeuristicsSummary {
                dead_exports: 2,
                circular_imports: 1,
                dead_parrots: 3,
                exact_twins: 4,
                total_files: 10,
                total_loc: 100,
            },
            ..Default::default()
        };
        let target = ResolvedRef {
            name: "feature/report".to_string(),
            commit_id: "deadbeef".to_string(),
            is_remote: false,
        };
        let base = ResolvedRef {
            name: "main".to_string(),
            commit_id: "cafebabe".to_string(),
            is_remote: false,
        };
        let bases = vec![base];
        let tmp = tempfile::tempdir().expect("tempdir");
        let input = ReportInput {
            dir: tmp.path(),
            config: &config,
            diffs: &[],
            checks: &[],
            resolved_target: &target,
            resolved_bases: &bases,
            ctx: &ctx,
            run_started_at: "2026-03-12T00:00:00Z",
            heuristics: Some(&heuristics),
            regression: None,
        };

        let report = build_report(&input);
        let json = serde_json::to_value(&report).expect("serialize report");
        let heuristics_json = &json["quality"]["heuristics"];

        assert_eq!(heuristics_json["unused_symbols"].as_u64(), Some(3));
        assert!(heuristics_json.get("dead_parrots").is_none());
    }

    #[test]
    fn report_includes_cargo_audit_informational_caveat_when_context_has_none() {
        use crate::artifacts::{CheckGateEntry, DashboardContext};
        use crate::cli::ExecutionMode;
        use crate::config::test_config;
        use crate::git::ResolvedRef;
        use std::time::Duration;

        let mut config = test_config();
        config.execution_mode = ExecutionMode::Standard;

        let checks = vec![CheckResult {
            name: "Cargo audit".to_string(),
            status: CheckStatus::Passed,
            duration: Duration::from_secs(1),
            output: r#"{"vulnerabilities":{"found":false,"count":0,"list":[]},"warnings":{"unmaintained":[{"kind":"unmaintained","package":{"name":"paste","version":"1.0.15"},"advisory":{"id":"RUSTSEC-2024-0436"}}]}}"#.to_string(),
            cached: false,
            provenance: None,
        }];

        let ctx = DashboardContext {
            verdict: "PASS",
            analysis_status: crate::policy::engine::AnalysisStatus::Complete,
            merge_recommendation: crate::policy::engine::MergeRecommendation::Approve,
            allow_merge: true,
            quality_pass: true,
            policy_allow_merge: true,
            recommended_merge: true,
            review_caveats: vec![],
            quality_failures: vec![],
            introduced_quality_failures: vec![],
            preexisting_quality_failures: vec![],
            mixed_quality_failures: vec![],
            unclassified_quality_failures: vec![],
            quality_failure_details: vec![],
            policy_mode: "warn",
            blocking_issues: vec![],
            check_gates: vec![CheckGateEntry {
                name: "cargo audit".to_string(),
                id: "cargo_audit".to_string(),
                blocking: false,
                class: "PASS",
                severity: "warn",
            }],
            breaking: vec![],
            coverage: crate::artifacts::signal::CoverageDelta {
                total_source: 0,
                covered_count: 0,
                pct: 0,
                uncovered: vec![],
                covered: vec![],
                non_code_count: 0,
                ghost_tests: vec![],
            },
            findings: vec![],
            per_file_diff_files: vec![],
            skipped_checks: vec![],
            previous_run: None,
            run_history: vec![],
            flaky_scores: vec![],
            lint_metrics: vec![],
            ownership_map: vec![],
            risk_scores: vec![],
            i18n_delta: None,
        };
        let target = ResolvedRef {
            name: "feature/report".to_string(),
            commit_id: "deadbeef".to_string(),
            is_remote: false,
        };
        let base = ResolvedRef {
            name: "main".to_string(),
            commit_id: "cafebabe".to_string(),
            is_remote: false,
        };
        let bases = vec![base];
        let tmp = tempfile::tempdir().expect("tempdir");
        let input = ReportInput {
            dir: tmp.path(),
            config: &config,
            diffs: &[],
            checks: &checks,
            resolved_target: &target,
            resolved_bases: &bases,
            ctx: &ctx,
            run_started_at: "2026-03-12T00:00:00Z",
            heuristics: None,
            regression: None,
        };

        let report = build_report(&input);
        let json = serde_json::to_value(&report).expect("serialize report");
        let caveats = json["gate"]["review_caveats"]
            .as_array()
            .expect("review caveats");

        assert!(caveats.iter().any(|value| {
            value
                .as_str()
                .is_some_and(|text| text.contains("Cargo audit note: 1 informational advisory"))
        }));
        assert!(caveats.iter().any(|value| {
            value
                .as_str()
                .is_some_and(|text| text.contains("paste (unmaintained)"))
        }));
    }
}
