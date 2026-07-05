//! Artifact generation
//!
//! Creates PR review artifacts: patches, reports, dashboards, ZIPs.

mod dashboard;
pub mod parsers;
pub(crate) mod report;
pub(crate) mod signal;

mod context_artifacts;
mod findings;
mod git_artifacts;
mod merge_gate;
mod sanity;

use context_artifacts::*;
use findings::*;
use git_artifacts::*;
use merge_gate::*;
use sanity::*;

mod ai_index;
mod audit;
mod brand;
mod context;
mod history;
mod lint;
mod ownership;
mod pr_review;
mod review_html;
mod root_cause;
mod verdict;

#[cfg(test)]
mod tests;

pub(crate) use ai_index::*;
pub(crate) use audit::*;
pub(crate) use context::*;
pub(crate) use history::*;
pub(crate) use lint::*;
pub(crate) use ownership::*;
pub(crate) use pr_review::*;
pub(crate) use review_html::*;
pub(crate) use root_cause::*;
pub(crate) use verdict::*;

use crate::checks::{CheckResult, CheckStatus};
use crate::config::Config;
use crate::git::git_cmd;
use crate::git::{Diff, Repository, ResolvedRef};
use crate::heuristics::HeuristicsResult;
use crate::paths::{read_dir_within, read_to_string_within, read_within};
use crate::policy::{GateClass, PolicySeverity};
use crate::regression;
use crate::regression::tests::is_test_file;
use anyhow::Result;
use signal::{
    BreakingFinding, BreakingKind, CoverageDelta, ReviewFileCategory, classify_review_file,
};
use std::collections::HashMap;
use std::fmt::Write as FmtWrite;
use std::fs;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

/// Timeout for context generators (30 seconds)
const CONTEXT_GEN_TIMEOUT_SECS: u64 = 30;

/// Maximum commits for per-commit diffs (avoid huge PRs)
const MAX_COMMITS_FOR_PER_COMMIT_DIFFS: usize = 50;

/// Maximum size for tsc-trace.log before truncation
const MAX_TSC_TRACE_BYTES: usize = 500_000;

/// Maximum combined patch text size used for in-memory regex analysis (2 MB).
/// The patch file on disk is never truncated — only the in-memory string passed to
/// breaking-changes / regression scanning.
const MAX_PATCH_TEXT_BYTES: usize = 2_097_152;

/// Batch size for grouping commits when PR has more than BATCH_THRESHOLD commits
const COMMIT_BATCH_SIZE: usize = 5;

/// Threshold above which per-commit diffs are batched instead of individual files
const COMMIT_BATCH_THRESHOLD: usize = 10;

/// Brand favicon as a self-contained `<link>` tag with a base64 data URI
/// (32x32 PNG). Embedded so generated HTML reports stay self-contained with
/// zero external requests.
pub(crate) const BRAND_FAVICON_LINK_TAG: &str = concat!(
    r#"<link rel="icon" type="image/png" sizes="32x32" href=""#,
    "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAACAAAAAgCAYAAABzenr0AAABmUlEQVR4AeyUsUtCURTGP03UBouE1KHIhOpPEGxoa5BIcmgLLA36GxQbIsOtTWhIHEJrkRYjhJYgTQcjpwpUkEQMAycR3kvz3ggUHnrNxOU+uPfc8z3uOb/33ceVazSa1jiHHGN+OAB3gDvAHeAOMDmQSNwjm80glXpAuVxEofAGh2MHCsUEisUcyPtc7gWh0PnAFzsTgFY7A51uFtPTU0gmH6FWq+H3n0CvN0CpVMJkWkS1+ol0Oj0agN+qFssa7PZtRCJXVLLZNmgslUowm1cRCJzRfJCJyQFSsNVqoV6vkyVqtRqNKpWKxkrlg8a/TMwAMpkM8fgNfL4juFx7tFcsdktj57TuFKAzNjulnmtmAFEUYTQuwOncpeceDIaQz+e6is+tNLF/2oD1QOjSeyXMAIIgtH+2ZVitm22QJbjdXojiFwyGeaqRJu+vchxvTeLC+3M0ROs3mAFIIdIwk3lCo9EgqeR4vlNAZDcATADkaz2eQ8mGw4pMANHoNcLhy2F7Se5nApDc+U8iB+AOcAe4AyN3oN999Q0AAP//qztb4AAAAAZJREFUAwC/LZQBocyfmQAAAABJRU5ErkJggg==",
    r#"">"#,
);

#[derive(Debug, Clone)]
struct StageTiming {
    label: String,
    duration_secs: f32,
}

#[derive(Debug, Clone)]
struct ContextArtifactDecision {
    key: &'static str,
    path: &'static str,
    generated: bool,
    recommended: bool,
    reason: String,
}

#[derive(Debug, Clone)]
struct ContextCommandTiming {
    label: String,
    artifact: Option<String>,
    status: &'static str,
    duration_secs: f32,
}

pub struct GenerateInput<'a> {
    pub config: &'a Config,
    pub diffs: &'a [Diff],
    pub checks: &'a [CheckResult],
    pub heuristics: Option<&'a HeuristicsResult>,
    pub resolved_target: &'a ResolvedRef,
    pub resolved_bases: &'a [ResolvedRef],
    pub run_start: Instant,
    pub skipped_checks: Vec<crate::checks::SkippedCheck>,
}

struct RunJsonInput<'a> {
    dir: &'a Path,
    artifacts_root: &'a Path,
    config: &'a Config,
    checks: &'a [CheckResult],
    heuristics: Option<&'a HeuristicsResult>,
    resolved_target: &'a ResolvedRef,
    resolved_bases: &'a [ResolvedRef],
    run_started_at: &'a str,
    total_duration_secs: f32,
    stage_timings: &'a [StageTiming],
    context_artifacts: &'a [ContextArtifactDecision],
    context_command_timings: &'a [ContextCommandTiming],
    regression: Option<&'a regression::RegressionReport>,
}

struct MergeGateInput<'a> {
    dir: &'a Path,
    config: &'a Config,
    checks: &'a [CheckResult],
    heuristics: Option<&'a HeuristicsResult>,
    inline: &'a InlineFindingsSummary,
    breaking: &'a [BreakingFinding],
    coverage: &'a CoverageDelta,
    diffs: &'a [Diff],
    skipped_checks: &'a [crate::checks::SkippedCheck],
    resolved_target: &'a ResolvedRef,
    resolved_bases: &'a [ResolvedRef],
    /// Per-check clean-comparison signal gating the pre-existing downgrade
    /// (R2-9/R3-16). Computed once per run for verdict parity with the dashboard
    /// context.
    clean_comparison: CleanComparison,
}

pub(crate) struct DashboardContextInput<'a> {
    config: &'a Config,
    checks: &'a [CheckResult],
    heuristics: Option<&'a HeuristicsResult>,
    inline: &'a InlineFindingsSummary,
    breaking: Vec<BreakingFinding>,
    coverage: CoverageDelta,
    diff_dir: &'a Path,
    skipped_checks: Vec<crate::checks::SkippedCheck>,
    out_dir: &'a Path,
    diffs: &'a [Diff],
    ownership_map: Vec<(String, String)>,
    /// Mirrors `MergeGateInput::clean_comparison` — the same value feeds both so
    /// the two verdict surfaces cannot disagree on the pre-existing downgrade.
    clean_comparison: CleanComparison,
}

