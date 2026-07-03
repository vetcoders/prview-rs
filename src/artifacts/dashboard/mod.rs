//! HTML Dashboard generation (v2)
//!
//! Creates a rich, production-quality HTML dashboard with:
//! - Merge decision (ALLOW/BLOCK) based on policy, not raw check status
//! - Action Center: failures, breaking changes, coverage signal
//! - Quality checks with blocking badges and provenance
//! - Files with filters, sorting, hotspot badges, per-file diff links
//! - Breaking changes table
//! - Coverage delta heuristic
//! - Inline findings (SARIF)
//! - Change distribution, commit timeline, code statistics
//! - Dark theme, responsive, fully interactive (vanilla JS)

use super::{
    DashboardContext, build_merge_decision_view, build_review_caveats,
    signal::{BreakingKind, BreakingRisk, ReviewFileCategory, classify_review_file},
};
use crate::checks::{CheckResult, CheckStatus};
use crate::config::Config;
use crate::git::{Diff, FileChange, FileStatus, short_sha};
use crate::heuristics::HeuristicsResult;
use crate::paths::read_to_string_within;
use anyhow::Result;
use std::collections::BTreeMap;
use std::fmt::Write as FmtWrite;
use std::path::Path;

struct BuildHtmlInput<'a> {
    config: &'a Config,
    diffs: &'a [Diff],
    checks: &'a [CheckResult],
    heuristics: Option<&'a HeuristicsResult>,
    ctx: &'a DashboardContext,
    report_json: &'a str,
    dir: &'a Path,
    pr_review: &'a str,
    regression: Option<&'a crate::regression::RegressionReport>,
}

pub fn generate(
    dir: &Path,
    config: &Config,
    diffs: &[Diff],
    checks: &[CheckResult],
    heuristics: Option<&HeuristicsResult>,
    ctx: &DashboardContext,
    regression: Option<&crate::regression::RegressionReport>,
) -> Result<()> {
    // Build report.json content for embedding (self-contained dashboard)
    let report_json = build_embedded_report_json(dir);
    // Read PR_REVIEW.md for narrative section (best-effort)
    let pr_review = read_to_string_within(dir, Path::new("PR_REVIEW.md")).unwrap_or_default();
    let html = build_html(BuildHtmlInput {
        config,
        diffs,
        checks,
        heuristics,
        ctx,
        report_json: &report_json,
        dir,
        pr_review: &pr_review,
        regression,
    });
    std::fs::write(dir.join("dashboard.html"), html)?;
    Ok(())
}

/// Try to read report.json from the same directory for embedding in HTML.
fn build_embedded_report_json(dir: &Path) -> String {
    read_to_string_within(dir, Path::new("report.json")).unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn escape_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#x27;"),
            _ => out.push(ch),
        }
    }
    out
}

fn format_number(n: usize) -> String {
    if n < 1_000 {
        return n.to_string();
    }
    let s = n.to_string();
    let mut result = String::with_capacity(s.len() + s.len() / 3);
    for (i, ch) in s.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 {
            result.push(',');
        }
        result.push(ch);
    }
    result.chars().rev().collect()
}

fn status_char(status: FileStatus) -> &'static str {
    match status {
        FileStatus::Added => "A",
        FileStatus::Modified => "M",
        FileStatus::Deleted => "D",
        FileStatus::Renamed => "R",
        FileStatus::Copied => "C",
    }
}

fn check_icon(status: CheckStatus) -> &'static str {
    match status {
        CheckStatus::Passed => "&#x2713;",
        CheckStatus::Failed => "&#x2717;",
        CheckStatus::Warnings => "&#x26A0;",
        CheckStatus::Skipped => "&#x25CB;",
        CheckStatus::Error => "&#x21;",
    }
}

fn check_badge_class(status: CheckStatus) -> &'static str {
    match status {
        CheckStatus::Passed => "badge-success",
        CheckStatus::Failed | CheckStatus::Error => "badge-error",
        CheckStatus::Warnings => "badge-warning",
        CheckStatus::Skipped => "badge-muted",
    }
}

fn dir_of(path: &str) -> &str {
    match path.rfind('/') {
        Some(i) => &path[..i],
        None => "(root)",
    }
}

fn filename_of(path: &str) -> &str {
    match path.rfind('/') {
        Some(i) => &path[i + 1..],
        None => path,
    }
}

