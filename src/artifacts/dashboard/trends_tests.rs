use super::*;
use crate::artifacts::LintMetrics;
use crate::checks::{CheckResult, CheckStatus};
use crate::config::{Config, test_config, test_rust_profile};
use crate::git::{CommitInfo, Diff, DiffStats, FileChange, FileStatus};
use crate::policy::PolicyConfig;
use std::time::Duration;

macro_rules! build_html_test {
    ($config:expr, $diffs:expr, $checks:expr, $heuristics:expr, $ctx:expr, $report_json:expr, $dir:expr, $pr_review:expr, $regression:expr $(,)?) => {
        build_html(BuildHtmlInput {
            config: $config,
            diffs: $diffs,
            checks: $checks,
            heuristics: $heuristics,
            ctx: $ctx,
            report_json: $report_json,
            dir: $dir,
            pr_review: $pr_review,
            regression: $regression,
        })
    };
}

fn mock_config() -> Config {
    let mut config = test_config();
    config.target = Some("feature/test".to_string());
    config.bases = vec!["main".to_string()];
    config.profile = test_rust_profile(true);
    config.do_fetch = false;
    config.use_cache = false;
    config.create_zip = false;
    config.policy = PolicyConfig::default();
    config
}

fn mock_diff() -> Diff {
    Diff {
        base: "main".into(),
        target: "feature/test".into(),
        base_commit_id: "aaa".into(),
        target_commit_id: "bbb".into(),
        stats: DiffStats {
            files_changed: 3,
            additions: 150,
            deletions: 30,
            copied: 0,
        },
        files: vec![FileChange {
            path: "src/main.rs".into(),
            status: FileStatus::Modified,
            additions: 100,
            deletions: 20,
        }],
        commits: vec![CommitInfo {
            id: "aabbcc".into(),
            short_id: "aabbc".into(),
            message: "test commit".into(),
            author: "dev".into(),
            email: "dev@test.com".into(),
            date: "2026-01-01".into(),
        }],
    }
}

fn mock_checks() -> Vec<CheckResult> {
    vec![CheckResult {
        name: "cargo check".into(),
        status: CheckStatus::Passed,
        duration: Duration::from_secs(2),
        output: "ok".into(),
        cached: false,
        provenance: None,
    }]
}