/// Build a synthetic CheckResult for heuristics_loctree so it appears in
/// report.json checks[] and dashboard checks table alongside real checks.
fn build_heuristics_check(heuristics: Option<&HeuristicsResult>) -> CheckResult {
    use crate::checks::CheckStatus;
    match heuristics {
        Some(heuristics)
            if heuristics
                .loctree
                .as_ref()
                .is_some_and(|loctree| loctree.available) =>
        {
            let loctree = heuristics.loctree.as_ref().expect("checked above");
            let dead = loctree.dead_exports.len();
            let cycles = loctree.cycles.len();
            let parrots = loctree.twins.dead_parrots.len();
            let twins = loctree.twins.exact_twins.len();
            let status = if heuristics.summary.total_files == 0 {
                CheckStatus::Skipped
            } else if dead > 0 || cycles > 0 {
                CheckStatus::Warnings
            } else {
                CheckStatus::Passed
            };
            let output = format!(
                "dead_exports={}, cycles={}, unused_symbols={}, exact_twins={}, total_files={}, total_loc={}",
                dead, cycles, parrots, twins, loctree.stats.total_files, loctree.stats.total_loc
            );
            CheckResult {
                name: "heuristics_loctree".to_string(),
                status,
                duration: std::time::Duration::ZERO,
                output,
                cached: false,
                provenance: None,
            }
        }
        _ => CheckResult {
            name: "heuristics_loctree".to_string(),
            status: crate::checks::CheckStatus::Skipped,
            duration: std::time::Duration::ZERO,
            output: "Loctree heuristics not available".to_string(),
            cached: false,
            provenance: None,
        },
    }
}