fn lang_hue(lang: &str) -> u16 {
    let mut h: u32 = 5381;
    for b in lang.bytes() {
        h = h.wrapping_mul(33).wrapping_add(b as u32);
    }
    (h % 360) as u16
}

/// Generate a safe HTML id from a file path (alphanumeric + underscores only).
///
/// Different paths always produce different IDs: each non-alphanumeric
/// character is encoded as `_xHH_` (hex of its ASCII byte) so that e.g.
/// `src/foo-bar.rs` and `src/foo_bar.rs` never collide.
fn safe_id(path: &str) -> String {
    let mut out = String::with_capacity(path.len() * 2);
    for c in path.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c);
        } else {
            // Encode as _xHH_ so every distinct character keeps a unique repr
            out.push_str(&format!("_x{:02x}_", c as u32));
        }
    }
    out
}

/// Wrap section HTML in a collapsible container.
/// `expanded`: whether the section starts expanded.
/// `summary`: one-line text shown in the collapsed header bar.
fn collapsible_section(
    id: &str,
    title_key: &str,
    title: &str,
    summary_html: &str,
    expanded: bool,
    hide_inner_header: bool,
    inner_html: &str,
) -> String {
    let class = if expanded {
        "section-collapsible expanded"
    } else {
        "section-collapsible"
    };
    let body_id = format!("{id}__body");
    let rendered_inner = if hide_inner_header {
        strip_first_section_header(inner_html)
    } else {
        inner_html.to_string()
    };
    format!(
        r#"<section class="{class}" data-section-id="{id}">
            <div class="section-header section-collapsible-header" role="button" tabindex="0" aria-controls="{body_id}" aria-expanded="{expanded_attr}">
                <div class="section-collapsible-title"><span class="toggle-indicator">&#9654;</span> <strong data-i18n="{title_key}">{title}</strong></div>
                <span class="section-summary">{summary}</span>
            </div>
            <div id="{body_id}" class="section-body">{inner}</div>
        </section>"#,
        class = class,
        id = id,
        body_id = body_id,
        expanded_attr = if expanded { "true" } else { "false" },
        title_key = title_key,
        title = title,
        summary = summary_html,
        inner = rendered_inner,
    )
}

fn strip_first_section_header(html: &str) -> String {
    let Some(header_start) = html.find(r#"<div class="section-header""#) else {
        return html.to_string();
    };

    let bytes = html.as_bytes();
    let mut idx = header_start;
    let mut depth = 0usize;
    let mut header_end = None;

    while idx < bytes.len() {
        if html[idx..].starts_with("<div") {
            depth += 1;
            idx += 4;
            continue;
        }
        if html[idx..].starts_with("</div>") {
            depth = depth.saturating_sub(1);
            idx += 6;
            if depth == 0 {
                header_end = Some(idx);
                break;
            }
            continue;
        }
        idx += 1;
    }

    let Some(header_end) = header_end else {
        return html.to_string();
    };

    let mut stripped = String::with_capacity(html.len());
    stripped.push_str(&html[..header_start]);
    stripped.push_str(&html[header_end..]);
    stripped
}

fn i18n_template(key: &str, fallback: &str, attrs: &[(&str, String)]) -> String {
    let mut attr_html = String::new();
    for (name, value) in attrs {
        let _ = write!(
            attr_html,
            r#" data-{}="{}""#,
            escape_html(name),
            escape_html(value),
        );
    }
    format!(
        r#"<span data-i18n-template="{key}"{attrs}>{fallback}</span>"#,
        key = escape_html(key),
        attrs = attr_html,
        fallback = escape_html(fallback),
    )
}

/// HTML badge for breaking change risk level (B3).
fn risk_badge_html(risk: &BreakingRisk) -> &'static str {
    match risk {
        BreakingRisk::High => {
            r#"<span class="badge badge-error" style="font-size:10px">HIGH</span>"#
        }
        BreakingRisk::Medium => {
            r#"<span class="badge badge-warning" style="font-size:10px">MED</span>"#
        }
        BreakingRisk::Low => r#"<span class="badge badge-muted" style="font-size:10px">LOW</span>"#,
    }
}

/// Check if a per-file diff patch exists for a given file path.
fn find_per_file_diff<'a>(file_path: &str, available_patches: &'a [String]) -> Option<&'a str> {
    super::signal::find_per_file_patch(file_path, available_patches)
}