fn mock_ctx() -> DashboardContext {
    DashboardContext {
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
        policy_mode: "standard",
        blocking_issues: vec![],
        check_gates: vec![],
        breaking: vec![],
        coverage: super::super::CoverageDelta {
            pct: 100,
            total_source: 1,
            covered_count: 1,
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
    }
}

#[test]
fn test_trends_empty_history() {
    let ctx = mock_ctx();
    let html = build_trends_section(&ctx);
    assert!(html.is_empty(), "Empty history should produce empty string");
}

#[test]
fn test_trends_single_run() {
    use super::super::HistoricalRun;
    let mut ctx = mock_ctx();
    ctx.run_history = vec![HistoricalRun {
        timestamp: "20260302-143022".into(),
        checks_passed: 5,
        checks_failed: 0,
        checks_warned: 1,
        quality_pass: true,
        findings_count: 0,
    }];
    let html = build_trends_section(&ctx);
    assert!(
        html.is_empty(),
        "Single run should produce empty string (need 2+)"
    );
}

#[test]
fn test_trends_multiple_runs() {
    use super::super::HistoricalRun;
    let mut ctx = mock_ctx();
    ctx.run_history = vec![
        HistoricalRun {
            timestamp: "20260302-160000".into(),
            checks_passed: 6,
            checks_failed: 0,
            checks_warned: 1,
            quality_pass: true,
            findings_count: 0,
        },
        HistoricalRun {
            timestamp: "20260302-143022".into(),
            checks_passed: 4,
            checks_failed: 2,
            checks_warned: 1,
            quality_pass: false,
            findings_count: 3,
        },
        HistoricalRun {
            timestamp: "20260301-120000".into(),
            checks_passed: 3,
            checks_failed: 3,
            checks_warned: 0,
            quality_pass: false,
            findings_count: 5,
        },
    ];
    let html = build_trends_section(&ctx);

    assert!(html.contains("Historical Trends"), "Should have title");
    assert!(html.contains("section-trends"), "Should have section id");
    assert!(html.contains("3 runs"), "Should show run count");
    assert!(html.contains("latest"), "Should show latest badge");
    assert!(
        html.contains("03-02 16:00"),
        "Should format newest timestamp"
    );
    assert!(
        html.contains("03-01 12:00"),
        "Should format oldest timestamp"
    );
    assert!(html.contains("trends-bar-pass"), "Should have pass bars");
    assert!(html.contains("trends-bar-fail"), "Should have fail bars");
    assert!(
        html.contains("trend-improving"),
        "Latest run improved (0 failed vs avg 2.5)"
    );
}

#[test]
fn test_trends_degrading() {
    use super::super::HistoricalRun;
    let mut ctx = mock_ctx();
    ctx.run_history = vec![
        HistoricalRun {
            timestamp: "20260302-160000".into(),
            checks_passed: 2,
            checks_failed: 4,
            checks_warned: 0,
            quality_pass: false,
            findings_count: 10,
        },
        HistoricalRun {
            timestamp: "20260301-120000".into(),
            checks_passed: 5,
            checks_failed: 1,
            checks_warned: 0,
            quality_pass: true,
            findings_count: 2,
        },
    ];
    let html = build_trends_section(&ctx);

    assert!(
        html.contains("trend-degrading"),
        "Should show degrading trend"
    );
}

#[test]
fn test_trends_mixed_conflicting_signals() {
    use super::super::HistoricalRun;
    let mut ctx = mock_ctx();
    // Newest run has fewer failed checks (improving) but MORE findings
    // (worsening). The old improving-first logic hid this behind "Improving";
    // it must now be reported as "Mixed".
    ctx.run_history = vec![
        HistoricalRun {
            timestamp: "20260302-160000".into(),
            checks_passed: 6,
            checks_failed: 0,
            checks_warned: 0,
            quality_pass: false,
            findings_count: 10,
        },
        HistoricalRun {
            timestamp: "20260301-120000".into(),
            checks_passed: 2,
            checks_failed: 4,
            checks_warned: 0,
            quality_pass: false,
            findings_count: 2,
        },
    ];
    let html = build_trends_section(&ctx);

    assert!(
        html.contains("trend-mixed"),
        "Conflicting signals must render as mixed, not improving"
    );
    assert!(
        !html.contains("trend-improving"),
        "A regression in findings must not be badged as improving"
    );
}

#[test]
fn test_trends_stable() {
    use super::super::HistoricalRun;
    let mut ctx = mock_ctx();
    ctx.run_history = vec![
        HistoricalRun {
            timestamp: "20260302-160000".into(),
            checks_passed: 5,
            checks_failed: 1,
            checks_warned: 0,
            quality_pass: true,
            findings_count: 2,
        },
        HistoricalRun {
            timestamp: "20260301-120000".into(),
            checks_passed: 5,
            checks_failed: 1,
            checks_warned: 0,
            quality_pass: true,
            findings_count: 2,
        },
    ];
    let html = build_trends_section(&ctx);

    assert!(html.contains("trend-stable"), "Should show stable trend");
}

#[test]
fn test_trends_nav_conditional() {
    let config = mock_config();
    let diffs = vec![mock_diff()];
    let checks = mock_checks();
    let ctx = mock_ctx();
    let dir = std::env::temp_dir();

    let html = build_html_test!(&config, &diffs, &checks, None, &ctx, "", &dir, "", None);
    assert!(
        !html.contains(r##"href="#section-trends""##),
        "Nav should NOT have Trends link when no history"
    );
}

#[test]
fn test_trends_nav_present_with_history() {
    use super::super::HistoricalRun;
    let config = mock_config();
    let diffs = vec![mock_diff()];
    let checks = mock_checks();
    let mut ctx = mock_ctx();
    ctx.run_history = vec![
        HistoricalRun {
            timestamp: "20260302-160000".into(),
            checks_passed: 5,
            checks_failed: 0,
            checks_warned: 0,
            quality_pass: true,
            findings_count: 0,
        },
        HistoricalRun {
            timestamp: "20260301-120000".into(),
            checks_passed: 4,
            checks_failed: 1,
            checks_warned: 0,
            quality_pass: true,
            findings_count: 1,
        },
    ];
    let dir = std::env::temp_dir();

    let html = build_html_test!(&config, &diffs, &checks, None, &ctx, "", &dir, "", None);
    assert!(
        html.contains(r##"href="#section-trends""##),
        "Nav SHOULD have Trends link when history >= 2"
    );
    assert!(
        html.contains("Historical Trends"),
        "Should render trends section"
    );
}

#[test]
fn test_format_run_timestamp() {
    assert_eq!(format_run_timestamp("20260302-143022"), "03-02 14:30");
    assert_eq!(format_run_timestamp("20260115-091500"), "01-15 09:15");
    assert_eq!(format_run_timestamp("short"), "short");
}

// ---- PRV-203: Ownership Map ----

#[test]
fn test_ownership_section_groups_by_owner() {
    let mut ctx = mock_ctx();
    ctx.ownership_map = vec![
        ("src/main.rs".into(), "core".into()),
        ("src/lib.rs".into(), "core".into()),
        ("tests/test_foo.rs".into(), "testing".into()),
    ];
    let html = build_ownership_section(&ctx);

    assert!(html.contains("Ownership"), "Should have Ownership title");
    assert!(html.contains("2 owners"), "Should show 2 owners");
    assert!(html.contains("core"), "Should show core owner");
    assert!(html.contains("testing"), "Should show testing owner");
    assert!(html.contains("2 files"), "core should have 2 files");
    assert!(html.contains("1 file"), "testing should have 1 file");
}

#[test]
fn test_ownership_section_empty() {
    let mut ctx = mock_ctx();
    ctx.ownership_map = vec![];
    let html = build_ownership_section(&ctx);
    assert!(
        html.is_empty(),
        "Empty ownership map should produce empty HTML"
    );
}

#[test]
fn test_ownership_badge_in_files_section() {
    let diff = mock_diff(); // has src/main.rs
    let mut ctx = mock_ctx();
    ctx.ownership_map = vec![("src/main.rs".into(), "core".into())];
    let html = build_files_section(Some(&diff), &ctx);

    // Owner badges should appear in file rows
    assert!(
        html.contains("badge badge-muted"),
        "Should have owner badges"
    );
    assert!(html.contains(">core<"), "Should show 'core' owner badge");
}

// ---- PRV-204: Flaky Checks ----

#[test]
fn test_flaky_section_hidden_with_no_history() {
    let ctx = mock_ctx();
    let html = build_flaky_section(&ctx);
    assert!(html.is_empty(), "Should be empty with no run history");
}

#[test]
fn test_flaky_section_hidden_with_single_run() {
    use super::super::HistoricalRun;
    let mut ctx = mock_ctx();
    ctx.run_history = vec![HistoricalRun {
        timestamp: "20260302-143022".into(),
        checks_passed: 5,
        checks_failed: 0,
        checks_warned: 1,
        quality_pass: true,
        findings_count: 0,
    }];
    let html = build_flaky_section(&ctx);
    assert!(html.is_empty(), "Should be empty with only 1 run");
}

#[test]
fn test_flaky_section_stable_card() {
    use super::super::HistoricalRun;
    let mut ctx = mock_ctx();
    ctx.run_history = vec![
        HistoricalRun {
            timestamp: "20260302-160000".into(),
            checks_passed: 5,
            checks_failed: 0,
            checks_warned: 0,
            quality_pass: true,
            findings_count: 0,
        },
        HistoricalRun {
            timestamp: "20260302-143022".into(),
            checks_passed: 5,
            checks_failed: 0,
            checks_warned: 0,
            quality_pass: true,
            findings_count: 0,
        },
    ];
    // No flaky scores = stable
    ctx.flaky_scores = vec![];
    let html = build_flaky_section(&ctx);
    assert!(
        html.contains("All checks stable across 2 runs"),
        "Should show stable card"
    );
    assert!(html.contains("section-flaky"), "Should have section id");
}

#[test]
fn test_flaky_section_with_flaky_checks() {
    use super::super::{FlakyCheckScore, HistoricalRun};
    let mut ctx = mock_ctx();
    ctx.run_history = vec![
        HistoricalRun {
            timestamp: "20260302-160000".into(),
            checks_passed: 5,
            checks_failed: 1,
            checks_warned: 0,
            quality_pass: false,
            findings_count: 0,
        },
        HistoricalRun {
            timestamp: "20260302-143022".into(),
            checks_passed: 6,
            checks_failed: 0,
            checks_warned: 0,
            quality_pass: true,
            findings_count: 0,
        },
        HistoricalRun {
            timestamp: "20260301-120000".into(),
            checks_passed: 5,
            checks_failed: 1,
            checks_warned: 0,
            quality_pass: false,
            findings_count: 0,
        },
    ];
    ctx.flaky_scores = vec![FlakyCheckScore {
        check_id: "cargo_test".into(),
        check_name: "cargo test".into(),
        flaky_score: 1.0,
        total_runs: 3,
        transitions: 2,
        last_statuses: vec!["FAIL".into(), "PASS".into(), "FAIL".into()],
        confidence: "low",
    }];
    let html = build_flaky_section(&ctx);
    assert!(
        html.contains(r#"data-i18n="section.flakyChecks""#),
        "Should have localized title"
    );
    assert!(html.contains("1 flaky"), "Should show flaky count");
    assert!(html.contains("cargo test"), "Should show check name");
    assert!(html.contains("100%"), "Should show 100% flaky score");
    assert!(html.contains("flaky-dot-fail"), "Should have fail dots");
    assert!(html.contains("flaky-dot-pass"), "Should have pass dots");
    assert!(
        html.contains("flaky-confidence-low"),
        "Should show low confidence"
    );
}

#[test]
fn test_flaky_section_score_classes() {
    use super::super::{FlakyCheckScore, HistoricalRun};
    let mut ctx = mock_ctx();
    ctx.run_history = vec![
        HistoricalRun {
            timestamp: "20260302-160000".into(),
            checks_passed: 5,
            checks_failed: 0,
            checks_warned: 0,
            quality_pass: true,
            findings_count: 0,
        },
        HistoricalRun {
            timestamp: "20260302-143022".into(),
            checks_passed: 5,
            checks_failed: 0,
            checks_warned: 0,
            quality_pass: true,
            findings_count: 0,
        },
    ];
    ctx.flaky_scores = vec![
        FlakyCheckScore {
            check_id: "high".into(),
            check_name: "high score".into(),
            flaky_score: 0.75,
            total_runs: 5,
            transitions: 3,
            last_statuses: vec![
                "PASS".into(),
                "FAIL".into(),
                "PASS".into(),
                "FAIL".into(),
                "PASS".into(),
            ],
            confidence: "medium",
        },
        FlakyCheckScore {
            check_id: "mid".into(),
            check_name: "mid score".into(),
            flaky_score: 0.33,
            total_runs: 4,
            transitions: 1,
            last_statuses: vec!["PASS".into(), "PASS".into(), "PASS".into(), "FAIL".into()],
            confidence: "medium",
        },
        FlakyCheckScore {
            check_id: "low".into(),
            check_name: "low score".into(),
            flaky_score: 0.1,
            total_runs: 10,
            transitions: 1,
            last_statuses: vec!["PASS".into(); 10],
            confidence: "high",
        },
    ];
    let html = build_flaky_section(&ctx);
    assert!(
        html.contains("flaky-score-high"),
        "Score >= 0.5 should be high"
    );
    assert!(
        html.contains("flaky-score-mid"),
        "Score >= 0.25 should be mid"
    );
    assert!(
        html.contains("flaky-score-low"),
        "Score < 0.25 should be low"
    );
    assert!(html.contains("3 flaky"), "Should show 3 flaky checks");
}

#[test]
fn test_flaky_nav_link_conditional() {
    use super::super::HistoricalRun;
    let config = mock_config();
    let diffs = vec![mock_diff()];
    let checks = mock_checks();
    let mut ctx = mock_ctx();
    let dir = std::env::temp_dir();

    // No history => no flaky nav link
    let html = build_html_test!(&config, &diffs, &checks, None, &ctx, "", &dir, "", None);
    assert!(
        !html.contains(r##"href="#section-flaky""##),
        "Nav should NOT have Flaky link when no history"
    );

    // With history => flaky nav link present
    ctx.run_history = vec![
        HistoricalRun {
            timestamp: "20260302-160000".into(),
            checks_passed: 5,
            checks_failed: 0,
            checks_warned: 0,
            quality_pass: true,
            findings_count: 0,
        },
        HistoricalRun {
            timestamp: "20260302-143022".into(),
            checks_passed: 5,
            checks_failed: 0,
            checks_warned: 0,
            quality_pass: true,
            findings_count: 0,
        },
    ];
    let html = build_html_test!(&config, &diffs, &checks, None, &ctx, "", &dir, "", None);
    assert!(
        html.contains(r##"href="#section-flaky""##),
        "Nav SHOULD have Flaky link when history >= 2"
    );
}

// -----------------------------------------------------------------------
// PRV-205: Lint metrics dashboard tests
// -----------------------------------------------------------------------

#[test]
fn test_lint_section_hidden_when_no_lint_checks() {
    let ctx = mock_ctx();
    let diffs = vec![mock_diff()];
    let html = build_lint_metrics_section(&ctx, &diffs);
    assert!(html.is_empty(), "No lint checks = section hidden");
}

#[test]
fn test_lint_section_clean() {
    let mut ctx = mock_ctx();
    ctx.lint_metrics = vec![LintMetrics {
        check_name: "cargo clippy".into(),
        new_issues: 0,
        legacy_issues: 0,
        total_issues: 0,
        changed_files_with_issues: vec![],
    }];
    let diffs = vec![mock_diff()];
    let html = build_lint_metrics_section(&ctx, &diffs);
    assert!(html.contains("section-lint"), "Section should exist");
    assert!(
        html.contains(r#"data-i18n="label.clean""#),
        "Should localize clean badge"
    );
    assert!(
        html.contains(r#"data-i18n-template="message.cleanLintAcrossChecks""#),
        "Should localize clean helper copy"
    );
}

#[test]
fn test_lint_section_no_diff_data() {
    let mut ctx = mock_ctx();
    ctx.lint_metrics = vec![LintMetrics {
        check_name: "cargo clippy".into(),
        new_issues: 0,
        legacy_issues: 0,
        total_issues: 0,
        changed_files_with_issues: vec![],
    }];
    let diffs: Vec<Diff> = vec![];
    let html = build_lint_metrics_section(&ctx, &diffs);
    assert!(html.contains("section-lint"), "Section should exist");
    assert!(
        html.contains(r#"data-i18n="message.lintMetricsUnavailable""#),
        "Should localize fallback message"
    );
}

#[test]
fn test_lint_section_new_issues() {
    let mut ctx = mock_ctx();
    ctx.lint_metrics = vec![LintMetrics {
        check_name: "cargo clippy".into(),
        new_issues: 3,
        legacy_issues: 1,
        total_issues: 4,
        changed_files_with_issues: vec!["src/main.rs".into(), "src/lib.rs".into()],
    }];
    let diffs = vec![mock_diff()];
    let html = build_lint_metrics_section(&ctx, &diffs);
    assert!(html.contains("section-lint"), "Section should exist");
    assert!(
        html.contains(r#"data-i18n="label.newIssues""#),
        "Should localize NEW ISSUES badge"
    );
    assert!(
        html.contains(r#"data-i18n-template="message.lintNewInChangedFiles""#),
        "Should localize new issue count"
    );
    assert!(
        html.contains(r#"data-i18n-template="message.lintLegacyPreExisting""#),
        "Should localize legacy issue count"
    );
    assert!(
        html.contains(r#"data-i18n-template="message.lintChangedFilesWithIssues""#),
        "Should localize changed-files helper copy"
    );
    assert!(html.contains("src/main.rs"), "Should list affected files");
    assert!(html.contains("src/lib.rs"), "Should list affected files");
}

#[test]
fn test_lint_section_legacy_only() {
    let mut ctx = mock_ctx();
    ctx.lint_metrics = vec![LintMetrics {
        check_name: "ESLint".into(),
        new_issues: 0,
        legacy_issues: 5,
        total_issues: 5,
        changed_files_with_issues: vec![],
    }];
    let diffs = vec![mock_diff()];
    let html = build_lint_metrics_section(&ctx, &diffs);
    assert!(
        html.contains(r#"data-i18n="label.legacyOnly""#),
        "Should localize LEGACY ONLY badge"
    );
    assert!(
        html.contains(r#"data-i18n-template="message.lintLegacyPreExisting""#),
        "Should localize legacy count"
    );
    assert!(
        !html.contains(r#"data-i18n-template="message.lintNewInChangedFiles""#),
        "Should NOT show new count"
    );
}

#[test]
fn test_regression_details_use_localized_markers() {
    let ctx = mock_ctx();
    let report = crate::regression::compute_regression(&crate::regression::RegressionContext {
        files_changed: 2,
        insertions: 40,
        deletions: 10,
        file_stats: vec![
            ("src/main.rs".into(), 'M', 30, 10),
            ("src/lib.rs".into(), 'M', 10, 0),
        ],
        ..Default::default()
    });

    let html = build_regression_details_section(&ctx, None, Some(&report));

    assert!(
        html.contains(r#"data-i18n-template="summary.regressionScore""#),
        "Regression summary should be localizable"
    );
    assert!(
        html.contains(r#"data-i18n="label.hotspots""#),
        "Hotspots tab should be localizable"
    );
    assert!(
        html.contains(r#"data-i18n-template="summary.severityValue""#),
        "Severity label should be localizable"
    );
}

#[test]
fn test_lint_nav_link_conditional() {
    let dir = std::env::temp_dir().join("prv205-nav-test");
    let _ = std::fs::create_dir_all(&dir);
    let config = mock_config();
    let diffs = vec![mock_diff()];
    let checks = mock_checks();

    // Without lint metrics — no nav link
    let ctx = mock_ctx();
    let html = build_html_test!(&config, &diffs, &checks, None, &ctx, "", &dir, "", None);
    assert!(
        !html.contains(r##"href="#section-lint""##),
        "No lint = no nav link"
    );

    // With lint metrics — statistics stays the single sidebar entry
    let mut ctx2 = mock_ctx();
    ctx2.lint_metrics = vec![LintMetrics {
        check_name: "cargo clippy".into(),
        new_issues: 1,
        legacy_issues: 0,
        total_issues: 1,
        changed_files_with_issues: vec!["src/main.rs".into()],
    }];
    let html2 = build_html_test!(&config, &diffs, &checks, None, &ctx2, "", &dir, "", None);
    assert!(
        !html2.contains(r##"href="#section-lint""##),
        "Lint should be folded under Statistics in the sidebar"
    );
    assert!(
        html2.contains(r##"href="#section-statistics""##),
        "Statistics link should remain available"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

// ── file_category tests ──────────────────────────────────────────

#[test]
fn test_file_category_image_assets() {
    assert_eq!(file_category("assets/logo.png"), "asset");
    assert_eq!(file_category("public/icon.svg"), "asset");
    assert_eq!(file_category("img/photo.jpg"), "asset");
    assert_eq!(file_category("img/photo.jpeg"), "asset");
    assert_eq!(file_category("img/anim.gif"), "asset");
    assert_eq!(file_category("img/hero.webp"), "asset");
    assert_eq!(file_category("img/icon.ico"), "asset");
    assert_eq!(file_category("img/next.avif"), "asset");
    assert_eq!(file_category("img/old.bmp"), "asset");
}

#[test]
fn test_file_category_i18n_under_locales() {
    assert_eq!(file_category("src/locales/en/common.json"), "i18n");
    assert_eq!(file_category("src/i18n/fr/messages.json"), "i18n");
    assert_eq!(file_category("app/translations/de/strings.json"), "i18n");
}

#[test]
fn test_file_category_json_not_under_locales_is_non_code() {
    assert_eq!(file_category("src/data/config.json"), "non-code");
    assert_eq!(file_category("fixtures/data.json"), "non-code");
}

#[test]
fn test_file_category_lock_files_are_config() {
    assert_eq!(file_category("Cargo.lock"), "config");
    assert_eq!(file_category("yarn.lock"), "config");
    assert_eq!(file_category("package-lock.json"), "config");
    assert_eq!(file_category("pnpm-lock.yaml"), "config");
}

#[test]
fn test_file_category_yaml_toml_are_config() {
    assert_eq!(file_category("Cargo.toml"), "config");
    assert_eq!(file_category(".github/workflows/ci.yaml"), "config");
    assert_eq!(file_category(".github/workflows/ci.yml"), "config");
}

#[test]
fn test_file_category_source_code() {
    assert_eq!(file_category("src/main.rs"), "code");
    assert_eq!(file_category("src/app.ts"), "code");
    assert_eq!(file_category("src/components/Button.tsx"), "code");
}

#[test]
fn test_file_category_root_tests_dir() {
    assert_eq!(file_category("tests/json_contract.rs"), "test");
    assert_eq!(file_category("src/regression/tests.rs"), "test");
}

#[test]
fn test_file_category_specific_config_files() {
    assert_eq!(file_category("tsconfig.json"), "config");
    assert_eq!(file_category(".gitignore"), "config");
    assert_eq!(file_category("package.json"), "config");
    assert_eq!(file_category(".editorconfig"), "config");
    assert_eq!(file_category("Makefile"), "config"); // lowercased → "makefile"
    assert_eq!(file_category("Dockerfile"), "config"); // lowercased → "dockerfile"
}

#[test]
fn test_file_category_markdown_is_non_code() {
    assert_eq!(file_category("README.md"), "non-code");
    assert_eq!(file_category("docs/DESIGN.md"), "non-code");
}

#[test]
fn test_file_category_font_is_non_code() {
    assert_eq!(file_category("assets/Inter.woff2"), "non-code");
}

#[test]
fn test_extract_narrative_preview_strips_markdown_noise() {
    let md = r#"
## PR Review

> **Branch:** feat/x | **Base:** develop | **Profile:** Mixed
> **Commits:** 3 • **Files:** 12
"#;

    let preview = extract_narrative_preview(md);
    assert!(preview.contains("Branch: feat/x | Base: develop | Profile: Mixed"));
    assert!(!preview.contains("**"));
    assert!(!preview.contains('>'));
}

// ── extract_finding_location tests ───────────────────────────────

#[test]
fn test_extract_finding_path_line_message() {
    let (path, line, msg) = extract_finding_location("src/foo.rs:42: error message");
    assert_eq!(path, Some("src/foo.rs"));
    assert_eq!(line, Some(42));
    assert_eq!(msg, "error message");
}

#[test]
fn test_extract_finding_path_no_line() {
    let (path, line, msg) = extract_finding_location("src/foo.rs: message without line");
    assert_eq!(path, Some("src/foo.rs"));
    assert_eq!(line, None);
    assert_eq!(msg, "message without line");
}

#[test]
fn test_extract_finding_plain_message() {
    let (path, line, msg) = extract_finding_location("just a message");
    assert_eq!(path, None);
    assert_eq!(line, None);
    assert_eq!(msg, "just a message");
}

#[test]
fn test_extract_finding_empty_string() {
    let (path, line, msg) = extract_finding_location("");
    assert_eq!(path, None);
    assert_eq!(line, None);
    assert_eq!(msg, "");
}

#[test]
fn test_extract_finding_path_line_col() {
    // Pattern: path:line:col: message
    let (path, line, msg) = extract_finding_location("src/lib.rs:10:5: unused variable");
    assert_eq!(path, Some("src/lib.rs"));
    assert_eq!(line, Some(10));
    // After line 10, rest is "5: unused variable" — parser takes 5 as col, returns msg
    assert!(msg.contains("unused variable"));
}