/// Generate all artifacts
pub fn generate(input: GenerateInput<'_>) -> Result<PathBuf> {
    let GenerateInput {
        config,
        diffs,
        checks,
        heuristics,
        resolved_target,
        resolved_bases,
        run_start,
        skipped_checks,
    } = input;
    let t_total = Instant::now();
    let mut stage_timings = Vec::new();
    // Compute wall-clock start time by subtracting elapsed since App::new()
    let run_started_at = (chrono::Local::now()
        - chrono::Duration::from_std(run_start.elapsed()).unwrap_or_default())
    .to_rfc3339();

    // Build extended checks list including synthetic heuristics check
    let heuristics_check = build_heuristics_check(heuristics);
    let mut all_checks: Vec<CheckResult> = checks.to_vec();
    all_checks.push(heuristics_check);
    let context_artifacts = plan_context_artifacts(config, diffs, &all_checks);

    let emit_human_stdout = !config.json && !config.quiet;
    let out_dir = config.allocate_artifacts_dir_for_commit(&resolved_target.commit_id)?;
    fs::create_dir_all(&out_dir)?;

    // Open repository once for all generators
    let repo = Repository::open(&config.repo_root)?;

    // Numbered directory layout
    let summary_dir = out_dir.join("00_summary");
    let diff_dir = out_dir.join("10_diff");
    let quality_dir = out_dir.join("20_quality");
    let context_dir = out_dir.join("30_context");
    let per_commit_dir = diff_dir.join("per-commit-diffs");

    fs::create_dir_all(&summary_dir)?;
    fs::create_dir_all(&diff_dir)?;
    fs::create_dir_all(&quality_dir)?;
    fs::create_dir_all(&context_dir)?;
    fs::create_dir_all(&per_commit_dir)?;

    // Artifact Pack version marker
    fs::write(summary_dir.join("ARTIFACT_VERSION.txt"), "1.0\n")?;

    if emit_human_stdout {
        use colored::Colorize;
        println!("\n{}", "Generating artifacts...".cyan());
    }

    // 10_diff/ — full patch (cache text for breaking_changes reuse and heuristical checkings)
    let t = Instant::now();
    let patch_texts = generate_full_patch(&diff_dir, &repo, diffs)?;
    stage_timings.push(finish_timing(emit_human_stdout, "full.patch", t));

    // Heuristics generation early for MERGE_GATE (bind them to the truth pipeline)
    if let Some(api) = signal::generate_public_api_diff(&quality_dir, &patch_texts)? {
        all_checks.push(api);
    }
    if let Some(uns) = signal::generate_unsafe_audit(&context_dir, diffs, &repo)? {
        all_checks.push(uns);
    }
    if let Some(ghr) = signal::generate_ghost_refs(&context_dir, diffs, &repo)? {
        all_checks.push(ghr);
    }

    // 00_summary/
    let t = Instant::now();
    generate_metadata(&summary_dir, config, diffs, &all_checks, resolved_target)?;
    generate_commit_list(&summary_dir, diffs)?;
    generate_file_status(&summary_dir, diffs)?;
    generate_system_meta(&summary_dir)?;
    generate_git_meta(&summary_dir, config, resolved_target, resolved_bases)?;
    stage_timings.push(finish_timing(emit_human_stdout, "00_summary", t));

    let t = Instant::now();
    generate_per_commit_diffs(&repo, &per_commit_dir, diffs, emit_human_stdout)?;
    stage_timings.push(finish_timing(emit_human_stdout, "per-commit-diffs", t));

    // 20_quality/ — per-gate result.json + .log, then aggregate logs
    let t = Instant::now();
    generate_gate_results(&quality_dir, &all_checks)?;
    generate_heuristics_gate_result(&quality_dir, heuristics)?;
    generate_checks_log(&quality_dir, &all_checks)?;
    signal::generate_checks_errors_log(&quality_dir, &all_checks)?;
    // Compute breaking changes once, reuse for file-write + dashboard
    let breaking_findings = signal::analyze_all_breaking_changes(&patch_texts);
    signal::write_breaking_changes(&quality_dir, &breaking_findings)?;
    // Compute coverage signal ONCE — all consumers (text, gate, dashboard, report)
    // use this same result to guarantee consistency.
    let coverage_signal =
        signal::compute_coverage_signal(diffs, Some(&config.repo_root), Some(&repo));
    let coverage_delta = signal::CoverageDelta::from_signal(&coverage_signal);
    signal::generate_coverage_delta(&quality_dir, &coverage_signal)?;
    stage_timings.push(finish_timing(emit_human_stdout, "20_quality", t));

    // 10_diff/ — per-file diffs for hotspots
    let t = Instant::now();
    signal::generate_per_file_diffs(&diff_dir, &repo, diffs)?;
    stage_timings.push(finish_timing(emit_human_stdout, "per-file-diffs", t));

    // Log signal generator status for human output
    if emit_human_stdout {
        use colored::Colorize;
        let has = |name: &str| quality_dir.join(name).exists() || diff_dir.join(name).exists();
        if !has("checks-errors.log") {
            println!(
                "  {} checks-errors.log: not generated (no errors/warnings)",
                "ℹ".blue()
            );
        }
        if !has("BREAKING_CHANGES.md") {
            println!(
                "  {} BREAKING_CHANGES.md: not generated (no breaking changes detected)",
                "ℹ".blue()
            );
        }
        if !diff_dir.join("per-file-diffs").exists() {
            println!(
                "  {} per-file-diffs: not generated (no changed files)",
                "ℹ".blue()
            );
        }
    }

    // 30_context/
    let t = Instant::now();
    generate_changed_tests(diffs, &context_dir)?;
    let deps_delta = signal::generate_deps_delta(&context_dir, diffs, &repo).ok();
    let inline_summary = generate_inline_findings(
        &context_dir,
        &all_checks,
        diffs,
        deps_delta.as_ref(),
        Some(&repo),
    )?;
    signal::generate_pattern_scan(&context_dir, diffs, &repo)?;

    let tauri_dir = if let Some(cargo_root) = &config.profile.cargo_root {
        if cargo_root.ends_with("src-tauri") {
            cargo_root.clone()
        } else {
            config.repo_root.join("src-tauri")
        }
    } else {
        config.repo_root.join("src-tauri")
    };
    if is_tauri_project(&config.repo_root)
        && tauri_dir.exists()
        && let Err(e) = signal::generate_tauri_commands(&context_dir, diffs, &repo, &tauri_dir)
    {
        eprintln!("Warning: failed scanning tauri commands: {}", e);
    }
    stage_timings.push(finish_timing(emit_human_stdout, "30_context", t));

    // 00_summary/ — merge gate + failures summary
    let t = Instant::now();
    // Whether out-of-diff findings may be trusted as pre-existing. Computed once
    // and shared by the merge gate and the dashboard context so both verdict
    // surfaces gate the pre-existing downgrade identically (R2-9).
    let clean_comparison = CleanComparison::resolve(config, resolved_target);

    generate_merge_gate(MergeGateInput {
        dir: &summary_dir,
        config,
        checks: &all_checks,
        heuristics,
        inline: &inline_summary,
        breaking: &breaking_findings,
        coverage: &coverage_delta,
        diffs,
        skipped_checks: &skipped_checks,
        resolved_target,
        resolved_bases,
        clean_comparison,
    })?;
    generate_failures_summary(&summary_dir, &all_checks)?;
    stage_timings.push(finish_timing(
        emit_human_stdout,
        "MERGE_GATE + FAILURES_SUMMARY",
        t,
    ));

    // Root-level content generators
    let t = Instant::now();
    generate_checks_status_json(&out_dir, config, &all_checks, heuristics)?;
    generate_pr_review(
        &out_dir,
        config,
        diffs,
        &all_checks,
        &coverage_delta,
        heuristics,
    )?;
    stage_timings.push(finish_timing(emit_human_stdout, "PR_REVIEW", t));

    // Load base coverage from previous run (if available)
    let prev_coverage = load_previous_coverage(&out_dir);

    // Build regression context and compute regression report
    let regression_report = {
        let all_files: Vec<_> = diffs.iter().flat_map(|d| &d.files).collect();
        let file_stats: Vec<(String, char, usize, usize)> = all_files
            .iter()
            .map(|f| {
                let status_char = match f.status {
                    crate::git::FileStatus::Added => 'A',
                    crate::git::FileStatus::Modified => 'M',
                    crate::git::FileStatus::Deleted => 'D',
                    crate::git::FileStatus::Renamed => 'R',
                    crate::git::FileStatus::Copied => 'C',
                };
                (f.path.clone(), status_char, f.additions, f.deletions)
            })
            .collect();

        let (base_cycles, base_dead_exports, base_unused_symbols) =
            if let Some(reg) = heuristics.and_then(|h| h.regression.as_ref()) {
                (
                    Some(reg.base_circular_imports),
                    Some(reg.base_dead_exports),
                    Some(reg.base_unused_symbols()),
                )
            } else {
                (None, None, None)
            };

        let (cycles, dead_exports, unused_symbols, top_cycles, top_unused) =
            if let Some(h) = heuristics {
                let loctree = h.loctree.as_ref();
                (
                    h.summary.circular_imports,
                    h.summary.dead_exports,
                    h.summary.dead_parrots,
                    loctree
                        .map(|l| {
                            l.cycles
                                .iter()
                                .take(20)
                                .map(|c| c.files.join(" -> "))
                                .collect()
                        })
                        .unwrap_or_default(),
                    loctree
                        .map(|l| {
                            l.twins
                                .dead_parrots
                                .iter()
                                .take(20)
                                .map(|p| format!("{}:{}", p.file, p.symbol))
                                .collect()
                        })
                        .unwrap_or_default(),
                )
            } else {
                (0, 0, 0, vec![], vec![])
            };

        let patch_text = build_regression_patch_text(&patch_texts);

        let reg_ctx = regression::RegressionContext {
            files_changed: all_files.len(),
            insertions: all_files.iter().map(|f| f.additions).sum(),
            deletions: all_files.iter().map(|f| f.deletions).sum(),
            file_stats,
            coverage_ratio: if coverage_delta.total_source > 0 {
                Some(coverage_delta.covered_count as f64 / coverage_delta.total_source as f64)
            } else {
                None
            },
            base_coverage_ratio: prev_coverage.0,
            untested_critical_files: coverage_delta
                .uncovered
                .iter()
                .map(|f| f.path.clone())
                .collect(),
            base_untested_critical_count: prev_coverage.1,
            cycles,
            dead_exports,
            unused_symbols,
            base_cycles,
            base_dead_exports,
            base_unused_symbols,
            top_cycles,
            top_unused,
            patch_text,
        };

        regression::compute_regression(&reg_ctx)
    };

    let file_paths: Vec<String> = diffs
        .iter()
        .flat_map(|d| d.files.iter().map(|f| f.path.clone()))
        .collect();
    let ownership_map = build_ownership_map(&config.repo_root, &file_paths);
    let dash_ctx = build_dashboard_context(DashboardContextInput {
        config,
        checks: &all_checks,
        heuristics,
        inline: &inline_summary,
        breaking: breaking_findings,
        coverage: coverage_delta.clone(),
        diff_dir: &diff_dir,
        skipped_checks,
        out_dir: &out_dir,
        diffs,
        ownership_map,
        clean_comparison,
    });

    // Root-level report.json (generated first so dashboard can embed it)
    let t = Instant::now();
    report::generate(&report::ReportInput {
        dir: &out_dir,
        config,
        diffs,
        checks: &all_checks,
        resolved_target,
        resolved_bases,
        ctx: &dash_ctx,
        run_started_at: &run_started_at,
        heuristics,
        regression: Some(&regression_report),
    })?;
    generate_consistency_check(&summary_dir, &out_dir, diffs)?;
    if config.create_dashboard {
        // Dashboard reads report.json for embedding
        dashboard::generate(
            &out_dir,
            config,
            diffs,
            &all_checks,
            heuristics,
            &dash_ctx,
            Some(&regression_report),
        )?;
        stage_timings.push(finish_timing(
            emit_human_stdout,
            "report.json + dashboard",
            t,
        ));
    } else {
        stage_timings.push(finish_timing(emit_human_stdout, "report.json", t));
    }

    // 30_context/ — profile-specific artifacts (with timeouts, skip duplicates)
    let t = Instant::now();
    let context_command_timings = generate_context_artifacts(
        config,
        &all_checks,
        &context_dir,
        emit_human_stdout,
        &context_artifacts,
    )?;
    stage_timings.push(finish_timing(emit_human_stdout, "context-tools", t));

    // REVIEW_SUMMARY.md + review.html + AI_INDEX.md — consolidated human
    // review, the always-present browser handoff, and the reading-order map
    // (all run after primary artifacts exist so existence checks are accurate).
    let t = Instant::now();
    generate_review_summary(&out_dir)?;
    generate_standard_review_html(&out_dir)?;
    generate_ai_index(&out_dir, config, diffs, &all_checks, &coverage_delta)?;
    generate_standard_review_html(&out_dir)?;
    stage_timings.push(finish_timing(
        emit_human_stdout,
        "REVIEW_SUMMARY + review.html + AI_INDEX",
        t,
    ));

    // 00_summary/RUN.json — after all generators complete for accurate timing
    let t = Instant::now();
    generate_run_json(RunJsonInput {
        dir: &summary_dir,
        artifacts_root: &out_dir,
        config,
        checks: &all_checks,
        heuristics,
        resolved_target,
        resolved_bases,
        run_started_at: &run_started_at,
        total_duration_secs: run_start.elapsed().as_secs_f32(),
        stage_timings: &stage_timings,
        context_artifacts: &context_artifacts,
        context_command_timings: &context_command_timings,
        regression: Some(&regression_report),
    })?;
    stage_timings.push(finish_timing(emit_human_stdout, "RUN.json", t));

    // 00_summary/MANIFEST.json — runs LAST (hashes all files)
    let t = Instant::now();
    generate_manifest(&out_dir)?;
    stage_timings.push(finish_timing(emit_human_stdout, "MANIFEST.json", t));

    // Sanity checks — verify pack integrity after manifest
    let t = Instant::now();
    let sanity = run_sanity_checks(&out_dir)?;
    stage_timings.push(finish_timing(emit_human_stdout, "SANITY checks", t));
    if emit_human_stdout {
        use colored::Colorize;
        if sanity.valid {
            println!(
                "  {} Sanity: {}/{} checks passed",
                "✓".green(),
                sanity.checks_passed,
                sanity.checks_run,
            );
        } else {
            println!(
                "  {} Sanity: INVALID ({}/{} passed)",
                "✗".red(),
                sanity.checks_passed,
                sanity.checks_run,
            );
            for f in &sanity.failures {
                println!("    - {}", f.yellow());
            }
        }
    }

    // Create ZIP LAST, after RUN.json + MANIFEST + SANITY have all been written
    // to disk. The shipped pack is a portable copy of the run, so it must carry
    // the source of truth (RUN.json), the integrity manifest (MANIFEST.json) and
    // the sanity verdict (SANITY.json). Zipping earlier produced a pack a
    // consumer could not validate on its own (P1: shipped pack was incomplete
    // by-design). create_zip self-verifies the archive contains this metadata.
    if config.create_zip {
        let t = Instant::now();
        create_zip(&out_dir, emit_human_stdout)?;
        stage_timings.push(finish_timing(emit_human_stdout, "artifacts.zip", t));
    }

    // Create `latest` symlink in parent directory
    create_latest_symlink(&out_dir)?;

    // Register run in index and prune old runs
    {
        use crate::storage;
        let checks_passed = all_checks
            .iter()
            .filter(|c| c.status == crate::checks::CheckStatus::Passed)
            .count();
        let checks_failed = all_checks
            .iter()
            .filter(|c| {
                matches!(
                    c.status,
                    crate::checks::CheckStatus::Failed | crate::checks::CheckStatus::Error
                )
            })
            .count();
        let merge_status = match dash_ctx.merge_recommendation {
            crate::policy::engine::MergeRecommendation::Approve => "ALLOW",
            crate::policy::engine::MergeRecommendation::ReviewRequired => "HOLD",
            crate::policy::engine::MergeRecommendation::Block => "BLOCK",
        };
        let entry = storage::RunEntry {
            id: out_dir
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("unknown")
                .to_string(),
            repo: config.repo_name(),
            branch: config.safe_target_name(),
            commit: crate::git::short_sha(&resolved_target.commit_id).to_string(),
            path: out_dir.clone(),
            created_at: run_started_at.clone(),
            quality_pass: dash_ctx.quality_pass,
            merge_status: merge_status.to_string(),
            policy_mode: dash_ctx.policy_mode.to_string(),
            checks_passed,
            checks_failed,
            files_changed: diffs
                .iter()
                .flat_map(|d| d.files.iter().map(|f| &f.path))
                .collect::<std::collections::HashSet<_>>()
                .len(),
            size_bytes: walkdir::WalkDir::new(&out_dir)
                .into_iter()
                .filter_map(|e| e.ok())
                .filter(|e| e.file_type().is_file())
                .filter_map(|e| e.metadata().ok())
                .map(|m| m.len())
                .sum(),
            has_dashboard: out_dir.join("dashboard.html").exists(),
        };
        if let Err(e) = storage::register_and_prune(&out_dir, entry, emit_human_stdout)
            && emit_human_stdout
        {
            use colored::Colorize;
            eprintln!("  {} Index: {}", "\u{26a0}".yellow(), e);
        }
    }

    if emit_human_stdout {
        use colored::Colorize;
        println!(
            "  {} Artifacts generated in {:.1}s",
            "✓".green(),
            t_total.elapsed().as_secs_f32()
        );
    }

    Ok(out_dir)
}