/// Decode a per-file diff patch filename back to a human-readable label.
///
/// Input: `abc12345__src~2Flib.rs.patch` → Output: `src/lib.rs`
fn decode_patch_label(patch_filename: &str) -> String {
    // Strip .patch suffix
    let without_ext = patch_filename
        .strip_suffix(".patch")
        .unwrap_or(patch_filename);
    // Strip commit prefix (everything up to and including the first `__`)
    let encoded = match without_ext.find("__") {
        Some(pos) => &without_ext[pos + 2..],
        None => without_ext,
    };
    // Decode ~XX sequences back to original bytes
    let mut result = Vec::with_capacity(encoded.len());
    let bytes = encoded.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'~'
            && i + 2 < bytes.len()
            && let Ok(byte) = u8::from_str_radix(&encoded[i + 1..i + 3], 16)
        {
            result.push(byte);
            i += 3;
            continue;
        }
        result.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&result).into_owned()
}

/// Classify a file path for noise-filtering purposes (B7).
fn file_category(path: &str) -> &'static str {
    match classify_review_file(path) {
        ReviewFileCategory::Code => "code",
        ReviewFileCategory::Test => "test",
        ReviewFileCategory::Config => "config",
        ReviewFileCategory::Asset => "asset",
        ReviewFileCategory::I18n => "i18n",
        ReviewFileCategory::NonCode => "non-code",
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct FileBreakdown {
    total: usize,
    code: usize,
    tests: usize,
    non_code: usize,
    additions: usize,
    deletions: usize,
}

fn build_file_breakdown(diff: Option<&Diff>) -> FileBreakdown {
    let Some(diff) = diff else {
        return FileBreakdown::default();
    };

    let mut breakdown = FileBreakdown {
        total: diff.files.len(),
        ..FileBreakdown::default()
    };

    for file in &diff.files {
        match file_category(&file.path) {
            "code" => breakdown.code += 1,
            "test" => breakdown.tests += 1,
            _ => breakdown.non_code += 1,
        }
        breakdown.additions += file.additions;
        breakdown.deletions += file.deletions;
    }

    breakdown
}

fn truncate_plain_text(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let mut out = String::with_capacity(max_chars + 1);
    for (idx, ch) in text.chars().enumerate() {
        if idx >= max_chars {
            break;
        }
        out.push(ch);
    }
    format!("{}…", out.trim_end())
}

fn sanitize_markdown_preview_line(line: &str) -> String {
    let mut sanitized = line.trim().to_string();
    while let Some(rest) = sanitized.strip_prefix('>') {
        sanitized = rest.trim_start().to_string();
    }

    for marker in ["**", "__", "`", "~~"] {
        sanitized = sanitized.replace(marker, "");
    }

    if let Some(rest) = sanitized.strip_prefix("- ") {
        sanitized = rest.trim_start().to_string();
    } else if let Some(rest) = sanitized.strip_prefix("* ") {
        sanitized = rest.trim_start().to_string();
    }

    sanitized.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn extract_narrative_preview(pr_review_content: &str) -> String {
    let mut paragraph = Vec::new();
    for line in pr_review_content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            if !paragraph.is_empty() {
                break;
            }
            continue;
        }
        if trimmed.starts_with('#')
            || trimmed.starts_with("```")
            || trimmed.starts_with('|')
            || trimmed.starts_with("- ")
            || trimmed.starts_with("* ")
        {
            continue;
        }
        let sanitized = sanitize_markdown_preview_line(trimmed);
        if !sanitized.is_empty() {
            paragraph.push(sanitized);
        }
    }

    if paragraph.is_empty() {
        "Review narrative available".to_string()
    } else {
        truncate_plain_text(&paragraph.join(" "), 120)
    }
}

mod assets;
mod sections;

use assets::{css, js};
use sections::*;

#[cfg(test)]
mod tests;
#[cfg(test)]
mod trends_tests;

// ---------------------------------------------------------------------------
// Main builder
// ---------------------------------------------------------------------------

