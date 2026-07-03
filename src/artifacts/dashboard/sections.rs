//! Dashboard section builders: every `build_*` widget/section of the HTML report.

use super::*;

// ---------------------------------------------------------------------------
// Section builders
// ---------------------------------------------------------------------------

pub(super) fn build_files_summary_widget(diff: Option<&Diff>) -> String {
    let breakdown = build_file_breakdown(diff);
    if breakdown.total == 0 {
        return String::new();
    }

    format!(
        r#"<div class="card files-summary-card">
    <div class="files-summary-header">
        <div>
            <div class="files-summary-title" data-i18n="section.filesChanged">Files Changed</div>
            <div class="files-summary-total">{total}</div>
        </div>
        <div class="stat-summary" style="text-align:right; font-family:var(--mono); font-size:12px; color:var(--muted)">
            <span class="green">+{adds}</span> / <span class="red">-{dels}</span>
        </div>
    </div>
    <div class="files-summary-breakdown">
        <div class="files-summary-stat"><strong>{code}</strong><span data-i18n="label.code">code</span></div>
        <div class="files-summary-stat"><strong>{tests}</strong><span data-i18n="label.tests">tests</span></div>
        <div class="files-summary-stat"><strong>{non_code}</strong><span data-i18n="label.nonCode">non-code</span></div>
    </div>
</div>"#,
        total = format_number(breakdown.total),
        adds = format_number(breakdown.additions),
        dels = format_number(breakdown.deletions),
        code = format_number(breakdown.code),
        tests = format_number(breakdown.tests),
        non_code = format_number(breakdown.non_code),
    )
}

pub(super) fn format_context_generated_at(run_id: &str) -> Option<String> {
    if run_id.len() >= 15 && run_id.as_bytes()[8] == b'-' {
        Some(format!(
            "{}-{}-{} {}:{}:{}",
            &run_id[0..4],
            &run_id[4..6],
            &run_id[6..8],
            &run_id[9..11],
            &run_id[11..13],
            &run_id[13..15]
        ))
    } else {
        None
    }
}

#[derive(Default)]
pub(super) struct HeaderContextData {
    repo: Option<String>,
    target: Option<String>,
    base: Option<String>,
    commit: Option<String>,
    generated_at: Option<String>,
    run_id: Option<String>,
}

pub(super) fn rfc3339_to_context_fields(value: &str) -> Option<(String, String)> {
    let dt = chrono::DateTime::parse_from_rfc3339(value).ok()?;
    Some((
        dt.format("%Y-%m-%d %H:%M:%S").to_string(),
        dt.format("%Y%m%d-%H%M%S").to_string(),
    ))
}