fn generate_consistency_check(summary_dir: &Path, out_dir: &Path, diffs: &[Diff]) -> Result<()> {
    let commit_count: usize = diffs.iter().map(|diff| diff.commits.len()).sum();

    // Cross-check the IN-MEMORY truth (diffs, ctx) against the ALREADY-SERIALIZED
    // artifacts on disk. MERGE_GATE.json, INLINE_FINDINGS.sarif and report.json
    // are all written before this stage, so their serialized counters are the
    // independent side — a mismatch means serialization/computation diverged (the
    // b1697d4 class). Comparing a value to itself, as this checker used to, could
    // never detect anything. A pair with a missing disk source stays unchecked.
    let disk = signal::read_disk_artifact_counters(out_dir);
    let counters = signal::ArtifactCounters {
        files_changed_diff: Some(diffs.iter().flat_map(|diff| &diff.files).count()),
        files_changed_report: disk.files_changed_report,
        findings_count_sarif: disk.findings_count_sarif,
        findings_count_gate: disk.findings_count_gate,
        findings_count_report: disk.findings_count_report,
        // No independent serialized source is wired for these yet; leave them
        // unset rather than compare a value to itself (the old tautology).
        breaking_count_signal: None,
        breaking_count_report: None,
        skipped_checks_gate: None,
        skipped_checks_report: None,
        commit_count_diff: Some(commit_count),
        commit_count_report: disk.commit_count_report,
        coverage_pct_signal: None,
        coverage_pct_report: None,
        verdict_gate: disk.verdict_gate,
        verdict_report: disk.verdict_report,
    };
    let report = counters.check_consistency();

    fs::write(
        summary_dir.join("CONSISTENCY_CHECK.json"),
        serde_json::to_string_pretty(&report)?,
    )?;

    let mut md = String::new();
    md.push_str("# Consistency Check\n\n");
    md.push_str(&format!(
        "- Checked fields: `{}`\n- Consistent: `{}`\n\n",
        report.checked_fields, report.consistent
    ));
    if report.warnings.is_empty() {
        md.push_str("No cross-artifact mismatches detected.\n");
    } else {
        md.push_str("Detected cross-artifact mismatches:\n");
        for warning in &report.warnings {
            md.push_str(&format!("- `{}`: {}\n", warning.field, warning.message));
        }
    }
    fs::write(summary_dir.join("CONSISTENCY_CHECK.md"), md)?;

    Ok(())
}

fn log_timing(emit: bool, label: &str, start: Instant) {
    if emit {
        use colored::Colorize;
        let elapsed = start.elapsed().as_secs_f32();
        if elapsed > 0.1 {
            println!("  {} {} ({:.1}s)", "·".dimmed(), label.dimmed(), elapsed);
        } else {
            println!("  {} {}", "·".dimmed(), label.dimmed());
        }
    }
}

fn finish_timing(emit: bool, label: &str, start: Instant) -> StageTiming {
    let elapsed = start.elapsed().as_secs_f32();
    log_timing(emit, label, start);
    StageTiming {
        label: label.to_string(),
        duration_secs: elapsed,
    }
}