fn build_html(input: BuildHtmlInput<'_>) -> String {
    let BuildHtmlInput {
        config,
        diffs,
        checks,
        heuristics,
        ctx,
        report_json,
        dir,
        pr_review,
        regression,
    } = input;
    let diff = diffs.first();
    let target = diff.map(|d| d.target.as_str()).unwrap_or("N/A");

    // Count batch files for commits section
    let batch_count = (1..=20usize)
        .take_while(|i| {
            dir.join(format!("10_diff/per-commit-diffs/batch-{:02}.patch", i))
                .exists()
        })
        .count();

    let header = build_header(config, diff, ctx, dir, report_json);
    let files_summary_widget = build_files_summary_widget(diff);
    let delta_html = build_delta_section(ctx, checks);
    let action_center = build_action_center(ctx, checks, heuristics);
    // PRV-101: detailed merge-decision readout (Policy/Quality/Merge triad + reason),
    // surfacing the decision reason that the compact header chip omits. Skipped when
    // there are no gates to decide on, matching the header's N/A behavior.
    let merge_decision_card = if ctx.check_gates.is_empty() {
        String::new()
    } else {
        build_merge_decision_card(ctx)
    };
    let regression_widget_html = build_regression_score_widget(regression);
    let blockers_html = build_blockers_section(ctx, checks);
    let security_html = build_security_section(checks, ctx);
    let lint_html = build_lint_metrics_section(ctx, diffs);
    let time_budget_html = build_time_budget(checks);
    let narrative_html = build_narrative_section(pr_review);
    let distribution_html = build_distribution_section(diff);
    let checks_html = build_checks_section(checks, ctx);
    let files_html = build_files_section(diff, ctx);
    let ownership_html = build_ownership_section(ctx);
    let breaking_html = build_breaking_section(ctx);
    let coverage_html = build_coverage_section(ctx);
    let findings_html = build_sarif_table_section(ctx);
    let assets_html = build_assets_section(diff);
    let i18n_html = build_i18n_section(ctx);
    let commits_html = build_commits_section(diff, config.pr_url.as_deref(), batch_count);
    let trends_html = build_trends_section(ctx);
    let flaky_html = build_flaky_section(ctx);
    let loctree_html = build_loctree_section(heuristics);
    let regression_details_html = build_regression_details_section(ctx, heuristics, regression);
    let artifacts_html = build_artifacts_section(ctx);
    let breakdown = build_file_breakdown(diff);

    // --- Summaries for collapsible headers ---
    let checks_passed = checks
        .iter()
        .filter(|c| c.status == CheckStatus::Passed)
        .count();
    let checks_failed = checks
        .iter()
        .filter(|c| matches!(c.status, CheckStatus::Failed | CheckStatus::Error))
        .count();
    let checks_warn = checks
        .iter()
        .filter(|c| c.status == CheckStatus::Warnings)
        .count();
    let checks_summary = i18n_template(
        "summary.checks",
        &format!(
            "{} passed, {} failed, {} warn",
            checks_passed, checks_failed, checks_warn
        ),
        &[
            ("passed", checks_passed.to_string()),
            ("failed", checks_failed.to_string()),
            ("warn", checks_warn.to_string()),
        ],
    );

    let files_summary = if breakdown.total == 0 {
        i18n_template("count.files", "0 files", &[("count", "0".to_string())])
    } else {
        i18n_template(
            "summary.filesBreakdown",
            &format!(
                "{} files | {} code | {} tests | {} non-code",
                breakdown.total, breakdown.code, breakdown.tests, breakdown.non_code
            ),
            &[
                ("total", breakdown.total.to_string()),
                ("code", breakdown.code.to_string()),
                ("tests", breakdown.tests.to_string()),
                ("other", breakdown.non_code.to_string()),
            ],
        )
    };

    let breaking_summary = if ctx.breaking.is_empty() {
        i18n_template("summary.none", "none", &[])
    } else {
        i18n_template(
            "summary.findingsCount",
            &format!("{} findings", ctx.breaking.len()),
            &[("count", ctx.breaking.len().to_string())],
        )
    };

    let coverage_summary = if ctx.coverage.total_source > 0 {
        i18n_template(
            "summary.coveragePct",
            &format!("{}% coverage", ctx.coverage.pct),
            &[("pct", ctx.coverage.pct.to_string())],
        )
    } else {
        escape_html("N/A")
    };

    let commit_count = diff.map(|d| d.commits.len()).unwrap_or(0);
    let commits_summary = i18n_template(
        "summary.commitsCount",
        &format!("{} commits", commit_count),
        &[("count", commit_count.to_string())],
    );

    let findings_summary = if ctx.findings.is_empty() {
        i18n_template("summary.none", "none", &[])
    } else {
        i18n_template(
            "summary.findingsCount",
            &format!("{} findings", ctx.findings.len()),
            &[("count", ctx.findings.len().to_string())],
        )
    };
    let narrative_summary = format!(
        "{} | {} commits | {} files",
        extract_narrative_preview(pr_review),
        diff.map(|d| d.commits.len()).unwrap_or(0),
        breakdown.total
    );
    let statistics_body = [
        loctree_html.as_str(),
        lint_html.as_str(),
        distribution_html.as_str(),
        assets_html.as_str(),
    ]
    .iter()
    .copied()
    .filter(|s| !s.is_empty())
    .collect::<String>();

    // --- Tier 2: important, collapsed ---
    let checks_html = if checks_html.is_empty() {
        String::new()
    } else {
        collapsible_section(
            "section-checks",
            "section.checks",
            "Checks",
            &checks_summary,
            false,
            true,
            &checks_html,
        )
    };
    let files_html = if files_html.is_empty() {
        String::new()
    } else {
        collapsible_section(
            "section-files",
            "section.filesChanged",
            "Files Changed",
            &files_summary,
            false,
            true,
            &files_html,
        )
    };
    let breaking_html = if breaking_html.is_empty() {
        String::new()
    } else {
        collapsible_section(
            "section-breaking",
            "section.breaking",
            "Breaking",
            &breaking_summary,
            false,
            true,
            &breaking_html,
        )
    };
    let coverage_html = if coverage_html.is_empty() {
        String::new()
    } else {
        collapsible_section(
            "section-coverage",
            "section.coverage",
            "Coverage",
            &coverage_summary,
            false,
            true,
            &coverage_html,
        )
    };
    let findings_html = if findings_html.is_empty() {
        String::new()
    } else {
        collapsible_section(
            "section-findings",
            "section.findings",
            "Findings",
            &findings_summary,
            false,
            true,
            &findings_html,
        )
    };
    let statistics_bundle = if statistics_body.is_empty() {
        String::new()
    } else {
        format!(
            r#"<div class="section" id="section-statistics" style="margin-bottom:0">{}</div>"#,
            statistics_body
        )
    };
    let statistics_html = if statistics_bundle.is_empty() {
        String::new()
    } else {
        collapsible_section(
            "section-statistics",
            "section.statistics",
            "Statistics",
            &i18n_template(
                "summary.statisticsBundle",
                "loctree + lint + change distribution",
                &[],
            ),
            false,
            false,
            &statistics_bundle,
        )
    };
    let regression_details_html = if regression_details_html.is_empty() {
        String::new()
    } else {
        collapsible_section(
            "section-regression",
            "section.regression",
            "Regression",
            &i18n_template(
                "summary.regressionBundle",
                "score + heuristics + risky files",
                &[],
            ),
            false,
            true,
            &regression_details_html,
        )
    };
    let artifacts_html = if artifacts_html.is_empty() {
        String::new()
    } else {
        collapsible_section(
            "section-artifacts",
            "section.artifactsExplorer",
            "Artifacts Explorer",
            &i18n_template("summary.packContents", "artifact pack contents", &[]),
            false,
            true,
            &artifacts_html,
        )
    };

    // --- Tier 3: lower priority, collapsed ---
    let narrative_html = if narrative_html.is_empty() {
        String::new()
    } else {
        collapsible_section(
            "section-narrative",
            "section.narrative",
            "Narrative",
            &escape_html(&narrative_summary),
            false,
            false,
            &narrative_html,
        )
    };
    let commits_html = if commits_html.is_empty() {
        String::new()
    } else {
        collapsible_section(
            "section-commits",
            "section.commits",
            "Commits",
            &commits_summary,
            false,
            true,
            &commits_html,
        )
    };
    let ownership_html = if ownership_html.is_empty() {
        String::new()
    } else {
        collapsible_section(
            "section-ownership",
            "section.ownership",
            "Ownership",
            &i18n_template("summary.codeOwners", "code owners", &[]),
            false,
            true,
            &ownership_html,
        )
    };
    let trends_html = if trends_html.is_empty() {
        String::new()
    } else {
        collapsible_section(
            "section-trends",
            "section.trends",
            "Trends",
            &i18n_template("summary.runHistory", "run history", &[]),
            false,
            true,
            &trends_html,
        )
    };
    let flaky_html = if flaky_html.is_empty() {
        String::new()
    } else {
        collapsible_section(
            "section-flaky",
            "section.flaky",
            "Flaky",
            &i18n_template("summary.stabilityAnalysis", "stability analysis", &[]),
            false,
            true,
            &flaky_html,
        )
    };
    let i18n_html = if i18n_html.is_empty() {
        String::new()
    } else {
        collapsible_section(
            "section-i18n",
            "section.i18n",
            "i18n",
            &i18n_template("summary.translationDelta", "translation changes", &[]),
            false,
            true,
            &i18n_html,
        )
    };
    let time_budget_html = if time_budget_html.is_empty() {
        String::new()
    } else {
        collapsible_section(
            "section-time-budget",
            "section.timeBudget",
            "Time Budget",
            &i18n_template("summary.checkDurations", "check durations", &[]),
            false,
            true,
            &time_budget_html,
        )
    };
    let security_html = if security_html.is_empty() {
        String::new()
    } else {
        collapsible_section(
            "section-security",
            "section.security",
            "Security",
            &i18n_template("summary.securityChecks", "security review", &[]),
            false,
            true,
            &security_html,
        )
    };

    // Static dashboard CSS plus the mdrender stylesheet (scoped under .mdr) for
    // the narrative section's rendered markdown.
    let css_content = format!(
        "{}\n{}",
        css(),
        crate::mdrender::stylesheet(&narrative_theme())
    );
    let js_content = js();

    // Build sidebar nav based on available sections
    let has_security = checks.iter().any(|c| is_security_check(&c.name));
    let fail_count = checks
        .iter()
        .filter(|c| matches!(c.status, CheckStatus::Failed | CheckStatus::Error))
        .count();
    let warn_count = checks
        .iter()
        .filter(|c| matches!(c.status, CheckStatus::Warnings))
        .count();
    let heuristic_issue_count = heuristics
        .and_then(|h| h.loctree.as_ref())
        .filter(|l| l.available)
        .map(|l| {
            l.dead_exports.len()
                + l.cycles.len()
                + l.twins.exact_twins.len()
                + l.twins.dead_parrots.len()
        })
        .unwrap_or(0);
    let security_issue_count = checks
        .iter()
        .filter(|c| is_security_check(&c.name) && c.status != CheckStatus::Passed)
        .count();

    let repo_display = config.gh_repo.clone().unwrap_or_else(|| config.repo_name());
    let mut nav = String::new();
    let _ = write!(
        nav,
        r#"<nav class="sidebar-nav"><div class="sidebar-context"><div class="sidebar-context-repo">{repo}</div><div class="sidebar-context-branch">{target} &#x2192; {base}</div></div>"#,
        repo = escape_html(&repo_display),
        target = escape_html(target),
        base = escape_html(diff.map(|d| d.base.as_str()).unwrap_or("N/A")),
    );

    // -- DECISION group --
    let _ = write!(
        nav,
        "<div class=\"nav-group-label\" data-i18n=\"nav.decision\">Decision</div>"
    );
    let _ = write!(
        nav,
        "<a href=\"#section-overview\" data-i18n=\"nav.overview\">Overview</a>"
    );
    if heuristics.is_some_and(|h| h.regression.is_some()) || regression.is_some() {
        let reg_badge = if regression
            .is_some_and(|r| !matches!(r.score.severity, crate::regression::score::Severity::OK))
        {
            let sev = regression
                .map(|r| r.score.severity.as_str())
                .unwrap_or("risk");
            format!(
                r#" <span class="nav-badge badge-warn">{}</span>"#,
                escape_html(sev)
            )
        } else {
            String::new()
        };
        let _ = write!(
            nav,
            "<a href=\"#section-regression\"><span data-i18n=\"nav.regression\">Regression</span>{}</a>",
            reg_badge
        );
    }

    // -- QUALITY group --
    let _ = write!(
        nav,
        "<div class=\"nav-group-label\" data-i18n=\"nav.quality\">Quality</div>"
    );
    if !checks.is_empty() {
        if fail_count > 0 {
            let _ = write!(
                nav,
                "<a href=\"#section-checks\"><span data-i18n=\"nav.checks\">Checks</span> <span class=\"nav-badge badge-error\">{}</span></a>",
                fail_count
            );
        } else if warn_count > 0 {
            let _ = write!(
                nav,
                "<a href=\"#section-checks\"><span data-i18n=\"nav.checks\">Checks</span> <span class=\"nav-badge badge-warn\">{}</span></a>",
                warn_count
            );
        } else {
            let _ = write!(
                nav,
                "<a href=\"#section-checks\" data-i18n=\"nav.checks\">Checks</a>"
            );
        }
    }
    if ctx.coverage.total_source > 0 {
        if ctx.coverage.pct < 80 {
            let _ = write!(
                nav,
                "<a href=\"#section-coverage\"><span data-i18n=\"nav.coverage\">Coverage</span> <span class=\"nav-badge badge-warn\">{}</span></a>",
                ctx.coverage.pct
            );
        } else {
            let _ = write!(
                nav,
                "<a href=\"#section-coverage\" data-i18n=\"nav.coverage\">Coverage</a>"
            );
        }
    }
    if !ctx.breaking.is_empty() {
        let _ = write!(
            nav,
            "<a href=\"#section-breaking\"><span data-i18n=\"nav.breaking\">Breaking</span> <span class=\"nav-badge badge-warn\">{}</span></a>",
            ctx.breaking.len()
        );
    }
    if !ctx.findings.is_empty() {
        let _ = write!(
            nav,
            "<a href=\"#section-findings\"><span data-i18n=\"nav.findings\">Findings</span> <span class=\"nav-badge badge-warn\">{}</span></a>",
            ctx.findings.len()
        );
    }
    // -- CODE group --
    let _ = write!(
        nav,
        "<div class=\"nav-group-label\" data-i18n=\"nav.code\">Code</div>"
    );
    let _ = write!(
        nav,
        "<a href=\"#section-files\" data-i18n=\"nav.files\">Files</a>"
    );
    if !statistics_bundle.is_empty() {
        if heuristic_issue_count > 0 {
            let _ = write!(
                nav,
                "<a href=\"#section-statistics\"><span data-i18n=\"nav.statistics\">Statistics</span> <span class=\"nav-badge badge-warn\">{}</span></a>",
                heuristic_issue_count
            );
        } else {
            let _ = write!(
                nav,
                "<a href=\"#section-statistics\" data-i18n=\"nav.statistics\">Statistics</a>"
            );
        }
    }
    if !ctx.ownership_map.is_empty() {
        let _ = write!(
            nav,
            "<a href=\"#section-ownership\" data-i18n=\"nav.ownership\">Ownership</a>"
        );
    }

    // -- META group --
    let _ = write!(
        nav,
        "<div class=\"nav-group-label\" data-i18n=\"nav.meta\">Meta</div>"
    );
    if !pr_review.trim().is_empty() {
        let _ = write!(
            nav,
            "<a href=\"#section-narrative\" data-i18n=\"nav.narrative\">Narrative</a>"
        );
    }
    let _ = write!(
        nav,
        "<a href=\"#section-commits\" data-i18n=\"nav.commits\">Commits</a>"
    );
    if ctx.run_history.len() >= 2 {
        let _ = write!(
            nav,
            "<a href=\"#section-trends\" data-i18n=\"nav.trends\">Trends</a>"
        );
    }
    if ctx.run_history.len() >= 2 {
        let flaky_badge = if !ctx.flaky_scores.is_empty() {
            format!(
                r#" <span class="nav-badge badge-warn">{}</span>"#,
                ctx.flaky_scores.len()
            )
        } else {
            String::new()
        };
        let _ = write!(
            nav,
            "<a href=\"#section-flaky\"><span data-i18n=\"nav.flaky\">Flaky</span>{}</a>",
            flaky_badge
        );
    }
    if ctx.i18n_delta.is_some() {
        let missing_keys = ctx
            .i18n_delta
            .as_ref()
            .map(|i| i.missing_keys.len())
            .unwrap_or(0);
        if missing_keys > 0 {
            let _ = write!(
                nav,
                "<a href=\"#section-i18n\"><span data-i18n=\"nav.i18n\">i18n</span> <span class=\"nav-badge badge-warn\">{}</span></a>",
                missing_keys
            );
        } else {
            let _ = write!(
                nav,
                "<a href=\"#section-i18n\" data-i18n=\"nav.i18n\">i18n</a>"
            );
        }
    }
    if has_security {
        if security_issue_count > 0 {
            let _ = write!(
                nav,
                "<a href=\"#section-security\"><span data-i18n=\"nav.security\">Security</span> <span class=\"nav-badge badge-warn\">{}</span></a>",
                security_issue_count
            );
        } else {
            let _ = write!(
                nav,
                "<a href=\"#section-security\" data-i18n=\"nav.security\">Security</a>"
            );
        }
    }
    if !checks.is_empty() {
        let _ = write!(
            nav,
            "<a href=\"#section-time-budget\" data-i18n=\"nav.timeBudget\">Time Budget</a>"
        );
    }
    // -- ARTIFACTS group --
    let _ = write!(
        nav,
        "<div class=\"nav-group-label\" data-i18n=\"nav.artifacts\">Artifacts</div>"
    );
    let _ = write!(
        nav,
        "<a href=\"#section-artifacts\" data-i18n=\"nav.artifactsLink\">Artifacts</a>"
    );

    nav.push_str("</nav>");

    // Embedded report.json (for JS consumers and external tooling)
    // Escape `</` to `<\/` to prevent premature script tag closure (HTML5 standard fix).
    // `\/` is a valid JSON escape for `/`, so JSON.parse still works correctly.
    let report_script = if report_json.is_empty() {
        String::new()
    } else {
        let safe_json = report_json.replace("</", "<\\/");
        format!(
            r#"<script id="report-data" type="application/json">{}</script>"#,
            safe_json
        )
    };

    format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
    <meta charset="UTF-8">
    <meta name="viewport" content="width=device-width, initial-scale=1.0">
    <title>prview Report - {repo} - {target}</title>
    {favicon}
    <!-- fonts: system stack only, no external CDN dependency -->
    <style>{css}</style>
</head>
	<body>
	    <div class="container">
	        <div class="tier-one-stack top-section-anchor" id="section-overview">
	            {header}
	            {merge_decision_card}
	            {action_center}
	            <div class="tier-one-grid">
	                <div class="tier-one-rail">
	                    {regression_widget}
	                    {delta}
	                </div>
	                {files_summary}
	            </div>
	        </div>
	        <div class="layout-wrap">
	            {nav}
	            <div class="main-content">
	                {blockers}
	                {checks}
	                {files}
	                {regression_details}
	                {breaking}
	                {coverage}
	                {findings}
	                {statistics}
	                {artifacts}
	                {narrative}
	                {commits}
	                {ownership}
	                {trends}
	                {flaky}
	                {i18n}
	                {time_budget}
	                {security}
	            </div>
        </div>
        {brand_footer}
    </div>
    <!-- Diff modal -->
    <div class="diff-modal-overlay" id="diff-modal-overlay">
        <div class="diff-modal">
            <div class="diff-modal-header">
                <span class="diff-modal-title" id="diff-modal-title"></span>
                <div class="diff-modal-actions">
                    <input type="text" class="diff-modal-search" id="diff-modal-search" placeholder="Search in diff..." data-i18n-placeholder="placeholder.searchDiff" />
                    <button class="diff-modal-close" id="diff-copy-path" data-i18n="button.copyPath">Copy path</button>
                    <button class="diff-modal-close" id="diff-modal-close"><span aria-hidden="true">&#x2715;</span> <span data-i18n="button.close">Close</span></button>
                </div>
            </div>
            <div class="diff-modal-body" id="diff-modal-body"></div>
        </div>
    </div>
    {report_script}
    <script>{js}</script>
</body>
</html>"#,
        repo = escape_html(&repo_display),
        target = escape_html(target),
        favicon = super::BRAND_FAVICON_LINK_TAG,
        brand_footer = crate::artifacts::brand::mini_footer_html(),
        css = css_content,
        js = js_content,
        header = header,
        merge_decision_card = merge_decision_card,
        action_center = action_center,
        regression_widget = regression_widget_html,
        delta = delta_html,
        files_summary = files_summary_widget,
        nav = nav,
        narrative = narrative_html,
        blockers = blockers_html,
        time_budget = time_budget_html,
        security = security_html,
        trends = trends_html,
        flaky = flaky_html,
        checks = checks_html,
        files = files_html,
        ownership = ownership_html,
        breaking = breaking_html,
        coverage = coverage_html,
        findings = findings_html,
        i18n = i18n_html,
        commits = commits_html,
        statistics = statistics_html,
        regression_details = regression_details_html,
        artifacts = artifacts_html,
        report_script = report_script,
    )
}