pub(super) fn context_from_run_json(dir: &Path) -> Option<HeaderContextData> {
    let json = std::fs::read_to_string(dir.join("00_summary").join("RUN.json")).ok()?;
    let parsed = serde_json::from_str::<serde_json::Value>(&json).ok()?;

    let repo = parsed
        .get("repo")
        .and_then(|r| r.get("gh_repo"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
        .map(ToString::to_string);
    let target = parsed
        .get("refs")
        .and_then(|r| r.get("target"))
        .and_then(|v| v.as_str())
        .map(ToString::to_string);
    let base = parsed
        .get("refs")
        .and_then(|r| r.get("bases"))
        .and_then(|v| v.as_array())
        .and_then(|bases| bases.first())
        .and_then(|base| base.get("name"))
        .and_then(|v| v.as_str())
        .map(ToString::to_string);
    let commit = parsed
        .get("refs")
        .and_then(|r| r.get("target_sha"))
        .and_then(|v| v.as_str())
        .map(|sha| short_sha(sha).to_string());
    let (generated_at, run_id) = parsed
        .get("run_finished_at")
        .and_then(|v| v.as_str())
        .and_then(rfc3339_to_context_fields)
        .unzip();

    Some(HeaderContextData {
        repo,
        target,
        base,
        commit,
        generated_at,
        run_id,
    })
}

pub(super) fn context_from_report_json(report_json: &str) -> Option<HeaderContextData> {
    let parsed = serde_json::from_str::<serde_json::Value>(report_json).ok()?;
    let repo = parsed
        .get("meta")
        .and_then(|m| m.get("repo"))
        .and_then(|r| r.get("full_name"))
        .and_then(|v| v.as_str())
        .map(ToString::to_string);
    let target = parsed
        .get("meta")
        .and_then(|m| m.get("range"))
        .and_then(|r| r.get("head"))
        .and_then(|v| v.as_str())
        .map(ToString::to_string);
    let base = parsed
        .get("meta")
        .and_then(|m| m.get("range"))
        .and_then(|r| r.get("base"))
        .and_then(|v| v.as_str())
        .map(ToString::to_string);
    let (generated_at, run_id) = parsed
        .get("meta")
        .and_then(|m| m.get("generated_at"))
        .and_then(|v| v.as_str())
        .and_then(rfc3339_to_context_fields)
        .unzip();

    Some(HeaderContextData {
        repo,
        target,
        base,
        commit: None,
        generated_at,
        run_id,
    })
}

pub(super) fn build_header(
    config: &Config,
    diff: Option<&Diff>,
    ctx: &DashboardContext,
    dir: &Path,
    report_json: &str,
) -> String {
    let target_fallback = diff.map(|d| d.target.as_str()).unwrap_or("N/A");
    let base_fallback = diff.map(|d| d.base.as_str()).unwrap_or("N/A");
    let run_context = context_from_run_json(dir).unwrap_or_default();
    let report_context = context_from_report_json(report_json).unwrap_or_default();
    let repo_display = run_context
        .repo
        .or(report_context.repo)
        .unwrap_or_else(|| config.gh_repo.clone().unwrap_or_else(|| config.repo_name()));
    let target = run_context
        .target
        .or(report_context.target)
        .unwrap_or_else(|| target_fallback.to_string());
    let base = run_context
        .base
        .or(report_context.base)
        .unwrap_or_else(|| base_fallback.to_string());
    let fallback_run_id = dir
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("current-run")
        .to_string();
    let generated_at = run_context
        .generated_at
        .or(report_context.generated_at)
        .unwrap_or_else(|| {
            format_context_generated_at(&fallback_run_id)
                .unwrap_or_else(|| chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string())
        });
    let run_id = run_context
        .run_id
        .or(report_context.run_id)
        .unwrap_or(fallback_run_id);
    let commit_short = run_context
        .commit
        .or_else(|| diff.map(|d| short_sha(&d.target_commit_id).to_string()))
        .unwrap_or_else(|| "N/A".to_string());
    let profile = config.profile.kind.as_str();

    let pr_badge_html = match (config.pr_number, config.pr_url.as_deref()) {
        (Some(num), Some(url)) => format!(
            r#"<span class="context-pill"><span class="context-pill-label" data-i18n="label.pullRequest">PR</span><a href="{url}">#{num}</a></span>"#,
            url = escape_html(url),
            num = num
        ),
        (Some(num), None) => format!(
            r#"<span class="context-pill"><span class="context-pill-label" data-i18n="label.pullRequest">PR</span><code>#{num}</code></span>"#,
            num = num
        ),
        _ => String::new(),
    };

    let decision = merge_decision_view(ctx);
    let has_review_caveats = !decision.review_caveats.is_empty();

    let (merge_class, merge_label) = if ctx.check_gates.is_empty() {
        ("merge-na", i18n_template("summary.none", "N/A", &[]))
    } else {
        let (key, fallback) = match decision.state {
            crate::artifacts::MergeDecisionState::Allow => {
                ("hero.mergeAllow", decision.state.hero_label())
            }
            crate::artifacts::MergeDecisionState::AllowWithReview => {
                ("hero.mergeAllowWithReview", decision.state.hero_label())
            }
            crate::artifacts::MergeDecisionState::Hold => {
                ("hero.mergeHold", decision.state.hero_label())
            }
            crate::artifacts::MergeDecisionState::Block => {
                ("hero.mergeBlock", decision.state.hero_label())
            }
        };
        (
            decision.state.hero_class(),
            i18n_template(key, fallback, &[]),
        )
    };

    // Quality badge next to merge chip
    let quality_badge = if ctx.check_gates.is_empty() {
        String::new()
    } else if ctx.quality_pass {
        format!(
            r#"<span class="badge badge-success">{}</span>"#,
            i18n_template("badge.qualityPass", "Quality: PASS", &[])
        )
    } else if ctx.policy_allow_merge {
        format!(
            r#"<span class="badge badge-warning" title="{} check(s) failed">{}</span>"#,
            ctx.quality_failures.len(),
            i18n_template("badge.qualityFail", "Quality: FAIL", &[])
        )
    } else {
        format!(
            r#"<span class="badge badge-error" title="{} check(s) failed">{}</span>"#,
            ctx.quality_failures.len(),
            i18n_template("badge.qualityFail", "Quality: FAIL", &[])
        )
    };

    let blocking_detail = if !ctx.blocking_issues.is_empty() {
        let items: String = ctx
            .blocking_issues
            .iter()
            .map(|i| {
                format!(
                    "<li style=\"font-size:11px;color:var(--block)\">{}</li>",
                    escape_html(i)
                )
            })
            .collect();
        format!(
            "<ul style=\"margin:2px 0 0 14px;list-style:disc\">{}</ul>",
            items
        )
    } else {
        String::new()
    };

    let policy_html = format!(
        r#"<div class="merge-policy">
    <div class="merge-policy-line">
        <span class="merge-policy-label" data-i18n="label.policyMode">Policy mode</span>
        <code class="merge-policy-value" data-policy-mode="{mode}">{mode}</code>
    </div>{blocking}
</div>"#,
        mode = escape_html(ctx.policy_mode),
        blocking = blocking_detail,
    );

    let review_note_html = if has_review_caveats {
        // Compact marker only: amber dot + muted count. The full caveat prose
        // lives in the Merge Decision card below (avoids duplicated noise in the
        // above-the-fold header). Amber is reserved for the small signal dot.
        let count = decision.review_caveats.len();
        format!(
            r#"<div class="merge-policy merge-policy-line review-signal-compact" style="margin-top:4px"><span class="review-signal-dot" aria-hidden="true"></span>{count_label}</div>"#,
            count_label = i18n_template(
                "summary.reviewSignalCount",
                &format!("{} review signals", count),
                &[("count", count.to_string())],
            ),
        )
    } else {
        String::new()
    };

    // File breakdown stats row
    let stats_html = {
        let breakdown = build_file_breakdown(diff);
        if breakdown.total > 0 {
            let stats_fallback = format!(
                "{} files | {} code | {} tests | {} non-code | +{} / -{}",
                breakdown.total,
                breakdown.code,
                breakdown.tests,
                breakdown.non_code,
                format_number(breakdown.additions),
                format_number(breakdown.deletions),
            );
            format!(
                r#"<div class="header-stats">{summary}</div>"#,
                summary = i18n_template(
                    "summary.headerStats",
                    &stats_fallback,
                    &[
                        ("total", breakdown.total.to_string()),
                        ("code", breakdown.code.to_string()),
                        ("test", breakdown.tests.to_string()),
                        ("other", breakdown.non_code.to_string()),
                        ("add", format_number(breakdown.additions)),
                        ("del", format_number(breakdown.deletions)),
                    ],
                ),
            )
        } else {
            String::new()
        }
    };

    format!(
        r#"<header class="header">
    <div class="header-left">
        <div class="header-title"><span class="wordmark">prview<span class="dot" aria-hidden="true"></span></span></div>
        <div class="dashboard-context">
            <div class="dashboard-context-repo">{repo}</div>
            <div class="dashboard-context-flow">
                <span class="context-pill">
                    <span class="context-pill-label" data-i18n="label.branch">Branch</span>
                    <span class="ref-arrow"><span class="ref-name">{target}</span></span>
                </span>
                <span class="dashboard-context-arrow" aria-hidden="true">&#x2192;</span>
                <span class="context-pill">
                    <span class="context-pill-label" data-i18n="label.base">Base</span>
                    <span class="ref-arrow"><span class="ref-name">{base}</span></span>
                </span>
            </div>
            <div class="dashboard-context-meta">
                <span class="context-pill">
                    <span class="context-pill-label" data-i18n="label.commit">Commit</span>
                    <code>{commit}</code>
                </span>
                <span class="context-pill">
                    <span class="context-pill-label" data-i18n="label.runId">Run</span>
                    <code>{run_id}</code>
                </span>
                <span class="context-pill">
                    <span class="context-pill-label" data-i18n="label.generated">Generated</span>
                    <code>{generated}</code>
                </span>
                {pr_badge}
            </div>
        </div>
        {stats}
    </div>
    <div class="header-right">
        <div class="header-actions">
            <button id="copy-pr-comment" class="header-action-btn" type="button" data-i18n="button.copyPrComment">Copy PR Comment</button>
            <div class="view-mode-toggle" role="group" aria-label="Dashboard view mode" data-i18n-aria-label="label.dashboardViewMode">
                <button id="view-mode-review" class="view-mode-btn active" type="button" aria-pressed="true" data-i18n="view.review">Review View</button>
                <button id="view-mode-author" class="view-mode-btn" type="button" aria-pressed="false" data-i18n="view.author">Author View</button>
            </div>
            <div class="lang-toggle" role="group" aria-label="Dashboard language" data-i18n-aria-label="label.dashboardLanguage">
                <button id="lang-toggle-en" class="lang-btn active" type="button" aria-pressed="true">EN</button>
                <button id="lang-toggle-pl" class="lang-btn" type="button" aria-pressed="false">PL</button>
            </div>
            <span class="badge badge-info header-profile-badge">{profile}</span>
        </div>
        <div class="merge-decision">
            <span class="merge-chip {mc}">{ml}</span>
            {qb}
            {policy}
            {review_note}
        </div>
    </div>
</header>"#,
        repo = escape_html(&repo_display),
        target = escape_html(&target),
        base = escape_html(&base),
        commit = escape_html(&commit_short),
        run_id = escape_html(&run_id),
        generated = escape_html(&generated_at),
        pr_badge = pr_badge_html,
        stats = stats_html,
        profile = escape_html(profile),
        mc = merge_class,
        ml = merge_label,
        qb = quality_badge,
        policy = policy_html,
        review_note = review_note_html,
    )
}

pub(super) fn merge_decision_view(ctx: &DashboardContext) -> crate::artifacts::MergeDecisionView {
    let review_caveats = if ctx.review_caveats.is_empty() {
        build_review_caveats(&ctx.breaking, &ctx.coverage, ctx.findings.len())
    } else {
        ctx.review_caveats.clone()
    };

    build_merge_decision_view(
        ctx.policy_allow_merge,
        ctx.quality_pass,
        ctx.recommended_merge,
        &ctx.quality_failures,
        &ctx.quality_failure_details,
        &ctx.blocking_issues,
        review_caveats,
    )
}

/// Build the delta comparison row (PRV-104).
/// Shows compact badges comparing current run vs previous run.
pub(super) fn build_delta_section(ctx: &DashboardContext, checks: &[CheckResult]) -> String {
    let Some(ref prev) = ctx.previous_run else {
        return String::new();
    };

    let passed_now = checks
        .iter()
        .filter(|c| c.status == CheckStatus::Passed)
        .count();
    let failed_now = checks
        .iter()
        .filter(|c| matches!(c.status, CheckStatus::Failed | CheckStatus::Error))
        .count();
    let findings_now = ctx.findings.len();
    let breaking_now = ctx.breaking.len();

    let mut badges = String::new();

    // Checks passed delta
    {
        let before = prev.checks_passed_before;
        let diff_val = passed_now as isize - before as isize;
        let (class, arrow) = if diff_val > 0 {
            ("delta-better", "+")
        } else if diff_val < 0 {
            ("delta-worse", "")
        } else {
            ("delta-same", "")
        };
        let _ = write!(
            badges,
            r#"<span class="delta-badge {cls}"><span class="delta-label" data-i18n="label.passed">Passed</span> {before}&#x2192;{now} ({arrow}{diff})</span>"#,
            cls = class,
            before = before,
            now = passed_now,
            arrow = arrow,
            diff = diff_val,
        );
    }

    // Checks failed delta
    {
        let before = prev.checks_failed_before;
        let diff_val = failed_now as isize - before as isize;
        let (class, arrow) = if diff_val < 0 {
            ("delta-better", "")
        } else if diff_val > 0 {
            ("delta-worse", "+")
        } else {
            ("delta-same", "")
        };
        let _ = write!(
            badges,
            r#"<span class="delta-badge {cls}"><span class="delta-label" data-i18n="label.failed">Failed</span> {before}&#x2192;{now} ({arrow}{diff})</span>"#,
            cls = class,
            before = before,
            now = failed_now,
            arrow = arrow,
            diff = diff_val,
        );
    }

    // Findings delta
    {
        let before = prev.findings_before;
        let diff_val = findings_now as isize - before as isize;
        let (class, arrow) = if diff_val < 0 {
            ("delta-better", "")
        } else if diff_val > 0 {
            ("delta-worse", "+")
        } else {
            ("delta-same", "")
        };
        let _ = write!(
            badges,
            r#"<span class="delta-badge {cls}"><span class="delta-label" data-i18n="label.findings">Findings</span> {before}&#x2192;{now} ({arrow}{diff})</span>"#,
            cls = class,
            before = before,
            now = findings_now,
            arrow = arrow,
            diff = diff_val,
        );
    }

    // Breaking changes delta
    if prev.breaking_before > 0 || breaking_now > 0 {
        let before = prev.breaking_before;
        let diff_val = breaking_now as isize - before as isize;
        let (class, arrow) = if diff_val < 0 {
            ("delta-better", "")
        } else if diff_val > 0 {
            ("delta-worse", "+")
        } else {
            ("delta-same", "")
        };
        let _ = write!(
            badges,
            r#"<span class="delta-badge {cls}"><span class="delta-label" data-i18n="section.breaking">Breaking</span> {before}&#x2192;{now} ({arrow}{diff})</span>"#,
            cls = class,
            before = before,
            now = breaking_now,
            arrow = arrow,
            diff = diff_val,
        );
    }

    // Quality pass status change
    {
        let (label, class) = match (prev.quality_pass_before, ctx.quality_pass) {
            (false, true) => ("FAIL&#x2192;PASS", "delta-better"),
            (true, false) => ("PASS&#x2192;FAIL", "delta-worse"),
            (true, true) => ("PASS&#x2192;PASS", "delta-same"),
            (false, false) => ("FAIL&#x2192;FAIL", "delta-same"),
        };
        let _ = write!(
            badges,
            r#"<span class="delta-badge {cls}"><span class="delta-label" data-i18n="label.quality">Quality</span> {label}</span>"#,
            cls = class,
            label = label,
        );
    }

    format!(
        r#"<div class="delta-row" id="section-delta"><span class="delta-label" style="font-weight:700" data-i18n="label.previousRun">vs Previous Run</span>: {badges}</div>"#,
        badges = badges,
    )
}

/// Build the historical trends section (PRV-201).
/// Only renders when there are at least 2 historical data points.
pub(super) fn build_trends_section(ctx: &DashboardContext) -> String {
    if ctx.run_history.len() < 2 {
        return String::new();
    }

    let history = &ctx.run_history;

    // Compute max values for bar scaling (avoid div-by-zero)
    let max_passed = history
        .iter()
        .map(|r| r.checks_passed)
        .max()
        .unwrap_or(1)
        .max(1);
    let max_failed = history
        .iter()
        .map(|r| r.checks_failed)
        .max()
        .unwrap_or(1)
        .max(1);
    let max_warned = history
        .iter()
        .map(|r| r.checks_warned)
        .max()
        .unwrap_or(1)
        .max(1);
    // Trend indicator: compare newest vs average of the rest
    let newest = &history[0];
    let older: Vec<&crate::artifacts::HistoricalRun> = history.iter().skip(1).collect();
    let avg_failed = if older.is_empty() {
        0.0
    } else {
        older.iter().map(|r| r.checks_failed as f64).sum::<f64>() / older.len() as f64
    };
    let avg_findings = if older.is_empty() {
        0.0
    } else {
        older.iter().map(|r| r.findings_count as f64).sum::<f64>() / older.len() as f64
    };

    let (trend_class, trend_label) = {
        let failed_improving = (newest.checks_failed as f64) < avg_failed;
        let findings_improving = (newest.findings_count as f64) < avg_findings;
        let failed_worsening = (newest.checks_failed as f64) > avg_failed;
        let findings_worsening = (newest.findings_count as f64) > avg_findings;

        let improving = failed_improving || findings_improving;
        let worsening = failed_worsening || findings_worsening;

        // When one signal improves while the other worsens, the older logic
        // (improving-first) hid the regression behind an "Improving" badge.
        // Report that conflict honestly as "Mixed".
        match (improving, worsening) {
            (true, true) => (
                "trend-mixed",
                r#"&#x2194; <span data-i18n="trend.mixed">Mixed</span>"#,
            ),
            (true, false) => (
                "trend-improving",
                r#"&#x2191; <span data-i18n="trend.improving">Improving</span>"#,
            ),
            (false, true) => (
                "trend-degrading",
                r#"&#x2193; <span data-i18n="trend.degrading">Degrading</span>"#,
            ),
            (false, false) => (
                "trend-stable",
                r#"&#x2194; <span data-i18n="trend.stable">Stable</span>"#,
            ),
        }
    };

    let mut rows = String::new();
    // History is newest-first; display newest at top
    for (i, run) in history.iter().enumerate() {
        let is_latest = i == 0;
        let row_class = if is_latest {
            "trends-row trends-row-latest"
        } else {
            "trends-row"
        };
        let quality_icon = if run.quality_pass {
            "&#x2713;"
        } else {
            "&#x2717;"
        };
        let quality_class = if run.quality_pass {
            "trends-pass"
        } else {
            "trends-fail"
        };

        // Format timestamp: "20260302-143022" -> "03-02 14:30"
        let display_ts = format_run_timestamp(&run.timestamp);

        // Bar widths as percentage of max
        let passed_pct = (run.checks_passed as f64 / max_passed as f64 * 100.0) as u32;
        let failed_pct = (run.checks_failed as f64 / max_failed as f64 * 100.0) as u32;
        let warned_pct = (run.checks_warned as f64 / max_warned as f64 * 100.0) as u32;
        let latest_badge = if is_latest {
            " <span class=\"trends-latest-badge\" data-i18n=\"label.latest\">latest</span>"
        } else {
            ""
        };

        let _ = write!(
            rows,
            r#"<tr class="{row_class}">
<td class="trends-ts">{display_ts}{latest_badge}</td>
<td><span class="trends-bar trends-bar-pass" style="width:{passed_pct}%"></span> <span class="trends-val">{passed}</span></td>
<td><span class="trends-bar trends-bar-fail" style="width:{failed_pct}%"></span> <span class="trends-val">{failed}</span></td>
<td><span class="trends-bar trends-bar-warn" style="width:{warned_pct}%"></span> <span class="trends-val">{warned}</span></td>
<td class="{quality_class}">{quality_icon}</td>
<td><span class="trends-val">{findings}</span></td>
</tr>"#,
            row_class = row_class,
            display_ts = display_ts,
            latest_badge = latest_badge,
            passed_pct = passed_pct,
            passed = run.checks_passed,
            failed_pct = failed_pct,
            failed = run.checks_failed,
            warned_pct = warned_pct,
            warned = run.checks_warned,
            quality_class = quality_class,
            quality_icon = quality_icon,
            findings = run.findings_count,
        );
    }

    format!(
        r#"<div class="section" id="section-trends">
<div class="section-header">
    <span class="section-title" data-i18n="section.historicalTrends">Historical Trends</span>
    <span><span class="section-count">{count_label}</span> <span class="trends-indicator {trend_class}">{trend_label}</span></span>
</div>
<div class="trends-table-wrap">
<table class="trends-table">
<thead><tr>
    <th data-i18n="label.run">Run</th><th data-i18n="label.passed">Passed</th><th data-i18n="label.failed">Failed</th><th data-i18n="label.warned">Warned</th><th data-i18n="label.quality">Quality</th><th data-i18n="label.findings">Findings</th>
</tr></thead>
<tbody>{rows}</tbody>
</table>
</div>
</div>"#,
        count_label = i18n_template(
            "count.runs",
            &format!("{} runs", history.len()),
            &[("count", history.len().to_string())],
        ),
        trend_class = trend_class,
        trend_label = trend_label,
        rows = rows,
    )
}

/// PRV-204: Build the flaky checks section.
///
/// Shows checks that have inconsistent statuses across historical runs.
/// Hidden if fewer than 2 historical runs exist or no flaky checks found.
pub(super) fn build_flaky_section(ctx: &DashboardContext) -> String {
    // Need at least 2 runs for any meaningful flaky data
    if ctx.run_history.len() < 2 {
        return String::new();
    }

    // If no flaky checks, show a stable card
    if ctx.flaky_scores.is_empty() {
        return format!(
            r#"<div class="section flaky-section" id="section-flaky">
<div class="section-header">
    <span class="section-title" data-i18n="section.flakyChecks">Flaky Checks</span>
</div>
<div class="flaky-stable-card">&#x2713; {stable}</div>
</div>"#,
            stable = i18n_template(
                "message.allChecksStableAcrossRuns",
                &format!("All checks stable across {} runs", ctx.run_history.len()),
                &[("count", ctx.run_history.len().to_string())],
            ),
        );
    }

    let mut rows = String::new();
    for score in &ctx.flaky_scores {
        // Score color class
        let score_class = if score.flaky_score >= 0.5 {
            "flaky-score-high"
        } else if score.flaky_score >= 0.25 {
            "flaky-score-mid"
        } else {
            "flaky-score-low"
        };

        // Sparkline dots (newest first)
        let mut sparkline = String::new();
        for status in &score.last_statuses {
            let dot_class = match status.as_str() {
                "PASS" => "flaky-dot-pass",
                "FAIL" => "flaky-dot-fail",
                "WARN" => "flaky-dot-warn",
                "ERROR" => "flaky-dot-error",
                _ => "flaky-dot-skip",
            };
            let _ = write!(
                sparkline,
                r#"<span class="flaky-dot {}" title="{}"></span>"#,
                dot_class,
                escape_html(status)
            );
        }

        // Confidence badge
        let conf_class = format!("flaky-confidence-{}", score.confidence);
        let confidence = match score.confidence {
            "high" => r#"<span data-i18n="label.high">high</span>"#.to_string(),
            "medium" => r#"<span data-i18n="label.medium">medium</span>"#.to_string(),
            "low" => r#"<span data-i18n="label.low">low</span>"#.to_string(),
            other => escape_html(other),
        };

        let pct = (score.flaky_score * 100.0) as u32;

        let _ = write!(
            rows,
            r#"<tr data-check-id="{check_id}">
<td>{name}</td>
<td><span class="flaky-score {score_class}">{pct}%</span></td>
<td><span class="flaky-sparkline">{sparkline}</span></td>
<td>{transitions}/{max_transitions}</td>
<td><span class="flaky-confidence {conf_class}">{confidence}</span></td>
<td>{total_runs}</td>
</tr>"#,
            check_id = escape_html(&score.check_id),
            name = escape_html(&score.check_name),
            score_class = score_class,
            pct = pct,
            sparkline = sparkline,
            transitions = score.transitions,
            max_transitions = score.total_runs.saturating_sub(1),
            conf_class = conf_class,
            confidence = confidence,
            total_runs = score.total_runs,
        );
    }

    let flaky_count = ctx.flaky_scores.len();

    format!(
        r#"<div class="section flaky-section" id="section-flaky">
<div class="section-header">
    <span class="section-title" data-i18n="section.flakyChecks">Flaky Checks</span>
    <span class="section-count">{count_label}</span>
</div>
<div class="flaky-table-wrap">
<table class="flaky-table">
<thead><tr>
    <th data-i18n="label.checks">Checks</th><th data-i18n="label.flakyScore">Flaky Score</th><th data-i18n="label.history">History</th><th data-i18n="label.transitions">Transitions</th><th data-i18n="label.confidence">Confidence</th><th data-i18n="label.runs">Runs</th>
</tr></thead>
<tbody>{rows}</tbody>
</table>
</div>
</div>"#,
        count_label = i18n_template(
            "count.flaky",
            &format!("{} flaky", flaky_count),
            &[("count", flaky_count.to_string())],
        ),
        rows = rows,
    )
}

/// Format a timestamp directory name into a human-readable short form.
/// "20260302-143022" -> "03-02 14:30"
/// Falls back to the raw string on any parse issue.
pub(super) fn format_run_timestamp(ts: &str) -> String {
    // Expected format: YYYYMMDD-HHMMSS (15 chars)
    if ts.len() >= 15 && ts.as_bytes()[8] == b'-' {
        let month = &ts[4..6];
        let day = &ts[6..8];
        let hour = &ts[9..11];
        let min = &ts[11..13];
        format!("{}-{} {}:{}", month, day, hour, min)
    } else {
        ts.to_string()
    }
}

/// PRV-206: Compute confidence level for a heuristic finding.
///
/// - Dead exports and cycles are deterministic (high confidence),
///   but downgrade to medium if >10 items (noise risk).
/// - Twins/unused symbols are similarity-based (medium confidence),
///   downgrade to low if >10 items.
#[cfg_attr(not(test), allow(dead_code))]
pub(super) fn heuristic_confidence(kind: &str, count: usize) -> &'static str {
    match kind {
        "dead_exports" | "cycles" => {
            if count > 10 {
                "medium"
            } else {
                "high"
            }
        }
        "twins" | "dead_parrots" | "unused_symbols" => {
            if count > 10 {
                "low"
            } else {
                "medium"
            }
        }
        _ => "low",
    }
}

pub(super) fn build_action_center(
    ctx: &DashboardContext,
    checks: &[CheckResult],
    heuristics: Option<&HeuristicsResult>,
) -> String {
    let mut cards = String::new();
    let mut chips = String::new();

    let push_card = |cards: &mut String,
                     href: &str,
                     card_class: &str,
                     title_html: String,
                     value_html: String,
                     meta_html: String| {
        let _ = write!(
            cards,
            r#"<a class="action-card {card_class}" href="{href}" style="text-decoration:none;color:inherit">
    <div class="signal-card-title">{title}</div>
    <div class="signal-card-value">{value}</div>
    <div class="signal-card-meta">{meta}</div>
</a>"#,
            href = href,
            card_class = card_class,
            title = title_html,
            value = value_html,
            meta = meta_html,
        );
    };

    let push_chip = |chips: &mut String, href: &str, chip_class: &str, label_html: String| {
        let _ = write!(
            chips,
            r#"<a class="action-chip {chip_class}" href="{href}">{label}</a>"#,
            chip_class = chip_class,
            href = href,
            label = label_html,
        );
    };

    if !checks.is_empty() {
        let failed_count = checks
            .iter()
            .filter(|c| matches!(c.status, CheckStatus::Failed | CheckStatus::Error))
            .count();
        let warn_count = checks
            .iter()
            .filter(|c| c.status == CheckStatus::Warnings)
            .count();
        let passed_count = checks
            .iter()
            .filter(|c| c.status == CheckStatus::Passed)
            .count();

        if failed_count > 0 {
            push_card(
                &mut cards,
                "#section-checks",
                "alert-error",
                r#"<span data-i18n="section.checks">Checks</span>"#.to_string(),
                i18n_template(
                    "count.failing",
                    &format!("{} failing", failed_count),
                    &[("count", failed_count.to_string())],
                ),
                i18n_template("message.openChecksFirst", "Open checks first", &[]),
            );
        } else if warn_count > 0 {
            push_card(
                &mut cards,
                "#section-checks",
                "alert-warning",
                r#"<span data-i18n="section.checks">Checks</span>"#.to_string(),
                i18n_template(
                    "count.warnings",
                    &format!("{} warnings", warn_count),
                    &[("count", warn_count.to_string())],
                ),
                i18n_template(
                    "message.reviewNoisyChecksBeforeMerge",
                    "Review noisy checks before merge",
                    &[],
                ),
            );
        } else {
            push_chip(
                &mut chips,
                "#section-checks",
                "chip-ok",
                format!(
                    r#"<span class="chip-ok-check">&#x2713;</span> {}"#,
                    i18n_template(
                        "chip.checksOk",
                        "Checks OK ({passed}/{total})",
                        &[
                            ("passed", passed_count.to_string()),
                            ("total", checks.len().to_string()),
                        ],
                    )
                ),
            );
        }
    }

    if !ctx.breaking.is_empty() {
        let high_risk = ctx
            .breaking
            .iter()
            .filter(|f| f.risk_level == BreakingRisk::High)
            .count();
        push_card(
            &mut cards,
            "#section-breaking",
            if high_risk > 0 {
                "alert-error"
            } else {
                "alert-warning"
            },
            r#"<span data-i18n="section.breaking">Breaking</span>"#.to_string(),
            i18n_template(
                "count.changes",
                &format!("{} changes", ctx.breaking.len(),),
                &[("count", ctx.breaking.len().to_string())],
            ),
            if high_risk > 0 {
                i18n_template(
                    "message.publicApiImpactDetected",
                    "Public API / env impact detected",
                    &[],
                )
            } else {
                i18n_template(
                    "message.verifyCompatibilityBeforeMerge",
                    "Verify compatibility before merge",
                    &[],
                )
            },
        );
    } else {
        push_chip(
            &mut chips,
            "#section-breaking",
            "chip-ok",
            format!(
                r#"<span class="chip-ok-check">&#x2713;</span> {}"#,
                i18n_template("chip.breakingClear", "Breaking: 0", &[])
            ),
        );
    }

    let cov = &ctx.coverage;
    if cov.total_source > 0 || cov.non_code_count > 0 {
        if cov.pct < 80 {
            push_card(
                &mut cards,
                "#section-coverage",
                if cov.pct < 50 {
                    "alert-error"
                } else {
                    "alert-warning"
                },
                r#"<span data-i18n="section.coverage">Coverage</span>"#.to_string(),
                escape_html(&format!("{}%", cov.pct)),
                i18n_template(
                    "message.changedCodeWithoutMatchingTests",
                    "Changed code without matching tests",
                    &[],
                ),
            );
        } else {
            push_chip(
                &mut chips,
                "#section-coverage",
                "chip-ok",
                format!(
                    r#"<span class="chip-ok-check">&#x2713;</span> {}"#,
                    i18n_template(
                        "chip.coverageOk",
                        "Coverage: {pct}%",
                        &[("pct", cov.pct.to_string())],
                    )
                ),
            );
        }
    }

    if let Some(h) = heuristics
        && let Some(ref loctree) = h.loctree
        && loctree.available
    {
        let total_findings = loctree.dead_exports.len()
            + loctree.cycles.len()
            + loctree.twins.exact_twins.len()
            + loctree.twins.dead_parrots.len();
        if total_findings > 0 {
            push_card(
                &mut cards,
                "#section-statistics",
                "alert-warning",
                r#"<span data-i18n="section.heuristics">Heuristics</span>"#.to_string(),
                i18n_template(
                    "count.signals",
                    &format!("{} signals", total_findings),
                    &[("count", total_findings.to_string())],
                ),
                i18n_template(
                    "message.structureChangedNotably",
                    "Structure changed in notable ways",
                    &[],
                ),
            );
        } else {
            push_chip(
                &mut chips,
                "#section-statistics",
                "chip-ok",
                format!(
                    r#"<span class="chip-ok-check">&#x2713;</span> {}"#,
                    i18n_template("chip.heuristicsOk", "Heuristics OK", &[])
                ),
            );
        }
    }

    if !ctx.findings.is_empty() {
        push_card(
            &mut cards,
            "#section-findings",
            "alert-warning",
            r#"<span data-i18n="section.findings">Findings</span>"#.to_string(),
            i18n_template(
                "count.inline",
                &format!("{} inline", ctx.findings.len()),
                &[("count", ctx.findings.len().to_string())],
            ),
            i18n_template(
                "message.analysisFlaggedChangedLines",
                "Lint / analysis flagged changed lines",
                &[],
            ),
        );
    } else if !checks.is_empty() {
        push_chip(
            &mut chips,
            "#section-findings",
            "chip-ok",
            format!(
                r#"<span class="chip-ok-check">&#x2713;</span> {}"#,
                i18n_template("chip.findingsClear", "Findings: 0", &[])
            ),
        );
    }

    if !checks.is_empty() || !ctx.check_gates.is_empty() {
        if ctx.blocking_issues.is_empty() {
            push_chip(
                &mut chips,
                "#section-artifacts",
                "chip-ok",
                format!(
                    r#"<span class="chip-ok-check">&#x2713;</span> {}"#,
                    i18n_template("chip.sanityOk", "Sanity OK", &[])
                ),
            );
        } else {
            push_card(
                &mut cards,
                "#section-artifacts",
                "alert-warning",
                i18n_template("label.artifactPack", "Artifact Pack", &[]),
                i18n_template(
                    "count.issues",
                    &format!("{} issues", ctx.blocking_issues.len()),
                    &[("count", ctx.blocking_issues.len().to_string())],
                ),
                i18n_template(
                    "message.artifactPackNeedsLook",
                    "Artifact pack needs a quick look",
                    &[],
                ),
            );
        }
    }

    if cards.is_empty() && chips.is_empty() {
        return String::new();
    }

    format!(
        r#"<div class="action-row">{cards_html}<div class="signal-chips">{chips}</div></div>"#,
        cards_html = if cards.is_empty() {
            String::new()
        } else {
            format!(r#"<div class="signal-cards">{cards}</div>"#)
        },
        chips = chips,
    )
}

pub(super) fn build_regression_score_widget(
    regression: Option<&crate::regression::RegressionReport>,
) -> String {
    let Some(reg) = regression else {
        return String::new();
    };

    let score = reg.score.score;
    let severity_str = reg.score.severity.as_str();
    let severity_class = match reg.score.severity {
        crate::regression::score::Severity::OK => "severity-ok",
        crate::regression::score::Severity::LOW => "severity-low",
        crate::regression::score::Severity::MED => "severity-med",
        crate::regression::score::Severity::HIGH => "severity-high",
        crate::regression::score::Severity::CRITICAL => "severity-crit",
    };

    let mut html = String::new();
    let _ = write!(
        html,
        "<div class=\"regression-widget\">\
    <div class=\"regression-widget-header\">\
        <span style=\"font-size:15px;font-weight:600\" data-i18n=\"section.regression\">Regression</span>\
        <div>\
            <span class=\"severity-badge {sev_cls}\" data-regression-severity=\"{sev}\">{sev}</span>\
            <span style=\"font-family:var(--mono);font-size:22px;font-weight:700;margin-left:10px\">{score}</span>\
            <span style=\"color:var(--faint);font-size:13px\">/100</span>\
        </div>\
    </div>",
        sev_cls = severity_class,
        sev = escape_html(severity_str),
        score = score,
    );

    // Score reasons
    if !reg.score.score_reasons.is_empty() {
        html.push_str("<ul class=\"regression-reasons\">");
        for reason in &reg.score.score_reasons {
            let _ = write!(
                html,
                r#"<li data-regression-reason="{}">{}</li>"#,
                escape_html(reason),
                escape_html(reason)
            );
        }
        html.push_str("</ul>");
    }

    // Top hotspots
    let hotspots = &reg.diff.top_hotspots;
    if !hotspots.is_empty() {
        html.push_str("<table class=\"risk-table\"><tbody>");
        for entry in hotspots.iter().take(3) {
            let _ = write!(
                html,
                "<tr><td><a href=\"#file-{id}\">{file}</a></td><td style=\"color:var(--faint)\">{churn} churn</td></tr>",
                id = safe_id(&entry.file),
                file = escape_html(&entry.file),
                churn = entry.churn,
            );
        }
        html.push_str("</tbody></table>");
    }

    html.push_str(
        "<div class=\"regression-mini-note\" data-i18n=\"message.seeFullRegressionSection\">See the full Regression section for heuristics changes and deeper hotspot context.</div>",
    );

    html.push_str("</div>");
    html
}

pub(super) fn build_checks_section(checks: &[CheckResult], ctx: &DashboardContext) -> String {
    if checks.is_empty() {
        return String::new();
    }

    let any_failed = checks
        .iter()
        .any(|c| matches!(c.status, CheckStatus::Failed | CheckStatus::Error));

    let mut rows = String::new();
    for (idx, check) in checks.iter().enumerate() {
        let status_str = check.status.as_str();
        let has_output = !check.output.trim().is_empty();
        let is_problem = matches!(
            check.status,
            CheckStatus::Failed | CheckStatus::Warnings | CheckStatus::Error
        );
        let expandable_class = if has_output { " expandable" } else { "" };

        let cached = if check.cached {
            format!(" {}", i18n_template("label.cached", "(cached)", &[]),)
        } else {
            String::new()
        };
        let duration = format!("{:.1}s{}", check.duration.as_secs_f32(), cached);

        let arrow_html = if has_output {
            let open_class = if is_problem { " open" } else { "" };
            format!(
                r#"<span class="check-expand{oc}">&#x25B6;</span>"#,
                oc = open_class
            )
        } else {
            r#"<span class="check-expand"></span>"#.to_string()
        };

        // Blocking badge + class/severity from gate info
        let gate = ctx.check_gates.iter().find(|g| g.name == check.name);
        let blocking_html = match gate {
            Some(g) if g.blocking => format!(
                r#" <span class="badge badge-blocking" data-i18n="status.blocking">BLOCKING</span> <span class="badge badge-muted" style="font-size:9px">{}/{}</span>"#,
                g.class, g.severity
            ),
            Some(g) => format!(
                r#" <span class="badge badge-nonblocking" data-i18n="status.nonBlocking">NON-BLOCKING</span> <span class="badge badge-muted" style="font-size:9px">{}/{}</span>"#,
                g.class, g.severity
            ),
            None => String::new(),
        };
        let blocking_html = blocking_html.as_str();

        // Provenance info
        let prov_html = if let Some(ref prov) = check.provenance {
            let mut parts = Vec::new();
            if !prov.command.is_empty() {
                parts.push(format!("<code>{}</code>", escape_html(&prov.command)));
            }
            if let Some(exit) = prov.exit_code {
                parts.push(format!("exit: {}", exit));
            }
            if !prov.hard_fail_signatures.is_empty() {
                for sig in &prov.hard_fail_signatures {
                    parts.push(format!(r#"<span class="badge badge-error" style="font-size:9px">HARD FAIL: {}</span>"#, escape_html(sig)));
                }
            }
            if parts.is_empty() {
                String::new()
            } else {
                format!(
                    r#"<div class="check-meta" style="margin-top:4px;font-size:11px;color:var(--faint)">{}</div>"#,
                    parts.join(" ")
                )
            }
        } else {
            String::new()
        };

        let check_anchor = gate.map(|g| g.id.as_str()).unwrap_or("");

        let _ = write!(
            rows,
            r#"<tr class="check-row{ec}" data-check-id="{idx}" id="check-{anchor}">
    <td><div class="check-name-cell">{arrow}<span class="check-icon {skey}">{icon}</span> {name}{blocking}</div>{prov}</td>
    <td><span class="badge {bc}">{status}</span></td>
    <td style="text-align:right; font-family:var(--mono); font-size:12px; color:var(--muted)">{dur}</td>
</tr>"#,
            ec = expandable_class,
            idx = idx,
            anchor = escape_html(check_anchor),
            arrow = arrow_html,
            skey = check.status.as_str(),
            icon = check_icon(check.status),
            name = escape_html(&check.name),
            blocking = blocking_html,
            prov = prov_html,
            bc = check_badge_class(check.status),
            status = escape_html(status_str),
            dur = escape_html(&duration),
        );

        if has_output {
            let open_class = if is_problem { " open" } else { "" };
            let _ = write!(
                rows,
                r#"<tr class="check-output{oc}" id="check-output-{idx}"><td colspan="3"><div class="check-output-inner"><pre>{output}</pre></div></td></tr>"#,
                oc = open_class,
                idx = idx,
                output = escape_html(check.output.trim()),
            );
        }
    }

    let passed = checks
        .iter()
        .filter(|c| c.status == CheckStatus::Passed)
        .count();
    let warned = checks
        .iter()
        .filter(|c| c.status == CheckStatus::Warnings)
        .count();
    let failed = checks
        .iter()
        .filter(|c| matches!(c.status, CheckStatus::Failed | CheckStatus::Error))
        .count();
    let skipped = checks
        .iter()
        .filter(|c| c.status == CheckStatus::Skipped)
        .count();
    let expand_btn = if any_failed {
        r#" <button id="expand-all-failed" class="badge badge-muted" style="cursor:pointer;border:none;font:inherit" data-i18n="button.expandAll">Expand all</button>"#
    } else {
        ""
    };

    let checks_summary = i18n_template(
        "summary.checksDetailed",
        &format!(
            "{} passed, {} warned, {} failed, {} skipped",
            passed, warned, failed, skipped
        ),
        &[
            ("passed", passed.to_string()),
            ("warn", warned.to_string()),
            ("failed", failed.to_string()),
            ("skipped", skipped.to_string()),
        ],
    );

    // Render skipped checks footer
    let skipped_html = if ctx.skipped_checks.is_empty() {
        String::new()
    } else {
        let mut items = String::new();
        for sc in &ctx.skipped_checks {
            let _ = write!(
                items,
                "<span style=\"display:inline-flex;align-items:center;gap:4px;padding:2px 8px;background:var(--surface-2);border-radius:var(--radius-sm);font-size:11px;font-family:var(--mono)\">\
                <span class=\"badge badge-muted\" style=\"font-size:9px\">SKIP</span> {} <span style=\"color:var(--faint)\">— {}</span></span>",
                escape_html(&sc.name),
                escape_html(&sc.reason),
            );
        }
        format!(
            r#"<div style="padding:10px 16px;border-top:1px solid var(--line);font-size:12px;color:var(--muted)"><strong><span data-i18n="label.skipped">Skipped</span> ({count}):</strong> <div style="display:flex;flex-wrap:wrap;gap:6px;margin-top:6px">{items}</div></div>"#,
            count = ctx.skipped_checks.len(),
            items = items,
        )
    };

    format!(
        r#"<div class="section section-noise" id="section-checks">
    <div class="section-header">
        <span class="section-title" data-i18n="section.checks">Checks</span>
        <span>{checks_summary}{expand_btn}</span>
    </div>
    <div class="card" style="padding:0; overflow:hidden">
        <table class="checks-table">
            <thead><tr><th data-i18n="label.checks">Checks</th><th data-i18n="label.status">Status</th><th style="text-align:right" data-i18n="label.duration">Duration</th></tr></thead>
            <tbody>{rows}</tbody>
        </table>
        {skipped}
    </div>
</div>"#,
        checks_summary = checks_summary,
        expand_btn = expand_btn,
        rows = rows,
        skipped = skipped_html,
    )
}

pub(super) fn build_files_section(diff: Option<&Diff>, ctx: &DashboardContext) -> String {
    let Some(diff) = diff else {
        return String::new();
    };
    if diff.files.is_empty() {
        return String::new();
    }

    // Count statuses for filter chips
    let count_a = diff
        .files
        .iter()
        .filter(|f| f.status == FileStatus::Added)
        .count();
    let count_m = diff
        .files
        .iter()
        .filter(|f| f.status == FileStatus::Modified)
        .count();
    let count_d = diff
        .files
        .iter()
        .filter(|f| f.status == FileStatus::Deleted)
        .count();
    let count_r = diff
        .files
        .iter()
        .filter(|f| f.status == FileStatus::Renamed)
        .count();
    let count_c = diff
        .files
        .iter()
        .filter(|f| f.status == FileStatus::Copied)
        .count();

    let breakdown = build_file_breakdown(Some(diff));
    let count_code = breakdown.code;
    let count_test = breakdown.tests;
    let count_other = breakdown.non_code;

    // Group files by directory
    let mut groups: BTreeMap<String, Vec<&FileChange>> = BTreeMap::new();
    for file in &diff.files {
        let dir = dir_of(&file.path).to_string();
        groups.entry(dir).or_default().push(file);
    }

    for files in groups.values_mut() {
        files.sort_by_key(|file| std::cmp::Reverse(file.additions + file.deletions));
    }

    let mut sorted_groups: Vec<(String, Vec<&FileChange>)> = groups.into_iter().collect();
    sorted_groups.sort_by(|a, b| {
        let churn_b: usize = b.1.iter().map(|f| f.additions + f.deletions).sum();
        let churn_a: usize = a.1.iter().map(|f| f.additions + f.deletions).sum();
        churn_b.cmp(&churn_a)
    });

    let max_churn: usize = diff
        .files
        .iter()
        .map(|f| f.additions + f.deletions)
        .max()
        .unwrap_or(1)
        .max(1);

    let mut html = String::new();
    for (dir_name, files) in &sorted_groups {
        let dir_adds: usize = files.iter().map(|f| f.additions).sum();
        let dir_dels: usize = files.iter().map(|f| f.deletions).sum();
        let dir_churn: usize = dir_adds + dir_dels;
        let file_count = files.len();

        let _ = write!(
            html,
            r#"<div class="dir-group" data-churn="{dir_churn}">
<div class="dir-header">
    <span class="dir-chevron open">&#x25B6;</span>
    <span class="dir-name">{dir}/</span>
    <span class="dir-stats">
        <span>{file_count}</span>
        <span style="color:var(--pass)">+{adds}</span>
        <span style="color:var(--block)">-{dels}</span>
    </span>
</div>
<div class="dir-files-wrap">"#,
            dir_churn = dir_churn,
            dir = escape_html(dir_name),
            file_count = i18n_template(
                "count.files",
                &format!("{file_count} files"),
                &[("count", file_count.to_string())],
            ),
            adds = format_number(dir_adds),
            dels = format_number(dir_dels),
        );

        for file in files {
            let sc = status_char(file.status);
            let fname = filename_of(&file.path);
            let churn = file.additions + file.deletions;
            let is_hotspot = churn >= 80;

            let total_pct = if max_churn > 0 {
                ((churn as f64 / max_churn as f64) * 100.0).min(100.0)
            } else {
                0.0
            };
            let add_pct = if churn > 0 {
                (file.additions as f64 / churn as f64) * total_pct
            } else {
                0.0
            };
            let del_pct = total_pct - add_pct;

            let hotspot_html = if is_hotspot {
                r#" <span class="badge badge-hotspot" data-i18n="label.hotspot">HOTSPOT</span>"#
            } else {
                ""
            };

            let owner_badge = ctx
                .ownership_map
                .iter()
                .find(|(p, _)| p == &file.path)
                .map(|(_, owner)| {
                    format!(
                        r#" <span class="badge badge-muted" style="font-size:10px">{}</span>"#,
                        escape_html(owner),
                    )
                })
                .unwrap_or_default();

            let matched_patch = find_per_file_diff(&file.path, &ctx.per_file_diff_files);
            let diff_class = if matched_patch.is_some() {
                " has-diff"
            } else {
                ""
            };
            let file_sid = safe_id(&file.path);
            let patch_attr = if let Some(patch_name) = matched_patch {
                format!(
                    r#" data-patch-path="10_diff/per-file-diffs/{}""#,
                    escape_html(patch_name),
                )
            } else {
                String::new()
            };

            let name_content = if let Some(patch_name) = matched_patch {
                format!(
                    r#"<a href="10_diff/per-file-diffs/{patch}" target="_blank" title="View diff" data-i18n-title="message.viewDiff">{fname}</a>"#,
                    patch = escape_html(patch_name),
                    fname = escape_html(fname),
                )
            } else {
                escape_html(fname)
            };

            let category = file_category(&file.path);

            let _ = write!(
                html,
                r#"<div class="file-row{diff_class}" id="file-{file_sid}" data-path="{full_path}" data-status="{sc}" data-churn="{churn}" data-category="{category}"{patch_attr}>
    <span class="file-badge {sc}">{sc}</span>
    <span class="file-name" title="{full_path}">{name_content}{hotspot}{owner}</span>
    <span class="diff-bar"><span class="diff-bar-add" style="width:{apct:.1}%"></span><span class="diff-bar-del" style="width:{dpct:.1}%"></span></span>
    <span class="file-adds">+{adds}</span>
    <span class="file-dels">-{dels}</span>
</div>"#,
                diff_class = diff_class,
                file_sid = file_sid,
                full_path = escape_html(&file.path),
                sc = sc,
                category = category,
                patch_attr = patch_attr,
                name_content = name_content,
                hotspot = hotspot_html,
                owner = owner_badge,
                apct = add_pct,
                dpct = del_pct,
                churn = churn,
                adds = format_number(file.additions),
                dels = format_number(file.deletions),
            );
        }

        html.push_str("</div></div>");
    }

    let total_files = breakdown.total;

    // Build filter chips
    let mut chips = String::new();
    if count_a > 0 {
        let _ = write!(
            chips,
            r#"<span class="filter-chip" data-status="A">A ({count_a})</span>"#
        );
    }
    if count_m > 0 {
        let _ = write!(
            chips,
            r#"<span class="filter-chip" data-status="M">M ({count_m})</span>"#
        );
    }
    if count_d > 0 {
        let _ = write!(
            chips,
            r#"<span class="filter-chip" data-status="D">D ({count_d})</span>"#
        );
    }
    if count_r > 0 {
        let _ = write!(
            chips,
            r#"<span class="filter-chip" data-status="R">R ({count_r})</span>"#
        );
    }
    if count_c > 0 {
        let _ = write!(
            chips,
            r#"<span class="filter-chip" data-status="C">C ({count_c})</span>"#
        );
    }
    chips.push_str(r#"<span class="filter-chip" data-filter="hotspot">Hotspots</span>"#);
    if count_r > 0 {
        chips.push_str(
            r#"<span class="filter-chip" data-filter="hide-renames" data-i18n="label.hideRenames">Hide renames</span>"#,
        );
    }

    // B7: Noise filter toggles
    let has_assets = diff.files.iter().any(|f| file_category(&f.path) == "asset");
    let has_i18n = diff.files.iter().any(|f| file_category(&f.path) == "i18n");
    let has_config = diff
        .files
        .iter()
        .any(|f| file_category(&f.path) == "config");
    if has_assets {
        chips.push_str(r#"<span class="filter-chip" data-filter="hide-assets" data-i18n="label.hideAssets">Hide assets</span>"#);
    }
    if has_i18n {
        chips.push_str(r#"<span class="filter-chip" data-filter="hide-i18n" data-i18n="label.hideI18n">Hide i18n</span>"#);
    }
    if has_config {
        chips.push_str(r#"<span class="filter-chip" data-filter="hide-config" data-i18n="label.hideConfig">Hide config</span>"#);
    }

    // Code-first hotspots mini-list
    let mut hotspots: Vec<(&str, usize)> = diff
        .files
        .iter()
        .map(|f| (f.path.as_str(), f.additions + f.deletions))
        .filter(|(path, churn)| *churn >= 80 && file_category(path) == "code")
        .collect();
    hotspots.sort_by_key(|entry| std::cmp::Reverse(entry.1));
    hotspots.truncate(8);

    let mut test_hotspots: Vec<(&str, usize)> = diff
        .files
        .iter()
        .map(|f| (f.path.as_str(), f.additions + f.deletions))
        .filter(|(path, churn)| *churn >= 80 && file_category(path) == "test")
        .collect();
    test_hotspots.sort_by_key(|entry| std::cmp::Reverse(entry.1));
    test_hotspots.truncate(6);

    let hotspot_html = if hotspots.is_empty() && test_hotspots.is_empty() {
        String::new()
    } else {
        let mut hs = String::from(
            r#"<div style="margin-bottom:12px;font-size:12px;color:var(--muted)"><strong data-i18n="label.codeHotspots">Code hotspots</strong> <span data-i18n="message.codeHotspotsLead">are shown first to keep reviewer focus on risky implementation paths.</span></div><div style="display:flex;flex-wrap:wrap;gap:6px;margin-bottom:14px">"#,
        );
        for (path, churn) in &hotspots {
            let fname = filename_of(path);
            let sid = safe_id(path);
            let _ = write!(
                hs,
                "<a href=\"#file-{sid}\" style=\"display:inline-flex;gap:4px;align-items:center;padding:3px 8px;background:var(--surface-2);border:1px solid var(--line);border-radius:var(--radius-sm);font-size:11px;font-family:var(--mono);color:var(--fg);text-decoration:none\" title=\"{title}\">{fname} <span style=\"color:var(--warn);font-weight:600\">{churn}</span></a>",
                sid = sid,
                title = escape_html(path),
                fname = escape_html(fname),
                churn = churn,
            );
        }
        hs.push_str("</div>");
        if !test_hotspots.is_empty() {
            hs.push_str(
                r#"<details style="margin-bottom:14px"><summary style="cursor:pointer;color:var(--muted);font-size:12px" data-i18n="label.testHotspots">Test hotspots</summary><div style="display:flex;flex-wrap:wrap;gap:6px;margin-top:8px">"#,
            );
            for (path, churn) in &test_hotspots {
                let fname = filename_of(path);
                let sid = safe_id(path);
                let _ = write!(
                    hs,
                    "<a href=\"#file-{sid}\" style=\"display:inline-flex;gap:4px;align-items:center;padding:3px 8px;background:var(--surface-2);border:1px solid var(--line);border-radius:var(--radius-sm);font-size:11px;font-family:var(--mono);color:var(--fg);text-decoration:none\" title=\"{title}\">{fname} <span style=\"color:var(--warn);font-weight:600\">{churn}</span></a>",
                    sid = sid,
                    title = escape_html(path),
                    fname = escape_html(fname),
                    churn = churn,
                );
            }
            hs.push_str("</div></details>");
        }
        hs
    };

    format!(
        r#"<div class="section" id="section-files">
    <div class="section-header">
        <span class="section-title" data-i18n="section.filesChanged">Files Changed</span>
        <span class="section-count">{files_summary}</span>
    </div>
    <div class="card">
        {hotspots}
<div class="file-toggle">
            <button class="toggle-btn active" data-show="code" data-i18n-count="label.codeCaps" data-count="{count_code}">Code ({count_code})</button>
            <button class="toggle-btn" data-show="test" data-i18n-count="label.testsCaps" data-count="{count_test}">Tests ({count_test})</button>
            <button class="toggle-btn" data-show="all" data-i18n-count="label.all" data-count="{total}">All ({total})</button>
        </div>
        <div class="files-toolbar">
            <input type="text" id="file-search" class="file-search" placeholder="Search files..." data-i18n-placeholder="placeholder.searchFiles" />
            <div class="filter-chips">{chips}</div>
            <select id="sort-files" class="sort-select">
                <option value="churn" data-i18n="label.sortChurn">Sort: Churn</option>
                <option value="alpha" data-i18n="label.sortAlpha">Sort: A-Z</option>
                <option value="status" data-i18n="label.sortStatus">Sort: Status</option>
            </select>
        </div>
        <div id="files-container">{html}</div>
    </div>
</div>"#,
        total = total_files,
        files_summary = i18n_template(
            "summary.filesBreakdown",
            &format!(
                "{} files | {} code | {} tests | {} non-code",
                total_files, count_code, count_test, count_other
            ),
            &[
                ("total", total_files.to_string()),
                ("code", count_code.to_string()),
                ("tests", count_test.to_string()),
                ("other", count_other.to_string()),
            ],
        ),
        hotspots = hotspot_html,
        count_code = count_code,
        count_test = count_test,
        chips = chips,
        html = html,
    )
}

pub(super) fn build_ownership_section(ctx: &DashboardContext) -> String {
    if ctx.ownership_map.is_empty() {
        return String::new();
    }

    // Group files by owner
    let mut groups: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (path, owner) in &ctx.ownership_map {
        groups.entry(owner.clone()).or_default().push(path.clone());
    }

    if groups.is_empty() {
        return String::new();
    }

    let mut content = String::new();
    content.push_str(r#"<p style="color:var(--muted);font-size:12px;margin-bottom:12px" data-i18n="message.fileOwnershipHint">File ownership from CODEOWNERS or path-based module detection. Use this to identify who to ping for review.</p>"#);

    for (owner, files) in &groups {
        let file_count = files.len();
        let collapsed = if file_count > 5 {
            " style=\"display:none\""
        } else {
            ""
        };
        let toggle = if file_count > 5 {
            r#" <button class="ownership-toggle" type="button" data-expanded="false" data-i18n="button.showFiles" onclick="toggleOwnershipFiles(this)" style="background:none;border:1px solid var(--line);color:var(--accent);font-size:11px;padding:2px 8px;border-radius:var(--radius-sm);cursor:pointer;margin-left:8px">Show files</button>"#.to_string()
        } else {
            String::new()
        };

        let _ = write!(
            content,
            r#"<div class="ownership-group" style="margin-bottom:14px">
<div style="display:flex;align-items:center;gap:8px;margin-bottom:6px">
    <span class="badge badge-accent" style="font-size:12px;font-weight:600">{owner}</span>
    <span style="color:var(--muted);font-size:12px">{file_count}</span>{toggle}
</div>
<div class="ownership-files"{collapsed}>"#,
            owner = escape_html(owner),
            file_count = i18n_template(
                "count.files",
                &format!("{file_count} files"),
                &[("count", file_count.to_string())],
            ),
            toggle = toggle,
            collapsed = collapsed,
        );

        for file in files {
            let fname = filename_of(file);
            let dir = dir_of(file);
            let _ = write!(
                content,
                r#"<div style="font-size:12px;font-family:var(--mono);padding:2px 0;color:var(--muted)"><span style="color:var(--faint)">{dir}/</span>{fname}</div>"#,
                dir = escape_html(dir),
                fname = escape_html(fname),
            );
        }

        content.push_str("</div></div>");
    }

    let owner_count = groups.len();

    format!(
        r#"<div class="section" id="section-ownership">
    <div class="section-header">
        <span class="section-title" data-i18n="section.ownership">Ownership</span>
    <span class="section-count">{owner_count}</span>
    </div>
    <div class="card">
        {content}
    </div>
</div>"#,
        owner_count = i18n_template(
            "count.owners",
            &format!("{owner_count} owners"),
            &[("count", owner_count.to_string())],
        ),
        content = content,
    )
}

pub(super) fn build_breaking_section(ctx: &DashboardContext) -> String {
    if ctx.breaking.is_empty() {
        return String::new();
    }

    let removed: Vec<_> = ctx
        .breaking
        .iter()
        .filter(|f| matches!(&f.kind, BreakingKind::RemovedSymbol { .. }))
        .collect();
    let changed: Vec<_> = ctx
        .breaking
        .iter()
        .filter(|f| matches!(&f.kind, BreakingKind::ChangedSignature { .. }))
        .collect();
    let env_reqs: Vec<_> = ctx
        .breaking
        .iter()
        .filter(|f| matches!(&f.kind, BreakingKind::NewEnvRequirement { .. }))
        .collect();

    let mut content = String::new();
    content.push_str(r#"<p style="color:var(--muted);font-size:12px;margin-bottom:12px" data-i18n="message.heuristicScanNote">Heuristic scan — may contain false positives. Verify manually.</p>"#);

    if !removed.is_empty() {
        content.push_str(r#"<h4 style="font-size:13px;color:var(--muted);margin:12px 0 6px" data-i18n="section.removedPublicSymbols">Removed Public Symbols</h4>"#);
        content.push_str(r#"<table class="breaking-table"><thead><tr><th data-i18n="label.file">File</th><th data-i18n="label.symbol">Symbol</th><th data-i18n="label.type">Type</th><th data-i18n="label.risk">Risk</th></tr></thead><tbody>"#);
        for f in &removed {
            if let BreakingKind::RemovedSymbol { symbol_type } = &f.kind {
                let risk_badge = risk_badge_html(&f.risk_level);
                let _ = write!(
                    content,
                    "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
                    escape_html(&f.file),
                    escape_html(&f.line),
                    escape_html(symbol_type),
                    risk_badge,
                );
            }
        }
        content.push_str("</tbody></table>");
    }

    if !changed.is_empty() {
        content.push_str(r#"<h4 style="font-size:13px;color:var(--muted);margin:12px 0 6px" data-i18n="section.changedSignatures">Changed Signatures</h4>"#);
        content.push_str(r#"<table class="breaking-table"><thead><tr><th data-i18n="label.file">File</th><th data-i18n="label.before">Before</th><th data-i18n="label.after">After</th><th data-i18n="label.risk">Risk</th></tr></thead><tbody>"#);
        for f in &changed {
            if let BreakingKind::ChangedSignature { before, after } = &f.kind {
                let risk_badge = risk_badge_html(&f.risk_level);
                let _ = write!(
                    content,
                    "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
                    escape_html(&f.file),
                    escape_html(before),
                    escape_html(after),
                    risk_badge,
                );
            }
        }
        content.push_str("</tbody></table>");
    }

    if !env_reqs.is_empty() {
        content.push_str(r#"<h4 style="font-size:13px;color:var(--muted);margin:12px 0 6px" data-i18n="section.newEnvRequirements">New Environment Requirements</h4>"#);
        content.push_str(r#"<table class="breaking-table"><thead><tr><th data-i18n="label.file">File</th><th data-i18n="label.variable">Variable</th><th data-i18n="label.risk">Risk</th></tr></thead><tbody>"#);
        for f in &env_reqs {
            if let BreakingKind::NewEnvRequirement { variable } = &f.kind {
                let risk_badge = risk_badge_html(&f.risk_level);
                let _ = write!(
                    content,
                    "<tr><td>{}</td><td>{}</td><td>{}</td></tr>",
                    escape_html(&f.file),
                    escape_html(variable),
                    risk_badge,
                );
            }
        }
        content.push_str("</tbody></table>");
    }

    format!(
        r#"<div class="section" id="section-breaking">
    <div class="section-header">
        <span class="section-title" data-i18n="section.breaking">Breaking</span>
        <span class="section-count">{count_label}</span>
    </div>
    <div class="card">{content}</div>
</div>"#,
        count_label = i18n_template(
            "count.detected",
            &format!("{} detected", ctx.breaking.len()),
            &[("count", ctx.breaking.len().to_string())],
        ),
        content = content,
    )
}

pub(super) fn build_coverage_section(ctx: &DashboardContext) -> String {
    let cov = &ctx.coverage;
    if cov.total_source == 0 {
        return String::new();
    }

    // Coverage % is a metric, not a verdict: keep color only for the negative
    // signal (below threshold = "what is wrong?"), neutralize the rest.
    let pct_color = if cov.pct < 50 {
        "var(--warn)"
    } else {
        "var(--fg)"
    };

    let mut uncovered_html = String::new();
    if !cov.uncovered.is_empty() {
        uncovered_html.push_str(r#"<details style="margin-top:12px"><summary style="cursor:pointer;color:var(--muted);font-size:13px">"#);
        uncovered_html.push_str(&i18n_template(
            "message.filesWithoutTests",
            &format!("Files without test changes ({})", cov.uncovered.len()),
            &[("count", cov.uncovered.len().to_string())],
        ));
        uncovered_html.push_str(":</summary>");
        uncovered_html.push_str(r#"<div class="coverage-file-list" style="margin-top:8px">"#);
        for f in &cov.uncovered {
            let _ = write!(
                uncovered_html,
                r#"<div class="coverage-file"><span style="color:var(--faint)">{}</span> <span>{}</span></div>"#,
                escape_html(&f.status.to_string()),
                escape_html(&f.path),
            );
        }
        uncovered_html.push_str("</div></details>");
    }

    let mut covered_html = String::new();
    if !cov.covered.is_empty() {
        covered_html.push_str(r#"<details style="margin-top:8px"><summary style="cursor:pointer;color:var(--muted);font-size:13px">"#);
        covered_html.push_str(&i18n_template(
            "message.filesWithMatchingTests",
            &format!("Files with matching test changes ({})", cov.covered.len()),
            &[("count", cov.covered.len().to_string())],
        ));
        covered_html.push_str(":</summary>");
        covered_html.push_str(r#"<div class="coverage-file-list" style="margin-top:8px">"#);
        for p in &cov.covered {
            let _ = write!(
                covered_html,
                r#"<div class="coverage-file"><span>{} {}</span> <span style="color:var(--faint)">&#x2194;</span> <span>{} {}</span></div>"#,
                escape_html(&p.src_status.to_string()),
                escape_html(&p.src_path),
                escape_html(&p.test_status.to_string()),
                escape_html(&p.test_path),
            );
        }
        covered_html.push_str("</div></details>");
    }

    format!(
        r#"<div class="section" id="section-coverage">
    <div class="section-header">
        <span class="section-title" data-i18n="section.coverage">Coverage</span>
        <span class="section-count" data-i18n="label.coverageHeuristic">coverage heuristic</span>
    </div>
    <div class="card">
        <div class="coverage-summary">
            <span class="coverage-pct" style="color:{color}">{pct}%</span>
            <div class="coverage-detail">
                <div>{coverage_detail}</div>
                <div style="color:var(--faint);font-size:12px">{coverage_note}{non_code_note}</div>
            </div>
        </div>
        {uncovered}
        {covered_details}
    </div>
</div>"#,
        color = pct_color,
        pct = cov.pct,
        coverage_detail = i18n_template(
            "message.coverageDetail",
            &format!(
                "{}/{} changed source files have matching test changes",
                cov.covered_count, cov.total_source
            ),
            &[
                ("covered", cov.covered_count.to_string()),
                ("total", cov.total_source.to_string()),
            ],
        ),
        coverage_note = i18n_template(
            "message.coverageHeuristicNote",
            "File-name matching heuristic — not actual code coverage",
            &[],
        ),
        uncovered = uncovered_html,
        covered_details = covered_html,
        non_code_note = if cov.non_code_count > 0 {
            i18n_template(
                "message.excludedNonCode",
                &format!(" · {} non-code files excluded", cov.non_code_count),
                &[("count", cov.non_code_count.to_string())],
            )
        } else {
            String::new()
        },
    )
}

/// Build the "Assets Changed" section (B1).
pub(super) fn build_assets_section(diff: Option<&Diff>) -> String {
    let Some(diff) = diff else {
        return String::new();
    };

    let asset_files: Vec<&FileChange> = diff
        .files
        .iter()
        .filter(|f| file_category(&f.path) == "asset")
        .collect();

    if asset_files.is_empty() {
        return String::new();
    }

    // Group by directory
    let mut groups: BTreeMap<String, Vec<&FileChange>> = BTreeMap::new();
    for file in &asset_files {
        let dir = dir_of(&file.path).to_string();
        groups.entry(dir).or_default().push(file);
    }

    let mut content = String::new();
    content.push_str(r#"<div style="display:grid;grid-template-columns:repeat(auto-fill,minmax(220px,1fr));gap:12px">"#);

    for (dir_name, files) in &groups {
        for file in files {
            let fname = filename_of(&file.path);
            let sc = status_char(file.status);
            let churn = file.additions + file.deletions;

            let _ = write!(
                content,
                r#"<div style="background:var(--surface-2);border-radius:var(--radius-sm);padding:12px;display:flex;flex-direction:column;gap:4px">
    <div style="display:flex;align-items:center;gap:6px">
        <span class="file-badge {sc}" style="font-size:10px;width:20px;height:16px">{sc}</span>
        <span style="font-family:var(--mono);font-size:12px;overflow:hidden;text-overflow:ellipsis;white-space:nowrap" title="{path}">{fname}</span>
    </div>
    <div style="font-size:11px;color:var(--faint)">{dir}/</div>
    <div style="font-size:11px;color:var(--muted)"><span data-i18n="label.churn">Churn</span>: {churn}</div>
</div>"#,
                sc = sc,
                path = escape_html(&file.path),
                fname = escape_html(fname),
                dir = escape_html(dir_name),
                churn = churn,
            );
        }
    }

    content.push_str("</div>");

    format!(
        r#"<div class="section" id="section-assets">
    <div class="section-header">
        <span class="section-title" data-i18n="section.assetsChanged">Assets Changed</span>
        <span class="section-count">{count}</span>
    </div>
    <div class="card">{content}</div>
</div>"#,
        count = i18n_template(
            "count.files",
            &format!("{} files", asset_files.len()),
            &[("count", asset_files.len().to_string())],
        ),
        content = content,
    )
}

/// Build the SARIF findings table section (B6).
/// Enhanced version of build_findings_section with structured columns.
pub(super) fn build_sarif_table_section(ctx: &DashboardContext) -> String {
    if ctx.findings.is_empty() {
        return String::new();
    }

    let mut rows = String::new();
    for f in &ctx.findings {
        let badge_class = if f.level == "error" {
            "badge-error"
        } else {
            "badge-warning"
        };
        let severity_text = if f.level == "error" {
            i18n_template("label.error", "Error", &[])
        } else {
            i18n_template("label.warning", "Warning", &[])
        };

        // Extract rule_id-like info from check_name (e.g. "cargo_clippy" -> "clippy")
        let rule_id = f.check_name.replace(' ', "_").to_lowercase();

        // Extract file path and line from message if available (pattern: "path:line:")
        let (file_path, line_num, clean_msg) = extract_finding_location(&f.message);

        let file_cell = if let Some(fp) = file_path {
            let line_suffix = line_num.map(|l| format!(":{}", l)).unwrap_or_default();
            format!(
                r#"<span style="font-family:var(--mono);font-size:11px">{}{}</span>"#,
                escape_html(fp),
                line_suffix
            )
        } else {
            r#"<span style="color:var(--faint)">-</span>"#.to_string()
        };

        let _ = write!(
            rows,
            r#"<tr>
    <td style="font-family:var(--mono);font-size:11px">{rule}</td>
    <td><span class="badge {bc}" style="font-size:10px">{sev}</span></td>
    <td>{file}</td>
    <td style="font-size:12px">{msg}</td>
</tr>"#,
            rule = escape_html(&rule_id),
            bc = badge_class,
            sev = severity_text,
            file = file_cell,
            msg = escape_html(&clean_msg),
        );
    }

    format!(
        r#"<div class="section" id="section-findings">
    <div class="section-header">
        <span class="section-title" data-i18n="section.inlineFindings">Inline Findings</span>
        <span class="section-count">{count}</span>
    </div>
    <div class="card" style="padding:0;overflow:hidden">
        <table class="checks-table">
            <thead><tr><th data-i18n="label.rule">Rule</th><th data-i18n="label.severity">Severity</th><th data-i18n="label.file">File</th><th data-i18n="label.message">Message</th></tr></thead>
            <tbody>{rows}</tbody>
        </table>
    </div>
</div>"#,
        count = i18n_template(
            "count.findings",
            &format!("{} findings", ctx.findings.len()),
            &[("count", ctx.findings.len().to_string())],
        ),
        rows = rows,
    )
}

/// Try to extract file path and line number from a finding message.
/// Common patterns: "src/foo.rs:42: message" or "at src/foo.rs:42"
pub(super) fn extract_finding_location(message: &str) -> (Option<&str>, Option<usize>, String) {
    // Pattern: "path:line: rest" or "path:line:col: rest"
    let trimmed = message.trim();

    // Try "path.ext:N:" pattern
    for (i, ch) in trimmed.char_indices() {
        if ch == ':' && i > 0 {
            let candidate = &trimmed[..i];
            // Must look like a file path (has extension or slashes)
            if (candidate.contains('.') || candidate.contains('/'))
                && !candidate.contains(' ')
                && candidate.len() > 2
            {
                let rest = &trimmed[i + 1..];
                // Try to parse line number
                let num_end = rest
                    .find(|c: char| !c.is_ascii_digit())
                    .unwrap_or(rest.len());
                if num_end > 0
                    && let Ok(line) = rest[..num_end].parse::<usize>()
                {
                    let msg_start = rest[num_end..].trim_start_matches(':').trim_start();
                    return (Some(candidate), Some(line), msg_start.to_string());
                }
                // No line number, just file path
                let msg_rest = rest.trim_start_matches(':').trim_start();
                return (Some(candidate), None, msg_rest.to_string());
            }
        }
    }

    (None, None, trimmed.to_string())
}

/// Build the i18n parity check section (B2).
pub(super) fn build_i18n_section(ctx: &DashboardContext) -> String {
    let Some(ref i18n) = ctx.i18n_delta else {
        return String::new();
    };

    if i18n.missing_keys.is_empty() && i18n.key_counts.is_empty() {
        return String::new();
    }

    let mut content = String::new();

    // Key counts per locale
    if !i18n.key_counts.is_empty() {
        content.push_str(r#"<h4 style="font-size:13px;color:var(--muted);margin:0 0 8px" data-i18n="label.keyCountsPerLocale">Key Counts per Locale</h4>"#);
        content.push_str(r#"<div style="display:flex;flex-wrap:wrap;gap:8px;margin-bottom:16px">"#);
        for (locale, count) in &i18n.key_counts {
            let _ = write!(
                content,
                r#"<span class="badge badge-info" style="font-size:11px">{}: {}</span>"#,
                escape_html(locale),
                count
            );
        }
        content.push_str("</div>");
    }

    // Missing keys
    if !i18n.missing_keys.is_empty() {
        content.push_str(r#"<h4 style="font-size:13px;color:var(--muted);margin:0 0 8px" data-i18n="label.missingKeys">Missing Keys</h4>"#);
        content.push_str(r#"<table class="checks-table"><thead><tr><th data-i18n="label.key">Key</th><th data-i18n="label.missingInLocales">Missing in Locales</th></tr></thead><tbody>"#);
        for (key, missing_locales) in &i18n.missing_keys {
            let locales_html: String = missing_locales.iter()
                .map(|l| format!(r#"<span class="badge badge-warning" style="font-size:9px;padding:1px 5px">{}</span>"#, escape_html(l)))
                .collect::<Vec<String>>()
                .join(" ");
            let _ = write!(
                content,
                "<tr><td style=\"font-family:var(--mono);font-size:11px\">{}</td><td>{}</td></tr>",
                escape_html(key),
                locales_html,
            );
        }
        content.push_str("</tbody></table>");
    } else {
        content.push_str(
            r#"<p style="color:var(--pass);font-size:13px" data-i18n="message.allLocalesMatching">All locales have matching keys.</p>"#,
        );
    }

    format!(
        r#"<div class="section" id="section-i18n">
    <div class="section-header">
        <span class="section-title" data-i18n="section.i18nParity">i18n Parity Check</span>
        <span class="section-count">{locale_count}</span>
    </div>
    <div class="card">{content}</div>
</div>"#,
        locale_count = i18n_template(
            "count.locales",
            &format!("{} locales", i18n.locale_count),
            &[("count", i18n.locale_count.to_string())],
        ),
        content = content,
    )
}

pub(super) fn build_distribution_section(diff: Option<&Diff>) -> String {
    let Some(diff) = diff else {
        return String::new();
    };
    if diff.files.is_empty() {
        return String::new();
    }

    let mut dir_churn: BTreeMap<String, (usize, usize)> = BTreeMap::new();
    for file in &diff.files {
        let top_dir = match file.path.find('/') {
            Some(i) => &file.path[..i],
            None => "(root)",
        };
        let entry = dir_churn.entry(top_dir.to_string()).or_default();
        entry.0 += file.additions;
        entry.1 += file.deletions;
    }

    let mut sorted: Vec<(String, usize, usize)> = dir_churn
        .into_iter()
        .map(|(name, (a, d))| (name, a, d))
        .collect();
    sorted.sort_by_key(|entry| std::cmp::Reverse(entry.1 + entry.2));
    sorted.truncate(10);

    if sorted.is_empty() {
        return String::new();
    }

    let max_total = sorted
        .iter()
        .map(|(_, a, d)| a + d)
        .max()
        .unwrap_or(1)
        .max(1);

    let mut bars = String::new();
    let total_files = diff.files.len();
    let total_adds: usize = diff.files.iter().map(|f| f.additions).sum();
    let total_dels: usize = diff.files.iter().map(|f| f.deletions).sum();

    let stat_summary = format!(
        r#"<div class="stat-summary" style="margin-bottom:15px;font-family:var(--mono);font-size:12px;color:var(--muted)">
    {} file{} changed, <span class="green">{} insertion{}(+)</span>, <span class="red">{} deletion{}(-)</span>
</div>"#,
        total_files,
        if total_files == 1 { "" } else { "s" },
        format_number(total_adds),
        if total_adds == 1 { "" } else { "s" },
        format_number(total_dels),
        if total_dels == 1 { "" } else { "s" },
    );

    for (name, adds, dels) in &sorted {
        let total = adds + dels;
        let total_pct = (total as f64 / max_total as f64) * 100.0;
        let add_pct = if total > 0 {
            (*adds as f64 / total as f64) * total_pct
        } else {
            0.0
        };
        let del_pct = total_pct - add_pct;

        let _ = write!(
            bars,
            r#"<div class="dist-row">
    <span class="dist-label" title="{name}">{name}/</span>
    <span class="dist-bar-wrap"><span class="dist-bar-add" style="width:{apct:.1}%"></span><span class="dist-bar-del" style="width:{dpct:.1}%"></span></span>
    <span class="dist-count">{total}</span>
</div>"#,
            name = escape_html(name),
            apct = add_pct,
            dpct = del_pct,
            total = format_number(total),
        );
    }

    format!(
        r#"<div class="section section-noise" id="section-distribution">
    <div class="section-header">
        <span class="section-title" data-i18n="section.changeDistribution">Change Distribution</span>
        <span class="section-count" data-i18n="label.topDirectoriesByChurn">top directories by churn</span>
    </div>
    <div class="card">
        {stat_summary}
        <div class="dist-chart">{bars}</div>
    </div>
</div>"#,
        stat_summary = stat_summary,
        bars = bars,
    )
}

pub(super) fn build_commits_section(
    diff: Option<&Diff>,
    pr_url: Option<&str>,
    batch_count: usize,
) -> String {
    let Some(diff) = diff else {
        return String::new();
    };
    if diff.commits.is_empty() {
        return String::new();
    }

    let mut items = String::new();
    for commit in &diff.commits {
        let hash_html = match pr_url {
            Some(url) => {
                let repo_url = url.rfind("/pull/").map(|i| &url[..i]).unwrap_or(url);
                format!(
                    r#"<a class="commit-hash" href="{repo}/commit/{id}" target="_blank">{short}</a>"#,
                    repo = escape_html(repo_url),
                    id = escape_html(&commit.id),
                    short = escape_html(&commit.short_id),
                )
            }
            None => format!(
                r#"<span class="commit-hash">{short}</span>"#,
                short = escape_html(&commit.short_id),
            ),
        };

        let date_display = if commit.date.len() >= 10 {
            &commit.date[..10]
        } else {
            &commit.date
        };

        let _ = write!(
            items,
            r#"<div class="commit-item">
    <span class="commit-dot"></span>
    <div class="commit-body">
        <span class="commit-msg">{msg}</span>
        <span class="commit-meta">
            <span>{author}</span>
            <span>{date}</span>
        </span>
    </div>
    {hash}
</div>"#,
            msg = escape_html(&commit.message),
            author = escape_html(&commit.author),
            date = escape_html(date_display),
            hash = hash_html,
        );
    }

    // Batch patch links
    let mut batch_html = String::new();
    if batch_count > 0 {
        let _ = write!(
            batch_html,
            r#"<div class="batch-links"><span data-i18n="label.perCommitPatches">Per-commit patches</span>:"#
        );
        let _ = write!(
            batch_html,
            r#"<a href="10_diff/per-commit-diffs/00-SUMMARY.md" data-i18n="label.summary">Summary</a>"#
        );
        for i in 1..=batch_count {
            let _ = write!(
                batch_html,
                r#"<a href="10_diff/per-commit-diffs/batch-{:02}.patch">{}</a>"#,
                i,
                i18n_template(
                    "label.batch",
                    &format!("Batch {:02}", i),
                    &[("count", format!("{:02}", i))],
                )
            );
        }
        batch_html.push_str("</div>");
    }

    let count = diff.commits.len();
    format!(
        r#"<div class="section section-noise" id="section-commits">
    <div class="section-header">
        <span class="section-title" data-i18n="section.commits">Commits</span>
        <span class="section-count">{count}</span>
    </div>
    <div class="card" style="padding:0; overflow:hidden">
        <div class="commits-list">{items}</div>
        {batches}
    </div>
</div>"#,
        count = count,
        items = items,
        batches = batch_html,
    )
}

pub(super) fn build_loctree_section(heuristics: Option<&HeuristicsResult>) -> String {
    let Some(h) = heuristics else {
        return String::new();
    };
    let Some(ref loctree) = h.loctree else {
        return String::new();
    };
    if !loctree.available {
        return String::new();
    }

    let mut langs: Vec<_> = loctree.stats.by_language.iter().collect();
    langs.sort_by_key(|entry| std::cmp::Reverse(entry.1.loc));
    let max_loc = langs.first().map(|(_, s)| s.loc).unwrap_or(1).max(1);

    let mut lang_html = String::new();
    for (lang, stats) in &langs {
        let pct = stats.loc as f64 / max_loc as f64 * 100.0;
        let hue = lang_hue(lang);
        let _ = write!(
            lang_html,
            r#"<div class="lang-row">
    <span class="lang-pill" style="background:hsl({hue},55%,40%)">{lang}</span>
    <span class="lang-bar" style="width:{pct:.1}%; background:hsl({hue},55%,35%)"></span>
    <span class="lang-count">{files}f / {loc}L</span>
</div>"#,
            hue = hue,
            lang = escape_html(lang),
            pct = pct,
            files = format_number(stats.files),
            loc = format_number(stats.loc),
        );
    }

    let mut issues_html = String::new();
    let issue_items: Vec<(usize, &str, &str)> = vec![
        (
            loctree.dead_exports.len(),
            "label.deadExports",
            "Dead exports",
        ),
        (
            loctree.cycles.len(),
            "message.cyclesLabel",
            "Circular imports",
        ),
        (
            loctree.twins.dead_parrots.len(),
            "message.unusedSymbolsLabel",
            "Unused symbols",
        ),
        (
            loctree.twins.exact_twins.len(),
            "message.exactTwinsLabel",
            "Exact twins",
        ),
    ];

    let any_issues = issue_items.iter().any(|(c, _, _)| *c > 0);
    if any_issues {
        for (count, key, label) in &issue_items {
            if *count > 0 {
                let _ = write!(
                    issues_html,
                    r#"<div class="issue-card"><span class="issue-count warn">{count}</span><span data-i18n="{key}">{label}</span></div>"#,
                    count = count,
                    key = key,
                    label = label,
                );
            }
        }
    } else {
        issues_html.push_str(r#"<div class="issue-card clean"><span class="issue-count ok">0</span><span data-i18n="label.noIssuesDetected">No issues detected</span></div>"#);
    }

    format!(
        r#"<div class="section section-noise" id="section-statistics">
    <div class="section-header">
        <span class="section-title" data-i18n="section.codeStatistics">Code Statistics</span>
        <span class="section-count">{files_loc}</span>
    </div>
    <div class="loctree-grid">
        <div class="card">
            <div style="font-size:13px; color:var(--muted); margin-bottom:10px; text-transform:uppercase; letter-spacing:0.5px" data-i18n="label.languages">Languages</div>
            <div class="lang-bar-wrap">{lang_html}</div>
        </div>
        <div class="card">
            <div style="font-size:13px; color:var(--muted); margin-bottom:10px; text-transform:uppercase; letter-spacing:0.5px" data-i18n="label.codeHealth">Code Health</div>
            {issues_html}
        </div>
    </div>
</div>"#,
        files_loc = i18n_template(
            "count.filesLoc",
            &format!(
                "{} files / {} LOC",
                format_number(loctree.stats.total_files),
                format_number(loctree.stats.total_loc),
            ),
            &[
                ("files", format_number(loctree.stats.total_files)),
                ("loc", format_number(loctree.stats.total_loc)),
            ],
        ),
        lang_html = lang_html,
        issues_html = issues_html,
    )
}

pub(super) fn build_regression_details_section(
    ctx: &DashboardContext,
    heuristics: Option<&HeuristicsResult>,
    regression: Option<&crate::regression::RegressionReport>,
) -> String {
    if heuristics.and_then(|h| h.regression.as_ref()).is_none() && regression.is_none() {
        return String::new();
    }

    // --- Section header: score from regression report, or fallback ---
    let (score_display, severity_display) = if let Some(reg) = regression {
        (
            format!("{}", reg.score.score),
            reg.score.severity.as_str().to_string(),
        )
    } else {
        ("--".to_string(), "N/A".to_string())
    };

    let mut html = String::new();
    let _ = write!(
        html,
        r#"<div class="section" id="section-regression">
    <div class="section-header">
        <span class="section-title" data-i18n="section.regression">Regression</span>
        <span class="section-count">{score_summary}</span>
    </div>
    <div class="tab-container">
        <div class="tab-buttons">"#,
        score_summary = i18n_template(
            "summary.regressionScore",
            &format!("Score: {score_display}/100 ({severity_display})"),
            &[
                ("score", score_display.clone()),
                ("severity", severity_display.clone()),
            ],
        ),
    );

    // Tab buttons — only render tabs that have data
    let has_score = regression.is_some();
    let has_hotspots = regression
        .is_some_and(|r| !r.diff.top_code_hotspots.is_empty() || !r.diff.top_hotspots.is_empty())
        || !ctx.risk_scores.is_empty();
    let has_heuristics = heuristics.and_then(|h| h.regression.as_ref()).is_some();

    let first_active = if has_score {
        "tab-reg-score"
    } else if has_hotspots {
        "tab-reg-hotspots"
    } else {
        "tab-reg-heuristics"
    };

    if has_score {
        let cls = if first_active == "tab-reg-score" {
            " active"
        } else {
            ""
        };
        let _ = write!(
            html,
            r#"<button class="tab-btn{cls}" data-tab="tab-reg-score" data-i18n="label.score">Score</button>"#,
            cls = cls,
        );
    }
    if has_hotspots {
        let cls = if first_active == "tab-reg-hotspots" {
            " active"
        } else {
            ""
        };
        let _ = write!(
            html,
            r#"<button class="tab-btn{cls}" data-tab="tab-reg-hotspots" data-i18n="label.hotspots">Hotspots</button>"#,
            cls = cls,
        );
    }
    if has_heuristics {
        let cls = if first_active == "tab-reg-heuristics" {
            " active"
        } else {
            ""
        };
        let _ = write!(
            html,
            r#"<button class="tab-btn{cls}" data-tab="tab-reg-heuristics" data-i18n="section.heuristics">Heuristics</button>"#,
            cls = cls,
        );
    }

    html.push_str("</div>"); // close tab-buttons

    // --- Tab 1: Score ---
    if has_score {
        let reg = regression.unwrap();
        let severity = reg.score.severity.as_str();
        let score = reg.score.score;

        let (header_class, header_icon) = match reg.score.severity {
            crate::regression::score::Severity::OK => ("alert-info", "&#x2713;"),
            crate::regression::score::Severity::LOW => ("alert-info", "&#x2139;"),
            crate::regression::score::Severity::MED => ("alert-warning", "&#x26A0;"),
            crate::regression::score::Severity::HIGH => ("alert-warning", "&#x26A0;"),
            crate::regression::score::Severity::CRITICAL => ("alert-error", "&#x2718;"),
        };

        let mut reasons_html = String::new();
        for reason in reg.score.score_reasons.iter().take(5) {
            let _ = write!(
                reasons_html,
                r#"<div style="padding:3px 0;font-size:13px;color:var(--muted);font-family:var(--mono)">{}</div>"#,
                escape_html(reason),
            );
        }
        if reasons_html.is_empty() {
            reasons_html.push_str(
                r#"<div style="padding:3px 0;font-size:13px;color:var(--faint)" data-i18n="message.noRegressionSignals">No regression signals</div>"#,
            );
        }

        let active = if first_active == "tab-reg-score" {
            " active"
        } else {
            ""
        };
        let _ = write!(
            html,
            r#"<div id="tab-reg-score" class="tab-panel{active}">
    <div class="card {header_class}" style="padding:14px">
        <div style="display:flex;align-items:center;gap:8px;margin-bottom:8px">
            <span style="font-size:18px">{icon}</span>
            <span style="font-weight:600;font-size:15px">{severity_label}</span>
            <span style="font-family:var(--mono);font-size:14px;color:var(--muted);margin-left:auto">{score_label}</span>
        </div>
        {reasons}
    </div>
</div>"#,
            active = active,
            header_class = header_class,
            icon = header_icon,
            severity_label = i18n_template(
                "summary.severityValue",
                &format!("Severity: {severity}"),
                &[("value", severity.to_string())],
            ),
            score_label = i18n_template(
                "summary.scoreValue",
                &format!("Score: {score}"),
                &[("value", score.to_string())],
            ),
            reasons = reasons_html,
        );
    }

    // --- Tab 2: Hotspots ---
    if has_hotspots {
        let mut hotspot_rows = String::new();
        if let Some(reg) = regression {
            let hotspots = if reg.diff.top_code_hotspots.is_empty() {
                &reg.diff.top_hotspots
            } else {
                &reg.diff.top_code_hotspots
            };

            for hs in hotspots.iter().take(10) {
                let untested = if reg.tests.untested_critical_files.contains(&hs.file) {
                    i18n_template("label.yes", "YES", &[])
                } else {
                    "-".to_string()
                };
                let in_cycle = if reg.deps.top_cycles.iter().any(|c| c.contains(&hs.file)) {
                    i18n_template("label.yes", "YES", &[])
                } else {
                    "-".to_string()
                };
                let mut flags = Vec::new();
                if reg.perf.suspected_files.iter().any(|s| s.file == hs.file) {
                    flags.push("perf");
                }
                if hs.churn > 500 {
                    flags.push("high-churn");
                }
                let flags_str = if flags.is_empty() {
                    "-".to_string()
                } else {
                    flags.join(", ")
                };

                let _ = write!(
                    hotspot_rows,
                    r#"<tr>
    <td style="font-family:var(--mono);font-size:12px;max-width:300px;overflow:hidden;text-overflow:ellipsis;white-space:nowrap" title="{file}">{file}</td>
    <td style="text-align:right">{churn}</td>
    <td style="text-align:center">{untested}</td>
    <td style="text-align:center">{cycle}</td>
    <td style="font-size:12px">{flags}</td>
</tr>"#,
                    file = escape_html(&hs.file),
                    churn = hs.churn,
                    untested = untested,
                    cycle = in_cycle,
                    flags = flags_str,
                );
            }
        }

        let mut risk_rows = String::new();
        for rs in ctx.risk_scores.iter().take(8) {
            let factors = if rs.factors.is_empty() {
                "-".to_string()
            } else {
                rs.factors.join(", ")
            };
            let _ = write!(
                risk_rows,
                r#"<tr>
    <td style="font-family:var(--mono);font-size:12px" title="{path}">{name}</td>
    <td style="text-align:right">{score}</td>
    <td style="font-size:12px;color:var(--muted)">{factors}</td>
</tr>"#,
                path = escape_html(&rs.path),
                name = escape_html(filename_of(&rs.path)),
                score = rs.score,
                factors = escape_html(&factors),
            );
        }

        let active = if first_active == "tab-reg-hotspots" {
            " active"
        } else {
            ""
        };
        let _ = write!(
            html,
            r#"<div id="tab-reg-hotspots" class="tab-panel{active}">
    {top_risk}
    {hotspots_table}
</div>"#,
            active = active,
            top_risk = if risk_rows.is_empty() {
                String::new()
            } else {
                format!(
                    r#"<div style="margin-bottom:16px">
    <div class="signal-card-title" style="margin-bottom:8px" data-i18n="label.topRiskyFiles">Top risky files</div>
    <table class="checks-table">
        <thead><tr><th data-i18n="label.file">File</th><th style="text-align:right" data-i18n="label.risk">Risk</th><th data-i18n="label.factors">Factors</th></tr></thead>
        <tbody>{rows}</tbody>
    </table>
</div>"#,
                    rows = risk_rows,
                )
            },
            hotspots_table = if hotspot_rows.is_empty() {
                String::new()
            } else {
                format!(
                    r#"<table class="checks-table">
        <thead><tr>
            <th data-i18n="label.file">File</th>
            <th style="text-align:right" data-i18n="label.churn">Churn</th>
            <th style="text-align:center" data-i18n="label.untested">Untested?</th>
            <th style="text-align:center" data-i18n="label.cycles">Cycles?</th>
            <th data-i18n="label.flags">Flags</th>
        </tr></thead>
        <tbody>{rows}</tbody>
    </table>"#,
                    rows = hotspot_rows,
                )
            },
        );
    }

    // --- Tab 3: Heuristics ---
    if has_heuristics {
        let reg = heuristics.unwrap().regression.as_ref().unwrap();

        let base_short = reg.base_sha.chars().take(7).collect::<String>();
        let target_short = reg.target_sha.chars().take(7).collect::<String>();

        let (heur_icon, heur_label) = if reg.regression_detected {
            (
                "&#x26A0;",
                i18n_template("message.heuristicsRegression", "Heuristics regression", &[]),
            )
        } else if reg.improvement_detected {
            (
                "&#x2713;",
                i18n_template(
                    "message.heuristicsImprovement",
                    "Heuristics improvement",
                    &[],
                ),
            )
        } else {
            (
                "&#x2015;",
                i18n_template("message.heuristicsUnchanged", "Heuristics unchanged", &[]),
            )
        };

        fn delta_html(label: &str, delta: i64, base: usize, target: usize) -> String {
            let (delta_display, color) = if delta > 0 {
                (format!("+{delta}"), "var(--block)")
            } else if delta < 0 {
                (format!("{delta}"), "var(--pass)")
            } else {
                ("\u{00b1}0".to_string(), "var(--muted)")
            };
            format!(
                r#"<div style="display:flex;justify-content:space-between;align-items:center;padding:5px 0;border-bottom:1px solid var(--line)">
    <span style="color:var(--muted);font-size:13px">{label}</span>
    <span style="font-family:var(--mono);font-size:13px">
        <span style="color:var(--faint)">{base} &#x2192; {target}</span>
        &nbsp;
        <span style="color:{color};font-weight:600">{delta_display}</span>
    </span>
</div>"#,
                label = label,
                base = base,
                target = target,
                color = color,
                delta_display = delta_display,
            )
        }

        let rows = format!(
            "{}{}{}",
            delta_html(
                &i18n_template("label.deadExports", "Dead exports", &[]),
                reg.dead_exports_delta,
                reg.base_dead_exports,
                reg.target_dead_exports
            ),
            delta_html(
                &i18n_template("message.cyclesLabel", "Circular imports", &[]),
                reg.cycles_delta,
                reg.base_circular_imports,
                reg.target_circular_imports
            ),
            delta_html(
                &i18n_template("message.unusedSymbolsLabel", "Unused symbols", &[]),
                reg.unused_symbols_delta(),
                reg.base_unused_symbols(),
                reg.target_unused_symbols()
            ),
        );

        let active = if first_active == "tab-reg-heuristics" {
            " active"
        } else {
            ""
        };
        let _ = write!(
            html,
            r#"<div id="tab-reg-heuristics" class="tab-panel{active}">
    <div style="margin-bottom:8px;display:flex;align-items:center;gap:6px">
        <span>{icon}</span>
        <span style="font-weight:600;font-size:14px">{label}</span>
        <span style="font-size:11px;color:var(--faint);margin-left:8px">
            <code style="font-family:var(--mono)">{base}</code> &rarr; <code style="font-family:var(--mono)">{target}</code>
        </span>
    </div>
    {rows}
</div>"#,
            active = active,
            icon = heur_icon,
            label = heur_label,
            base = base_short,
            target = target_short,
            rows = rows,
        );
    }

    html.push_str("</div></div>"); // close tab-container + section
    html
}

/// Brand-aligned [`Theme`](crate::mdrender::Theme) for the dashboard narrative.
///
/// Delegates to [`crate::artifacts::brand::dashboard_narrative_theme`] so the
/// narrative shares the single source of truth for brand tokens with the rest
/// of the artifact pack.
pub(super) fn narrative_theme() -> crate::mdrender::Theme {
    crate::artifacts::brand::dashboard_narrative_theme()
}

pub(super) fn build_narrative_section(pr_review_content: &str) -> String {
    if pr_review_content.trim().is_empty() {
        return String::new();
    }

    let rendered = crate::mdrender::render(pr_review_content.trim(), &narrative_theme());

    format!(
        r#"<div class="section" id="section-narrative">
    <div class="section-header">
        <span class="section-title" data-i18n="section.narrativeReview">Narrative Review</span>
        <button id="copy-narrative-btn" class="btn-ghost" data-i18n="button.copyMarkdown">Copy as Markdown</button>
    </div>
    <div class="card narrative-rendered">{content}</div>
    <pre class="narrative-content" style="display:none">{raw}</pre>
</div>"#,
        content = rendered,
        raw = escape_html(pr_review_content.trim()),
    )
}

pub(super) fn artifact_kind(path: &str) -> &'static str {
    if path.ends_with(".patch") {
        "PATCH"
    } else if path.ends_with(".json") {
        "JSON"
    } else if path.ends_with(".md") {
        "MD"
    } else if path.ends_with(".sarif") {
        "SARIF"
    } else if path.ends_with(".log") || path.ends_with(".txt") {
        "LOG"
    } else if path.ends_with(".html") {
        "HTML"
    } else if path.ends_with(".zip") {
        "ZIP"
    } else {
        "FILE"
    }
}

pub(super) fn build_artifacts_section(ctx: &DashboardContext) -> String {
    // Core artifacts (always present in the pack)
    let core: Vec<(&str, &str)> = vec![
        ("10_diff/full.patch", "Full Patch"),
        ("20_quality/full-checks.log", "Full Checks Log"),
        ("20_quality/checks-errors.log", "Checks Errors"),
        ("20_quality/BREAKING_CHANGES.md", "Breaking Changes"),
        ("20_quality/coverage-delta.txt", "Coverage Delta"),
        ("30_context/INLINE_FINDINGS.sarif", "Inline Findings"),
        ("30_context/changed-tests.txt", "Changed Tests"),
        ("00_summary/MERGE_GATE.json", "Merge Gate (JSON)"),
        ("00_summary/MERGE_GATE.md", "Merge Gate (MD)"),
        ("00_summary/RUN.json", "Run Metadata"),
        ("00_summary/MANIFEST.json", "Manifest"),
        ("00_summary/SANITY.json", "Sanity Report"),
        (
            "20_quality/heuristics_loctree.log",
            "Loctree Heuristics Log",
        ),
        (
            "20_quality/heuristics_loctree.result.json",
            "Loctree Heuristics Result",
        ),
        ("report.json", "Report Data"),
        ("dashboard.html", "Dashboard"),
        ("PR_REVIEW.md", "PR Review"),
        ("AI_INDEX.md", "AI Review Index"),
        ("artifacts.zip", "Download ZIP"),
    ];

    // Per-file diff patches from DashboardContext
    let mut all_items: Vec<(String, String, &str)> = Vec::new(); // (path, label, kind)
    for (path, label) in &core {
        let kind = artifact_kind(path);
        all_items.push((path.to_string(), label.to_string(), kind));
    }
    for patch in &ctx.per_file_diff_files {
        let path = format!("10_diff/per-file-diffs/{}", patch);
        let label = decode_patch_label(patch);
        all_items.push((path, label, "PATCH"));
    }

    let total = all_items.len();

    // Collect unique kinds for filter chips
    let mut kinds: Vec<&str> = all_items.iter().map(|(_, _, k)| *k).collect();
    kinds.sort_unstable();
    kinds.dedup();
    let mut kind_chips = String::new();
    for kind in &kinds {
        let _ = write!(
            kind_chips,
            r#"<span class="filter-chip artifact-kind-chip" data-kind="{k}">{k}</span>"#,
            k = kind,
        );
    }

    // Build table rows
    let mut rows = String::new();
    for (path, label, kind) in &all_items {
        let _ = write!(
            rows,
            r#"<tr class="artifact-row" data-kind="{kind}" data-path="{path}">
    <td><a href="{path}" style="color:var(--accent);text-decoration:none;font-family:var(--mono);font-size:12px" target="_blank">{label}</a></td>
    <td><span class="badge badge-muted" style="font-size:10px">{kind}</span></td>
    <td style="font-family:var(--mono);font-size:11px;color:var(--faint)">{path}</td>
    <td><button class="btn-ghost artifact-copy-btn" data-i18n="button.copyShort" style="font-size:10px;padding:2px 6px">Copy</button></td>
</tr>"#,
            kind = kind,
            path = escape_html(path),
            label = escape_html(label),
        );
    }

    format!(
        r#"<div class="section section-system" id="section-artifacts">
    <div class="section-header">
        <span class="section-title" data-i18n="section.artifactsExplorer">Artifacts Explorer</span>
        <span class="section-count">{files_count}</span>
    </div>
    <div class="card" style="padding:0;overflow:hidden">
        <div style="padding:10px 16px;border-bottom:1px solid var(--line);display:flex;gap:8px;align-items:center;flex-wrap:wrap">
            <input type="text" id="artifact-search" class="file-search" placeholder="Search artifacts..." data-i18n-placeholder="placeholder.searchArtifacts" style="max-width:260px" />
            <div style="display:flex;gap:4px;flex-wrap:wrap">{kind_chips}</div>
        </div>
        <div style="max-height:400px;overflow-y:auto">
        <table class="checks-table" id="artifacts-table">
            <thead><tr><th data-i18n="label.name">Name</th><th data-i18n="label.kind">Kind</th><th data-i18n="label.path">Path</th><th></th></tr></thead>
            <tbody>{rows}</tbody>
        </table>
        </div>
    </div>
</div>"#,
        files_count = i18n_template(
            "count.files",
            &format!("{} files", total),
            &[("count", total.to_string())],
        ),
        kind_chips = kind_chips,
        rows = rows,
    )
}

// ---------------------------------------------------------------------------
// PRV-101: Merge Decision Card (first card in Action Center)
// ---------------------------------------------------------------------------

pub(super) fn build_merge_decision_card(ctx: &DashboardContext) -> String {
    let decision = merge_decision_view(ctx);
    let has_review_caveats = !decision.review_caveats.is_empty();

    // Policy badge
    let (policy_class, policy_label) = if ctx.policy_allow_merge {
        (
            "mdb-pass",
            i18n_template("badge.policyAllow", "Policy: ALLOW", &[]),
        )
    } else {
        (
            "mdb-fail",
            i18n_template("badge.policyBlock", "Policy: BLOCK", &[]),
        )
    };

    // Quality badge
    let (quality_class, quality_label) = if ctx.quality_pass {
        (
            "mdb-pass",
            i18n_template("badge.qualityPass", "Quality: PASS", &[]),
        )
    } else {
        (
            "mdb-fail",
            i18n_template("badge.qualityFail", "Quality: FAIL", &[]),
        )
    };

    // Merge badge: GO / HOLD / BLOCK
    let merge_class = decision.state.card_badge_class();
    let merge_fallback = format!("Merge: {}", decision.state.card_label());
    let merge_label = match decision.state {
        crate::artifacts::MergeDecisionState::Allow => {
            i18n_template("badge.mergeGo", merge_fallback.as_str(), &[])
        }
        crate::artifacts::MergeDecisionState::AllowWithReview => {
            i18n_template("badge.mergeGoReview", merge_fallback.as_str(), &[])
        }
        crate::artifacts::MergeDecisionState::Hold => {
            i18n_template("badge.mergeHold", merge_fallback.as_str(), &[])
        }
        crate::artifacts::MergeDecisionState::Block => {
            i18n_template("badge.mergeBlock", merge_fallback.as_str(), &[])
        }
    };
    let reason = decision.reason.clone();
    let card_class = decision.state.card_class();

    let caveat_html = if has_review_caveats {
        // Amber stays only on the small "Review signals" label marker; the
        // full caveat prose reads as normal (--fg) text, not a warn-colored wall.
        format!(
            r#"<div class="merge-decision-reason merge-policy-line review-signal-full" style="margin-top:8px"><span class="merge-policy-label review-signal-label" data-i18n="message.reviewSignalsPrefix">Review signals</span><span class="merge-policy-value review-signal-prose" data-review-signals="{}">{}</span></div>"#,
            escape_html(&decision.review_caveats.join(" · ")),
            escape_html(&decision.review_caveats.join(" · "))
        )
    } else {
        String::new()
    };

    format!(
        r#"<div class="action-card merge-decision-card {card_class}">
    <div class="action-card-header" data-i18n="section.mergeDecision">Merge Decision</div>
    <div class="merge-decision-badges">
        <span class="merge-decision-badge {pc}">{pl}</span>
        <span class="merge-decision-badge {qc}">{ql}</span>
        <span class="merge-decision-badge {mc}">{ml}</span>
    </div>
    <div class="merge-decision-reason" data-decision-reason="{reason}">{reason}</div>
    {caveat}
</div>"#,
        card_class = card_class,
        pc = policy_class,
        pl = policy_label,
        qc = quality_class,
        ql = quality_label,
        mc = merge_class,
        ml = merge_label,
        reason = escape_html(&reason),
        caveat = caveat_html,
    )
}

// ---------------------------------------------------------------------------
// PRV-102: Top 3 Blockers
// ---------------------------------------------------------------------------

/// Suggest a debug command based on check name.
pub(super) fn debug_hint_for_check(name: &str) -> &'static str {
    let lower = name.to_ascii_lowercase();
    if lower.contains("clippy") {
        "cargo clippy --fix"
    } else if lower.contains("test") {
        "cargo test -- --nocapture"
    } else if lower.contains("check") && lower.contains("cargo") {
        "cargo check 2>&1 | head -50"
    } else if lower.contains("build") || lower.contains("compile") {
        "cargo build 2>&1 | head -50"
    } else if lower.contains("fmt") || lower.contains("format") {
        "cargo fmt"
    } else if lower.contains("eslint") || lower.contains("lint") {
        "npx eslint --fix ."
    } else if lower.contains("tsc") || lower.contains("typescript") {
        "npx tsc --noEmit"
    } else if lower.contains("ruff") {
        "ruff check --fix ."
    } else if lower.contains("mypy") {
        "mypy --show-error-codes ."
    } else if lower.contains("pytest") {
        "pytest -x --tb=short"
    } else if lower.contains("geiger") {
        "cargo geiger --all-features"
    } else {
        "Re-run locally with verbose output"
    }
}

pub(super) fn build_blockers_section(ctx: &DashboardContext, checks: &[CheckResult]) -> String {
    // Collect failed/errored checks, sorted by severity (Error > Fail)
    let mut blockers: Vec<&CheckResult> = checks
        .iter()
        .filter(|c| matches!(c.status, CheckStatus::Error | CheckStatus::Failed))
        .collect();

    // Sort: Error first, then Failed
    blockers.sort_by(|a, b| {
        let priority = |s: CheckStatus| -> u8 {
            match s {
                CheckStatus::Error => 0,
                CheckStatus::Failed => 1,
                _ => 2,
            }
        };
        priority(a.status).cmp(&priority(b.status))
    });

    blockers.truncate(3);

    if blockers.is_empty() {
        return String::new();
    }

    let mut cards = String::new();
    for check in &blockers {
        let gate = ctx.check_gates.iter().find(|g| g.name == check.name);
        let gate_id = gate.map(|g| g.id.as_str()).unwrap_or("");

        let status_badge_class = check_badge_class(check.status);
        let status_str = check.status.as_str();

        // Extract first 2 non-empty lines from output as cause summary
        let cause_lines: Vec<&str> = check
            .output
            .lines()
            .filter(|l| !l.trim().is_empty())
            .take(2)
            .collect();
        let cause_html: String = cause_lines
            .iter()
            .map(|l| {
                let end = l.floor_char_boundary(120);
                escape_html(&l[..end])
            })
            .collect::<Vec<_>>()
            .join("<br>");

        let log_link = if !gate_id.is_empty() {
            format!(
                r#"<a href="20_quality/{}.log" data-i18n="message.viewFullLog">View full log</a>"#,
                escape_html(gate_id)
            )
        } else {
            String::new()
        };

        let hint = debug_hint_for_check(&check.name);
        let hint_html = if hint == "Re-run locally with verbose output" {
            i18n_template(
                "message.rerunLocallyVerbose",
                "Re-run locally with verbose output",
                &[],
            )
        } else {
            escape_html(hint)
        };

        let _ = write!(
            cards,
            r#"<div class="blocker-card">
    <div class="blocker-card-header">
        <span class="check-icon {skey}">{icon}</span>
        {name}
        <span class="badge {bc}">{status}</span>
    </div>
    <div class="blocker-card-body">{cause}</div>
    <div class="blocker-card-footer">
        {log_link}
        <span class="blocker-debug-hint">{hint}</span>
    </div>
</div>"#,
            skey = check.status.as_str(),
            icon = check_icon(check.status),
            name = escape_html(&check.name),
            bc = status_badge_class,
            status = escape_html(status_str),
            cause = if cause_html.is_empty() {
                i18n_template("message.noDetailsAvailable", "No details available", &[])
            } else {
                cause_html
            },
            log_link = log_link,
            hint = hint_html,
        );
    }

    format!(
        r#"<div class="section" id="section-blockers">
    <div class="section-header">
        <span class="section-title" data-i18n="section.blockers">Blockers</span>
        <span class="section-count">{count}</span>
    </div>
    <div class="blockers-grid">{cards}</div>
</div>"#,
        count = blockers.len(),
        cards = cards,
    )
}

// ---------------------------------------------------------------------------
// PRV-202: Security Lens
// ---------------------------------------------------------------------------

/// Check if a check result is security-related based on its name.
pub(super) fn is_security_check(name: &str) -> bool {
    let lower = name.to_lowercase();
    lower.contains("audit")
        || lower.contains("geiger")
        || lower.contains("security")
        || lower.contains("semgrep")
        || lower.contains("snyk")
}

/// Extract a key metric from a security check's output.
pub(super) fn extract_security_metric(name: &str, output: &str) -> String {
    let lower = name.to_lowercase();
    if lower.contains("audit") {
        // Try to parse vulnerability count from JSON output
        for line in output.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with('{')
                && let Ok(parsed) = serde_json::from_str::<serde_json::Value>(trimmed)
            {
                if let Some(count) = parsed
                    .get("vulnerabilities")
                    .and_then(|v| v.get("count"))
                    .and_then(|v| v.as_u64())
                {
                    return if count == 0 {
                        "0 vulnerabilities".to_string()
                    } else {
                        format!(
                            "{} vulnerabilit{}",
                            count,
                            if count == 1 { "y" } else { "ies" }
                        )
                    };
                }
                if let Some(list) = parsed
                    .get("vulnerabilities")
                    .and_then(|v| v.get("list"))
                    .and_then(|v| v.as_array())
                {
                    let count = list.len();
                    return if count == 0 {
                        "0 vulnerabilities".to_string()
                    } else {
                        format!(
                            "{} vulnerabilit{}",
                            count,
                            if count == 1 { "y" } else { "ies" }
                        )
                    };
                }
            }
        }
        // Fallback: look for text patterns
        for line in output.lines() {
            let ll = line.to_lowercase();
            if ll.contains("vulnerabilit") {
                let end = line.trim().len().min(80);
                let end = line.trim().floor_char_boundary(end);
                return line.trim()[..end].to_string();
            }
        }
        if output.contains("0 vulnerabilities")
            || output.to_lowercase().contains("no vulnerabilities")
        {
            return "0 vulnerabilities".to_string();
        }
        return "See log for details".to_string();
    }
    if lower.contains("geiger") {
        // Look for unsafe ratio lines
        for line in output.lines() {
            let trimmed = line.trim();
            if trimmed.contains("unsafe")
                || (trimmed.contains('/') && trimmed.chars().any(|c| c.is_ascii_digit()))
            {
                let end = trimmed.len().min(80);
                let end = trimmed.floor_char_boundary(end);
                return trimmed[..end].to_string();
            }
        }
        return "See log for details".to_string();
    }
    "See log for details".to_string()
}

pub(super) fn security_card_class(status: CheckStatus) -> &'static str {
    match status {
        CheckStatus::Passed => "sec-pass",
        CheckStatus::Failed => "sec-fail",
        CheckStatus::Warnings => "sec-warn",
        CheckStatus::Error | CheckStatus::Skipped => "sec-error",
    }
}

pub(super) fn build_security_section(checks: &[CheckResult], ctx: &DashboardContext) -> String {
    let security_checks: Vec<_> = checks
        .iter()
        .filter(|c| is_security_check(&c.name))
        .collect();

    if security_checks.is_empty() {
        return String::new();
    }

    // Overall status: worst non-skipped status wins.
    // Skipped checks are excluded from the fold entirely.
    let skipped_count = security_checks
        .iter()
        .filter(|c| c.status == CheckStatus::Skipped)
        .count();
    let non_skipped: Vec<_> = security_checks
        .iter()
        .filter(|c| c.status != CheckStatus::Skipped)
        .collect();

    let overall_status =
        non_skipped
            .iter()
            .map(|c| c.status)
            .fold(None, |worst: Option<CheckStatus>, s| {
                Some(match (worst, s) {
                    (Some(CheckStatus::Failed), _) | (_, CheckStatus::Failed) => {
                        CheckStatus::Failed
                    }
                    (Some(CheckStatus::Error), _) | (_, CheckStatus::Error) => CheckStatus::Error,
                    (Some(CheckStatus::Warnings), _) | (_, CheckStatus::Warnings) => {
                        CheckStatus::Warnings
                    }
                    _ => CheckStatus::Passed,
                })
            });

    let (overall_badge_class, overall_label) = match overall_status {
        Some(s) => (
            check_badge_class(s),
            match s {
                CheckStatus::Passed => "PASS",
                CheckStatus::Failed => "FAIL",
                CheckStatus::Warnings => "WARN",
                CheckStatus::Error => "ERROR",
                CheckStatus::Skipped => "SKIP",
            },
        ),
        // All checks were skipped
        None => ("badge-muted", "SKIP"),
    };

    let skipped_note = if skipped_count > 0 {
        format!(
            r#" <span class="badge badge-muted">{}</span>"#,
            i18n_template(
                "count.skipped",
                &format!("{skipped_count} skipped"),
                &[("count", skipped_count.to_string())],
            ),
        )
    } else {
        String::new()
    };

    let mut cards_html = String::new();
    for check in &security_checks {
        let card_class = security_card_class(check.status);
        let icon = check_icon(check.status);
        let metric = extract_security_metric(&check.name, &check.output);
        let metric_html = if metric == "See log for details" {
            i18n_template("message.seeLogForDetails", "See log for details", &[])
        } else {
            escape_html(&metric)
        };

        // Find the gate ID for log link
        let gate = ctx.check_gates.iter().find(|g| g.name == check.name);
        let log_link = match gate {
            Some(g) => format!(
                r#"<div class="security-card-link"><a href="20_quality/{}.log" data-i18n="message.viewFullLog">View full log</a></div>"#,
                escape_html(&g.id)
            ),
            None => String::new(),
        };

        let _ = write!(
            cards_html,
            r#"<div class="security-card {card_class}">
    <div class="security-card-header">{icon} {name} <span class="badge {bc}">{status}</span></div>
    <div class="security-card-metric">{metric}</div>
    {log_link}
</div>"#,
            card_class = card_class,
            icon = icon,
            name = escape_html(&check.name),
            bc = check_badge_class(check.status),
            status = check.status.as_str(),
            metric = metric_html,
            log_link = log_link,
        );
    }

    format!(
        r#"<div class="section" id="section-security">
    <div class="section-header">
        <span class="section-title" data-i18n="section.security">Security</span>
        <span class="badge {overall_bc}">{overall_label}</span>{skipped_note}
    </div>
    <div class="security-grid">{cards}</div>
</div>"#,
        overall_bc = overall_badge_class,
        overall_label = overall_label,
        skipped_note = skipped_note,
        cards = cards_html,
    )
}

// ---------------------------------------------------------------------------
// PRV-205: Diff-aware Lint Metrics
// ---------------------------------------------------------------------------

pub(super) fn build_lint_metrics_section(ctx: &DashboardContext, diffs: &[Diff]) -> String {
    let metrics = &ctx.lint_metrics;

    // No lint checks ran at all — section hidden
    if metrics.is_empty() {
        return String::new();
    }

    // Check if we have diff data
    let has_diff_data = !diffs.is_empty() && diffs.iter().any(|d| !d.files.is_empty());

    // All lint checks are clean (zero issues everywhere)
    let all_clean = metrics.iter().all(|m| m.total_issues == 0);

    if !has_diff_data {
        return r#"<div class="section" id="section-lint">
    <div class="section-header">
        <span class="section-title" data-i18n="section.lint">Lint</span>
        <span class="badge badge-muted">N/A</span>
    </div>
    <div class="lint-nodata-msg" data-i18n="message.lintMetricsUnavailable">Lint metrics unavailable &mdash; missing change data</div>
</div>"#
            .to_string();
    }

    if all_clean {
        return format!(
            r#"<div class="section" id="section-lint">
    <div class="section-header">
        <span class="section-title" data-i18n="section.lint">Lint</span>
        <span class="badge badge-success" data-i18n="label.clean">CLEAN</span>
    </div>
    <div class="lint-clean-msg">&#x2713; {clean_msg}</div>
</div>"#,
            clean_msg = i18n_template(
                "message.cleanLintAcrossChecks",
                &format!(
                    "Clean lint — no issues found across {} checks",
                    metrics.len()
                ),
                &[("count", metrics.len().to_string())],
            ),
        );
    }

    // Build cards for each lint check with issues
    let total_new: usize = metrics.iter().map(|m| m.new_issues).sum();
    let total_legacy: usize = metrics.iter().map(|m| m.legacy_issues).sum();
    let total_all: usize = metrics.iter().map(|m| m.total_issues).sum();

    let overall_badge = if total_new > 0 {
        r#"<span class="badge badge-warning" data-i18n="label.newIssues">NEW ISSUES</span>"#
    } else if total_legacy > 0 {
        r#"<span class="badge badge-muted" data-i18n="label.legacyOnly">LEGACY ONLY</span>"#
    } else {
        r#"<span class="badge badge-success" data-i18n="label.clean">CLEAN</span>"#
    };

    let mut cards_html = String::new();
    for m in metrics {
        let card_class = if m.total_issues == 0 {
            "lint-card lint-clean"
        } else if m.new_issues > 0 {
            "lint-card lint-new"
        } else {
            "lint-card lint-mixed"
        };

        let icon = if m.total_issues == 0 {
            "&#x2713;"
        } else if m.new_issues > 0 {
            "&#x26A0;"
        } else {
            "&#x2139;"
        };

        let mut card = String::new();
        let _ = write!(
            card,
            r#"<div class="{card_class}"><div class="lint-card-header">{icon} {name}"#,
            card_class = card_class,
            icon = icon,
            name = escape_html(&m.check_name),
        );

        if m.total_issues == 0 {
            let _ = write!(
                card,
                r#" <span class="badge badge-success" data-i18n="label.clean">CLEAN</span>"#
            );
        } else if m.new_issues > 0 {
            let _ = write!(
                card,
                r#" <span class="badge badge-warning">{}</span>"#,
                i18n_template(
                    "summary.newCount",
                    &format!("{} new", m.new_issues),
                    &[("count", m.new_issues.to_string())],
                )
            );
        }
        card.push_str("</div>");

        if m.total_issues > 0 {
            let _ = write!(card, r#"<div class="lint-card-stats">"#);
            if m.new_issues > 0 {
                let _ = write!(
                    card,
                    r#"<span class="lint-stat-new">{}</span>"#,
                    i18n_template(
                        "message.lintNewInChangedFiles",
                        &format!("{} new in changed files", m.new_issues),
                        &[("count", m.new_issues.to_string())],
                    ),
                );
            }
            if m.legacy_issues > 0 {
                let _ = write!(
                    card,
                    r#"<span class="lint-stat-legacy">{}</span>"#,
                    i18n_template(
                        "message.lintLegacyPreExisting",
                        &format!("{} legacy (pre-existing)", m.legacy_issues),
                        &[("count", m.legacy_issues.to_string())],
                    ),
                );
            }
            card.push_str("</div>");

            if !m.changed_files_with_issues.is_empty() {
                let _ = write!(
                    card,
                    r#"<div class="lint-card-files"><details><summary>{}</summary><ul>"#,
                    i18n_template(
                        "message.lintChangedFilesWithIssues",
                        &format!("Files with issues: {}", m.changed_files_with_issues.len()),
                        &[("count", m.changed_files_with_issues.len().to_string())],
                    ),
                );
                for file in &m.changed_files_with_issues {
                    let _ = write!(card, "<li>{}</li>", escape_html(file));
                }
                card.push_str("</ul></details></div>");
            }
        }

        card.push_str("</div>");
        cards_html.push_str(&card);
    }

    format!(
        r#"<div class="section" id="section-lint">
    <div class="section-header">
        <span class="section-title" data-i18n="section.lint">Lint</span>
        {badge}
        <span class="section-count">{count_summary}</span>
    </div>
    <div class="lint-grid">{cards}</div>
</div>"#,
        badge = overall_badge,
        count_summary = i18n_template(
            "summary.lintTotals",
            &format!("{total_new} new / {total_legacy} legacy / {total_all} total"),
            &[
                ("new", total_new.to_string()),
                ("legacy", total_legacy.to_string()),
                ("total", total_all.to_string()),
            ],
        ),
        cards = cards_html,
    )
}

// ---------------------------------------------------------------------------
// PRV-103: Time Budget per check
// ---------------------------------------------------------------------------

pub(super) fn build_time_budget(checks: &[CheckResult]) -> String {
    if checks.is_empty() {
        return String::new();
    }

    let max_duration = checks.iter().map(|c| c.duration).max().unwrap_or_default();
    let max_ms = max_duration.as_millis().max(1) as f64;

    // Find the slowest check index
    let slowest_idx = checks
        .iter()
        .enumerate()
        .max_by_key(|(_, c)| c.duration)
        .map(|(i, _)| i)
        .unwrap_or(0);

    let total: std::time::Duration = checks.iter().map(|c| c.duration).sum();

    let mut rows = String::new();
    for (idx, check) in checks.iter().enumerate() {
        let pct = (check.duration.as_millis() as f64 / max_ms) * 100.0;
        let dur_secs = check.duration.as_secs_f32();
        let is_slowest = idx == slowest_idx && checks.len() > 1;

        let bar_class = if check.cached {
            "time-budget-bar tb-cached"
        } else if is_slowest {
            "time-budget-bar tb-slowest"
        } else {
            "time-budget-bar"
        };

        let cached_label = if check.cached {
            r#" <span class="badge badge-muted" data-i18n="label.cached">(cached)</span>"#
                .to_string()
        } else {
            String::new()
        };
        let slowest_label = if is_slowest {
            r#" <span class="badge badge-warning" data-i18n="label.slowest">slowest</span>"#
        } else {
            ""
        };

        let _ = write!(
            rows,
            r#"<div class="time-budget-row">
    <span class="time-budget-name" title="{name}">{name}</span>
    <span class="time-budget-bar-wrap"><span class="{bar_class}" style="width:{pct:.1}%"></span></span>
    <span class="time-budget-duration">{dur:.1}s{cached}{slowest}</span>
</div>"#,
            name = escape_html(&check.name),
            bar_class = bar_class,
            pct = pct,
            dur = dur_secs,
            cached = cached_label,
            slowest = slowest_label,
        );
    }

    format!(
        r#"<div class="section" id="section-time-budget">
    <div class="section-header">
        <span class="section-title" data-i18n="section.timeBudget">Time Budget</span>
        <span class="section-count">{count_label}</span>
    </div>
    <div class="card">
        <div class="time-budget-chart">{rows}</div>
        <div class="time-budget-total"><span data-i18n="label.total">Total</span>: {total:.1}s</div>
    </div>
</div>"#,
        count_label = i18n_template(
            "count.checks",
            &format!("{} checks", checks.len()),
            &[("count", checks.len().to_string())],
        ),
        rows = rows,
        total = total.as_secs_f32(),
    )
}