fn generate_metadata(
    dir: &Path,
    config: &Config,
    diffs: &[Diff],
    checks: &[CheckResult],
    resolved_target: &ResolvedRef,
) -> Result<()> {
    let mut content = String::new();

    content.push_str(&format!(
        "PR_URL: {}\n\n",
        config.pr_url.as_deref().unwrap_or("n/a")
    ));

    if let Some(diff) = diffs.first() {
        content.push_str(&format!(
            "HEAD: {} ({})\n",
            diff.target, resolved_target.commit_id
        ));
        content.push_str(&format!("BASE: {}\n\n", diff.base));
    }

    content.push_str("CHECKS:\n");
    for check in checks {
        content.push_str(&format!("  - {}: {}\n", check.name, check.status.as_str()));
    }

    fs::write(dir.join("pr-metadata.txt"), content)?;
    Ok(())
}

fn generate_checks_log(dir: &Path, checks: &[CheckResult]) -> Result<()> {
    let mut content = String::new();

    for check in checks {
        content.push_str(&format!("=== {} ===\n", check.name));
        content.push_str(&format!("Status: {}\n", check.status.as_str()));
        content.push_str(&format!("Duration: {:.2}s\n", check.duration.as_secs_f32()));
        content.push_str(&format!("Cached: {}\n", check.cached));

        if let Some(ref prov) = check.provenance {
            content.push_str(&format!("Command: {}\n", prov.command));
            content.push_str(&format!("Exit code: {:?}\n", prov.exit_code));
            content.push_str(&format!("CWD: {}\n", prov.cwd));
            if !prov.hard_fail_signatures.is_empty() {
                content.push_str(&format!(
                    "Hard fail signatures: {}\n",
                    prov.hard_fail_signatures.join(", ")
                ));
            }
        }

        content.push('\n');
        content.push_str(&check.output);
        content.push_str("\n\n");
    }

    fs::write(dir.join("full-checks.log"), content)?;

    // Extract warnings-only log for build checks (cargo build, tsc, etc.)
    let mut warnings = String::new();
    for check in checks {
        let mut saw_warning_in_check = false;
        let check_warnings: Vec<&str> = check
            .output
            .lines()
            .filter(|line| {
                let t = line.trim();
                let is_warning = t.starts_with("warning")
                    || t.starts_with("Warning")
                    || t.starts_with("WARNING")
                    || t.contains("warning:")
                    || t.contains("Warning:");
                if is_warning {
                    saw_warning_in_check = true;
                }
                // Include location lines only after seeing a warning in THIS check
                is_warning || (t.starts_with("  --> ") && saw_warning_in_check)
            })
            .collect();

        if !check_warnings.is_empty() {
            let _ = writeln!(
                warnings,
                "=== {} ({} warnings) ===",
                check.name,
                check_warnings.len()
            );
            for w in &check_warnings {
                warnings.push_str(w);
                warnings.push('\n');
            }
            warnings.push('\n');
        }
    }
    if !warnings.is_empty() {
        fs::write(dir.join("warnings-only.log"), warnings)?;
    }

    Ok(())
}

/// Per-gate result.json + .log files (Artifact Pack v1)
fn generate_gate_results(dir: &Path, checks: &[CheckResult]) -> Result<()> {
    use serde_json::json;

    for check in checks {
        let gate_id = check_id_from_name(&check.name);

        // Skip synthetic heuristics check — handled by generate_heuristics_gate_result()
        // which writes a richer schema with dead_exports/circular_imports fields.
        if gate_id == "heuristics_loctree" {
            continue;
        }

        let class = gate_class_for_check(check.status);

        let mut result = json!({
            "gate": gate_id,
            "name": check.name,
            "status": check.status.as_str(),
            "class": gate_class_to_str(class),
            "duration_secs": check.duration.as_secs_f32(),
            "cached": check.cached,
        });

        if let Some(ref prov) = check.provenance {
            result["command"] = json!(prov.command);
            result["exit_code"] = json!(prov.exit_code);
            result["cwd"] = json!(prov.cwd);
            result["started_at"] = json!(prov.started_at);
            result["finished_at"] = json!(prov.finished_at);
            result["hard_fail_signatures"] = json!(prov.hard_fail_signatures);
            if let Some(ref ver) = prov.tool_version {
                result["tool_version"] = json!(ver);
            }
            if let Some(ref key) = prov.cache_key {
                result["cache_key"] = json!(key);
            }
        }

        if gate_id == "cargo_test"
            && matches!(check.status, CheckStatus::Failed | CheckStatus::Error)
        {
            let failed_tests = parsers::cargo_test::extract_failed_test_names(&check.output);
            if !failed_tests.is_empty() {
                result["failed_tests"] = json!(failed_tests);
                result["failed_test_count"] = json!(failed_tests.len());
            }
        }

        fs::write(
            dir.join(format!("{}.result.json", gate_id)),
            serde_json::to_string_pretty(&result)?,
        )?;

        fs::write(dir.join(format!("{}.log", gate_id)), &check.output)?;
    }

    Ok(())
}

/// Per-gate result for heuristics (not a regular Check, so handled separately)
fn generate_heuristics_gate_result(
    dir: &Path,
    heuristics: Option<&HeuristicsResult>,
) -> Result<()> {
    use serde_json::json;

    let (status, class, dead, cycles, unused_symbols) = if let Some(h) = heuristics {
        let dead = h.summary.dead_exports;
        let cycles = h.summary.circular_imports;
        let unused_symbols = h.summary.dead_parrots;
        if h.summary.total_files == 0 {
            // Consistent with MERGE_GATE and dashboard: zero-file scan = SKIP
            ("skipped", "SKIP", dead, cycles, unused_symbols)
        } else if dead > 0 || cycles > 0 {
            ("warnings", "INFO", dead, cycles, unused_symbols)
        } else {
            ("passed", "PASS", dead, cycles, unused_symbols)
        }
    } else {
        ("skipped", "SKIP", 0, 0, 0)
    };

    let mut result = json!({
        "gate": "heuristics_loctree",
        "name": "Loctree Heuristics",
        "status": status,
        "class": class,
        "duration_secs": 0.0,
        "dead_exports": dead,
        "circular_imports": cycles,
        "unused_symbols": unused_symbols,
        "cached": false,
    });

    if let Some(h) = heuristics {
        if let Some(ref root) = h.analysis_root {
            result["scanned_dir"] = json!(root);
        }
        if let Some(ref reg) = h.regression {
            result["regression"] = json!(reg);
        }
    }

    fs::write(
        dir.join("heuristics_loctree.result.json"),
        serde_json::to_string_pretty(&result)?,
    )?;

    // Log — write summary text
    let log = if let Some(h) = heuristics {
        let twins_count = h.loctree.as_ref().map_or(0, |l| l.twins.exact_twins.len());
        let mut text = format!(
            "Loctree Heuristics\nDead exports: {}\nUnused symbols: {}\nCircular imports: {}\nExact twins: {}\n",
            h.summary.dead_exports, h.summary.dead_parrots, h.summary.circular_imports, twins_count
        );
        if let Some(ref root) = h.analysis_root {
            text.push_str(&format!("Analysis root: {}\n", root));
        }
        if let Some(ref reg) = h.regression {
            text.push_str(&format!(
                "\nRegression (base {} → target {}):\n  Dead exports delta: {:+}\n  Cycles delta: {:+}\n  Unused symbols delta: {:+}\n  Regression: {} | Improvement: {}\n",
                &reg.base_sha[..7.min(reg.base_sha.len())],
                &reg.target_sha[..7.min(reg.target_sha.len())],
                reg.dead_exports_delta,
                reg.cycles_delta,
                reg.unused_symbols_delta(),
                if reg.regression_detected { "YES" } else { "no" },
                if reg.improvement_detected { "YES" } else { "no" },
            ));
        }
        text
    } else {
        "Heuristics not run.\n".to_string()
    };

    fs::write(dir.join("heuristics_loctree.log"), log)?;
    Ok(())
}

/// RUN.json — single source of truth (Artifact Pack v1)
fn generate_run_json(input: RunJsonInput<'_>) -> Result<()> {
    use serde_json::json;
    let RunJsonInput {
        dir,
        artifacts_root,
        config,
        checks,
        heuristics,
        resolved_target,
        resolved_bases,
        run_started_at,
        total_duration_secs,
        stage_timings,
        context_artifacts,
        context_command_timings,
        regression,
    } = input;

    let check_results: Vec<serde_json::Value> = checks
        .iter()
        .map(|c| {
            let gate_id = check_id_from_name(&c.name);
            let class = gate_class_for_check(c.status);
            let mut entry = json!({
                "gate": gate_id,
                "name": c.name,
                "status": c.status.as_str(),
                "class": gate_class_to_str(class),
                "duration_secs": c.duration.as_secs_f32(),
                "cached": c.cached,
            });
            if let Some(ref prov) = c.provenance {
                entry["exit_code"] = json!(prov.exit_code);
                entry["hard_fail_signatures"] = json!(prov.hard_fail_signatures);
            }
            entry
        })
        .collect();

    let has_failures = checks.iter().any(|c| c.is_failure());

    let generated_at = chrono::Local::now().to_rfc3339();

    let run = json!({
        "schema_version": "1.0",
        "run_started_at": run_started_at,
        "run_finished_at": &generated_at,
        "total_duration_secs": total_duration_secs,
        "freshness": {
            "fresh": true,
            "target_sha": resolved_target.commit_id,
            "base_sha": resolved_bases.first().map(|b| b.commit_id.as_str()).unwrap_or(""),
            "generated_at": &generated_at,
        },
        "repo": {
            "root": config.repo_root.display().to_string(),
            "pr_url": config.pr_url.as_deref().unwrap_or(""),
            "pr_number": config.pr_number,
            "gh_repo": config.gh_repo.as_deref().unwrap_or(""),
        },
        "artifacts_root": artifacts_root.display().to_string(),
        "refs": {
            "target": resolved_target.name,
            "target_sha": resolved_target.commit_id,
            "bases": resolved_bases.iter().map(|b| json!({
                "name": b.name,
                "sha": b.commit_id,
            })).collect::<Vec<_>>(),
        },
        "profile": config.profile.kind.as_str(),
        "flags": {
            "mode": config.execution_mode.as_str(),
            "quick": matches!(config.execution_mode, crate::cli::ExecutionMode::Quick),
            "deep": matches!(config.execution_mode, crate::cli::ExecutionMode::Deep),
            "ci": matches!(config.execution_mode, crate::cli::ExecutionMode::Ci),
            "ai_only": matches!(config.execution_mode, crate::cli::ExecutionMode::AiOnly),
            "update": matches!(config.execution_mode, crate::cli::ExecutionMode::Update),
            "remote_only": config.remote_only,
            "remote_mode": config.remote_mode,
            "fast_remote_only_standard": config.is_fast_remote_only_standard(),
            "run_lint": config.run_lint,
            "run_tests": config.run_tests,
            "run_heuristics": config.run_heuristics,
            "dashboard": config.create_dashboard,
            "cached": config.use_cache,
        },
        "runner": {
            "tool": env!("CARGO_PKG_NAME"),
            "version": env!("CARGO_PKG_VERSION"),
            "os": std::env::consts::OS,
            "arch": std::env::consts::ARCH,
        },
        "checks": check_results,
        "analysis": {
            "mode": if !config.run_heuristics {
                "disabled"
            } else if heuristics.and_then(|h| h.analysis_root.as_ref()).is_some() {
                "snapshot"
            } else {
                "local"
            },
            "root": heuristics.and_then(|h| h.analysis_root.as_deref()),
        },
        "heuristics": heuristics.map(|h| {
            let mut obj = json!({
                "dead_exports": h.summary.dead_exports,
                "unused_symbols": h.summary.dead_parrots,
                "circular_imports": h.summary.circular_imports,
                "analysis_root": h.analysis_root,
            });
            if let Some(ref reg) = h.regression {
                obj["regression"] = json!(reg);
            }
            obj
        }),
        "outcome": {
            "has_failures": has_failures,
            "checks_run": checks.len(),
            "checks_passed": checks.iter().filter(|c| matches!(c.status, crate::checks::CheckStatus::Passed)).count(),
            "checks_failed": checks.iter().filter(|c| c.is_failure()).count(),
            "checks_warned": checks.iter().filter(|c| matches!(c.status, crate::checks::CheckStatus::Warnings)).count(),
        },
        "regression": regression.map(|r| json!(r)),
        "timings": stage_timings.iter().map(|stage| json!({
            "label": stage.label,
            "duration_secs": stage.duration_secs,
        })).collect::<Vec<_>>(),
        "context_artifacts": context_artifacts.iter().map(|artifact| json!({
            "key": artifact.key,
            "path": artifact.path,
            "generated": artifact.generated,
            "recommended": artifact.recommended,
            "reason": artifact.reason,
        })).collect::<Vec<_>>(),
        "context_commands": context_command_timings.iter().map(|command| json!({
            "label": command.label,
            "artifact": command.artifact,
            "status": command.status,
            "duration_secs": command.duration_secs,
        })).collect::<Vec<_>>(),
    });

    fs::write(dir.join("RUN.json"), serde_json::to_string_pretty(&run)?)?;
    Ok(())
}

/// system_meta.txt (Artifact Pack v1)
fn generate_system_meta(dir: &Path) -> Result<()> {
    let mut content = String::new();

    content.push_str(&format!(
        "os: {} {}\n",
        std::env::consts::OS,
        std::env::consts::ARCH
    ));

    if let Ok(hostname) = std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("HOST"))
        .or_else(|_| {
            Command::new("hostname")
                .output()
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        })
    {
        content.push_str(&format!("hostname: {}\n", hostname));
    }

    content.push_str(&format!("prview_version: {}\n", env!("CARGO_PKG_VERSION")));

    if let Ok(output) = Command::new("rustc").arg("--version").output() {
        content.push_str(&format!(
            "rustc: {}\n",
            String::from_utf8_lossy(&output.stdout).trim()
        ));
    }

    if let Ok(output) = Command::new("cargo").arg("--version").output() {
        content.push_str(&format!(
            "cargo: {}\n",
            String::from_utf8_lossy(&output.stdout).trim()
        ));
    }

    if let Ok(output) = Command::new("node").arg("--version").output() {
        content.push_str(&format!(
            "node: {}\n",
            String::from_utf8_lossy(&output.stdout).trim()
        ));
    }

    if let Ok(output) = Command::new("pnpm").arg("--version").output() {
        content.push_str(&format!(
            "pnpm: {}\n",
            String::from_utf8_lossy(&output.stdout).trim()
        ));
    }

    content.push_str(&format!(
        "generated_at: {}\n",
        chrono::Local::now().to_rfc3339()
    ));

    fs::write(dir.join("system_meta.txt"), content)?;
    Ok(())
}

/// git_meta.txt (Artifact Pack v1)
fn generate_git_meta(
    dir: &Path,
    config: &Config,
    resolved_target: &ResolvedRef,
    resolved_bases: &[ResolvedRef],
) -> Result<()> {
    let mut content = String::new();

    // Remote URL
    if let Ok(output) = git_cmd()
        .args(["config", "--get", "remote.origin.url"])
        .current_dir(&config.repo_root)
        .output()
    {
        content.push_str(&format!(
            "remote_url: {}\n",
            String::from_utf8_lossy(&output.stdout).trim()
        ));
    }

    content.push_str(&format!("target_ref: {}\n", resolved_target.name));
    content.push_str(&format!("target_sha: {}\n", resolved_target.commit_id));

    for base in resolved_bases {
        content.push_str(&format!("base_ref: {}\n", base.name));
        content.push_str(&format!("base_sha: {}\n", base.commit_id));
    }

    content.push_str(&format!("head_commit: {}\n", resolved_target.commit_id));

    if let Some(ref pr_url) = config.pr_url {
        content.push_str(&format!("pr_url: {}\n", pr_url));
    }
    if let Some(ref gh_repo) = config.gh_repo {
        content.push_str(&format!("gh_repo: {}\n", gh_repo));
    }
    if let Some(pr_num) = config.pr_number {
        content.push_str(&format!("pr_number: {}\n", pr_num));
    }

    content.push_str(&format!(
        "generated_at: {}\n",
        chrono::Local::now().to_rfc3339()
    ));

    fs::write(dir.join("git_meta.txt"), content)?;
    Ok(())
}

fn collect_quick_wins(config: &Config, checks: &[CheckResult], exact_twins: usize) -> Vec<String> {
    use std::collections::HashSet;

    let mut wins = Vec::new();

    if config.profile.has_cargo {
        let has_cargo_test = checks
            .iter()
            .any(|check| check.name.eq_ignore_ascii_case("cargo test"));
        let has_clippy = checks
            .iter()
            .any(|check| check.name.eq_ignore_ascii_case("clippy"));

        if !has_cargo_test {
            if config.run_tests {
                wins.push(
                    "Run `cargo test` for runtime validation; compile-only Rust signal is still incomplete."
                        .to_string(),
                );
            } else {
                wins.push(
                    "Enable `cargo test` for this run; Rust review quality is much stronger with test signal."
                        .to_string(),
                );
            }
        }

        if !has_clippy {
            if config.run_lint {
                wins.push(
                    "Run `cargo clippy -- -D warnings` to catch idiomatic Rust issues before review."
                        .to_string(),
                );
            } else {
                wins.push(
                    "Enable `cargo clippy` for this run; it catches common Rust correctness and cleanup issues."
                        .to_string(),
                );
            }
        }
    }

    if let Some(cargo_audit) = checks
        .iter()
        .find(|check| check.name.eq_ignore_ascii_case("cargo audit"))
    {
        let mut seen_packages = HashSet::new();
        for finding in parse_cargo_audit_findings(&cargo_audit.output) {
            if !seen_packages.insert(finding.package_name.clone()) {
                continue;
            }
            if let Some(patched) = &finding.patched_versions {
                wins.push(format!(
                    "Bump `{}` to `{}` to address `{}`.",
                    finding.package_name, patched, finding.advisory_id
                ));
            } else {
                wins.push(format!(
                    "Review `{}` (`{}`) and move to a patched release.",
                    finding.package_name, finding.advisory_id
                ));
            }
            if seen_packages.len() >= 3 {
                break;
            }
        }
    }

    if exact_twins > 0 {
        wins.push(format!(
            "Inspect {} loctree exact twin pair(s) for low-risk extraction or dedup wins.",
            exact_twins
        ));
    }

    wins
}

/// FAILURES_SUMMARY.md (Artifact Pack v1)
fn generate_failures_summary(dir: &Path, checks: &[CheckResult]) -> Result<()> {
    let failures: Vec<&CheckResult> = checks.iter().filter(|c| c.is_failure()).collect();

    let mut md = String::new();
    let cargo_tree = dir.parent().and_then(load_cargo_tree_index);
    md.push_str("# Failures Summary\n\n");

    if failures.is_empty() {
        md.push_str("No blocking check failures.\n\n");
        md.push_str(&format!("{} check(s) recorded.\n", checks.len()));
        fs::write(dir.join("FAILURES_SUMMARY.md"), md)?;
        return Ok(());
    }

    md.push_str(&format!("{} check(s) failed:\n\n", failures.len()));

    for check in &failures {
        md.push_str(&format!("## {}\n\n", check.name));
        md.push_str(&format!("- **Status:** {}\n", check.status.as_str()));
        md.push_str(&format!(
            "- **Duration:** {:.2}s\n",
            check.duration.as_secs_f32()
        ));
        if check.cached {
            md.push_str("- **Source:** cached result\n");
        }

        if let Some(ref prov) = check.provenance {
            md.push_str(&format!("- **Command:** `{}`\n", prov.command));
            md.push_str(&format!("- **Exit code:** {:?}\n", prov.exit_code));
            if !prov.hard_fail_signatures.is_empty() {
                md.push_str(&format!(
                    "- **Hard fail signatures:** {}\n",
                    prov.hard_fail_signatures.join(", ")
                ));
            }
        }

        md.push_str(&format!(
            "- **Log:** `20_quality/{}.log`\n",
            check_id_from_name(&check.name)
        ));

        // Root cause analysis
        if let Some(rc) = extract_root_cause(check) {
            md.push_str("\n### Root Cause\n\n");
            md.push_str(&format!("- **Cause:** {}\n", rc.cause));
            if !rc.evidence.is_empty() {
                md.push_str(&format!("- **Evidence:** `{}`\n", rc.evidence));
            }
            md.push_str(&format!("- **Hint:** {}\n", rc.hint));
        }

        let cargo_audit_findings = if check.name.eq_ignore_ascii_case("cargo audit") {
            parse_cargo_audit_findings(&check.output)
        } else {
            Vec::new()
        };
        let failed_tests = if check.name.eq_ignore_ascii_case("cargo test") {
            parsers::cargo_test::extract_failed_tests_with_locations(&check.output)
        } else {
            Vec::new()
        };

        if !cargo_audit_findings.is_empty() {
            md.push_str("\n### Advisories\n\n");
            append_cargo_audit_findings(&mut md, &cargo_audit_findings, None, cargo_tree.as_ref());
        } else if !failed_tests.is_empty() {
            md.push_str("\n### Failed Tests\n\n");
            for failed_test in &failed_tests {
                if let Some(file) = &failed_test.file {
                    let line = failed_test.line.unwrap_or_default();
                    match failed_test.column {
                        Some(column) => md.push_str(&format!(
                            "- `{}` ({}:{}:{})\n",
                            failed_test.name, file, line, column
                        )),
                        None => {
                            md.push_str(&format!("- `{}` ({}:{})\n", failed_test.name, file, line))
                        }
                    }
                } else {
                    md.push_str(&format!("- `{}`\n", failed_test.name));
                }
            }
            md.push('\n');
        } else {
            // First 12 lines of output as preview for non-structured failures
            let preview: Vec<&str> = check.output.lines().take(12).collect();
            if !preview.is_empty() {
                md.push_str("\n```\n");
                md.push_str(&preview.join("\n"));
                if check.output.lines().count() > 12 {
                    md.push_str("\n... (truncated, see full log)");
                }
                md.push_str("\n```\n");
            }
        }
        md.push('\n');
    }

    fs::write(dir.join("FAILURES_SUMMARY.md"), md)?;
    Ok(())
}

/// MANIFEST.json with SHA256 hashes (Artifact Pack v1, runs LAST)
/// OS packaging cruft that must never reach the shipped MANIFEST or ZIP.
fn is_packaging_junk(path: &Path) -> bool {
    matches!(
        path.file_name().and_then(|n| n.to_str()),
        Some(".DS_Store") | Some("Thumbs.db")
    )
}

fn generate_manifest(out_dir: &Path) -> Result<()> {
    use serde_json::json;
    use sha2::{Digest, Sha256};
    use walkdir::WalkDir;

    let mut files = Vec::new();

    for entry in WalkDir::new(out_dir)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
    {
        let path = entry.path();
        let rel = path.strip_prefix(out_dir).unwrap_or(path);
        let rel_str = rel.to_string_lossy().to_string();

        // Skip the manifest itself and the ZIP envelope (path-separator
        // agnostic), plus OS packaging junk. The manifest hashes every shipped
        // pack file except itself and the archive that wraps them; SANITY.json
        // is written after this step, so it is not covered here.
        if rel.file_name() == Some(std::ffi::OsStr::new("MANIFEST.json"))
            || rel_str.ends_with(".zip")
            || is_packaging_junk(path)
        {
            continue;
        }

        let content = read_within(out_dir, path)?;
        let mut hasher = Sha256::new();
        hasher.update(&content);
        let hash = format!("{:x}", hasher.finalize());

        files.push(json!({
            "path": rel_str,
            "sha256": hash,
            "size": content.len(),
        }));
    }

    files.sort_by(|a, b| {
        a["path"]
            .as_str()
            .unwrap_or("")
            .cmp(b["path"].as_str().unwrap_or(""))
    });

    let manifest = json!({
        "schema_version": "1.0",
        "generated_at": chrono::Local::now().to_rfc3339(),
        "file_count": files.len(),
        "files": files,
    });

    let summary_dir = out_dir.join("00_summary");
    fs::create_dir_all(&summary_dir)?;
    fs::write(
        summary_dir.join("MANIFEST.json"),
        serde_json::to_string_pretty(&manifest)?,
    )?;

    Ok(())
}

fn create_zip(dir: &Path, emit_human_stdout: bool) -> Result<()> {
    use colored::Colorize;
    use std::io::Write;
    use walkdir::WalkDir;
    use zip::ZipWriter;
    use zip::write::SimpleFileOptions;

    let zip_rel_path = Path::new("artifacts.zip");
    let zip_path = dir.join(zip_rel_path);
    let file = crate::paths::create_file_within(dir, zip_rel_path)?;
    let mut zip = ZipWriter::new(file);

    let options = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);

    for entry in WalkDir::new(dir).into_iter().filter_map(|e| e.ok()) {
        let path = entry.path();
        if path == zip_path || path.extension().map(|e| e == "zip").unwrap_or(false) {
            continue;
        }
        // Don't ship OS packaging junk to reviewers.
        if is_packaging_junk(path) {
            continue;
        }

        if path.is_file() {
            let name = path.strip_prefix(dir).unwrap_or(path);
            // Don't ship OS packaging junk to reviewers.
            if is_packaging_junk(name) {
                continue;
            }
            zip.start_file(name.to_string_lossy(), options)?;
            let content = read_within(dir, name)?;
            zip.write_all(&content)?;
        }
    }

    zip.finish()?;

    // Verify the shipped archive actually carries the pack metadata a consumer
    // needs to validate it standalone. Zipping before these files existed used
    // to produce an archive that failed its own required-files sanity check.
    verify_zip_contains_metadata(&zip_path)?;

    let size = fs::metadata(&zip_path)?.len();
    let size_str = if size > 1_000_000 {
        format!("{:.1}M", size as f64 / 1_000_000.0)
    } else {
        format!("{:.1}K", size as f64 / 1_000.0)
    };

    if emit_human_stdout {
        println!(
            "  {} ZIP created: {} ({})",
            "✓".green(),
            zip_path.display(),
            size_str
        );
    }

    Ok(())
}

/// Metadata files every shipped `artifacts.zip` must contain so a consumer can
/// validate the pack after extracting it: the source of truth, the integrity
/// manifest, and the sanity verdict.
const REQUIRED_ZIP_METADATA: [&str; 3] = [
    "00_summary/RUN.json",
    "00_summary/MANIFEST.json",
    "00_summary/SANITY.json",
];

/// Open a freshly written archive and confirm it carries the required pack
/// metadata. Fails loudly rather than shipping an incomplete pack.
fn verify_zip_contains_metadata(zip_path: &Path) -> Result<()> {
    let file = fs::File::open(zip_path)?;
    let mut archive = zip::ZipArchive::new(file)?;

    let mut names: std::collections::HashSet<String> = std::collections::HashSet::new();
    for i in 0..archive.len() {
        if let Ok(entry) = archive.by_index(i) {
            names.insert(entry.name().replace('\\', "/"));
        }
    }

    let missing: Vec<&str> = REQUIRED_ZIP_METADATA
        .iter()
        .copied()
        .filter(|req| !names.contains(*req))
        .collect();

    if !missing.is_empty() {
        anyhow::bail!(
            "artifacts.zip is missing required pack metadata: {}",
            missing.join(", ")
        );
    }

    Ok(())
}

// ─────────────────────────────────────────────────────────────────

// ── checks-status.json ──────────────────────────────────────────────

fn generate_checks_status_json(
    dir: &Path,
    config: &Config,
    checks: &[CheckResult],
    heuristics: Option<&HeuristicsResult>,
) -> Result<()> {
    use serde_json::json;
    use std::collections::HashMap;

    let ran: HashMap<String, &CheckResult> =
        checks.iter().map(|c| (c.name.to_lowercase(), c)).collect();

    let all_profile_checks = crate::checks::get_checks_for_profile(config);
    let mut status_map = serde_json::Map::new();

    for check in &all_profile_checks {
        let name = check.name();
        let id = check_id_from_name(name);

        if let Some(result) = ran.get(&name.to_lowercase()) {
            let status_str = result.status.as_str().to_string();
            status_map.insert(id, json!(status_str));
        } else {
            let reason = match check.check_eligibility(config) {
                crate::checks::CheckEligibility::Skip(r) => r,
                crate::checks::CheckEligibility::Run => "unknown skip reason".to_string(),
            };
            status_map.insert(id, json!(format!("skipped ({})", reason)));
        }
    }

    let heuristics_status = if !config.run_heuristics {
        if config.is_fast_remote_only_standard() {
            "skipped (fast remote-only preset)".to_string()
        } else {
            "skipped (heuristics disabled)".to_string()
        }
    } else if let Some(h) = heuristics {
        if h.summary.total_files == 0 {
            "skipped (no files scanned)".to_string()
        } else if h.summary.dead_exports > 0 || h.summary.circular_imports > 0 {
            "warnings".to_string()
        } else {
            "passed".to_string()
        }
    } else {
        "skipped (heuristics unavailable)".to_string()
    };
    status_map.insert("heuristics_loctree".to_string(), json!(heuristics_status));

    fs::write(
        dir.join("checks-status.json"),
        serde_json::to_string_pretty(&serde_json::Value::Object(status_map))?,
    )?;
    Ok(())
}
