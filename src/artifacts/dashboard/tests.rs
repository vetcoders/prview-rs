use super::super::CheckGateEntry;
use super::super::signal::BreakingFinding;
use super::*;
use crate::checks::{CheckResult, CheckStatus, SkippedCheck};
use crate::config::{Config, test_config, test_rust_profile};
use crate::git::{CommitInfo, Diff, DiffStats, FileChange, FileStatus};
use crate::policy::PolicyConfig;
use regex::Regex;
use serde_json::json;
use std::fs;
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
        files: vec![
            FileChange {
                path: "src/main.rs".into(),
                status: FileStatus::Modified,
                additions: 100,
                deletions: 20,
            },
            FileChange {
                path: "src/lib.rs".into(),
                status: FileStatus::Modified,
                additions: 40,
                deletions: 5,
            },
            FileChange {
                path: "src/old.rs".into(),
                status: FileStatus::Deleted,
                additions: 0,
                deletions: 5,
            },
        ],
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
    vec![
        CheckResult {
            name: "cargo check".into(),
            status: CheckStatus::Passed,
            duration: Duration::from_secs(2),
            output: "ok".into(),
            cached: false,
            provenance: None,
        },
        CheckResult {
            name: "cargo clippy".into(),
            status: CheckStatus::Warnings,
            duration: Duration::from_secs(3),
            output: "2 warnings".into(),
            cached: false,
            provenance: None,
        },
    ]
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
        per_file_diff_files: vec!["abc12345__src~2Fmain.rs.patch".into()],
        skipped_checks: vec![SkippedCheck {
            id: "cargo_geiger".into(),
            name: "Cargo Geiger".into(),
            reason: "not installed".into(),
        }],
        previous_run: None,
        run_history: vec![],
        flaky_scores: vec![],
        lint_metrics: vec![],
        ownership_map: vec![
            ("src/main.rs".into(), "core".into()),
            ("src/lib.rs".into(), "core".into()),
            ("src/old.rs".into(), "core".into()),
        ],
        risk_scores: vec![],
        i18n_delta: None,
    }
}

#[test]
fn test_safe_id_deterministic() {
    // Slashes become _x2f_, dots become _x2e_
    assert_eq!(safe_id("src/main.rs"), "src_x2f_main_x2e_rs");
    assert_eq!(
        safe_id("src/artifacts/dashboard.rs"),
        "src_x2f_artifacts_x2f_dashboard_x2e_rs"
    );
    assert_eq!(safe_id("a/b/c.test.tsx"), "a_x2f_b_x2f_c_x2e_test_x2e_tsx");
    // Same input always produces same output
    assert_eq!(safe_id("src/main.rs"), safe_id("src/main.rs"));
}

#[test]
fn test_safe_id_no_special_chars() {
    let id = safe_id("src/[foo]/{bar}.rs");
    assert!(id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'));
}

#[test]
fn test_safe_id_no_collisions() {
    // Paths differing only in separator characters must produce different IDs
    assert_ne!(safe_id("src/foo-bar.rs"), safe_id("src/foo_bar.rs"));
    assert_ne!(safe_id("src/foo.bar"), safe_id("src/foo_bar"));
    assert_ne!(safe_id("a-b"), safe_id("a_b"));
    assert_ne!(safe_id("a/b"), safe_id("a_b"));
    assert_ne!(safe_id("a.b"), safe_id("a_b"));
}

#[test]
fn test_deep_links_consistency() {
    // Every href="#file-..." in the HTML must have a corresponding id="file-..."
    let config = mock_config();
    let diffs = vec![mock_diff()];
    let checks = mock_checks();
    let ctx = mock_ctx();
    let dir = std::env::temp_dir();

    let html = build_html_test!(&config, &diffs, &checks, None, &ctx, "", &dir, "", None);

    // Extract all href="#file-..." targets
    let href_re = Regex::new(r##"href="#file-([^"]+)""##).unwrap();
    let id_re = Regex::new(r##"id="file-([^"]+)""##).unwrap();

    let hrefs: std::collections::HashSet<String> = href_re
        .captures_iter(&html)
        .map(|c| c[1].to_string())
        .collect();
    let ids: std::collections::HashSet<String> = id_re
        .captures_iter(&html)
        .map(|c| c[1].to_string())
        .collect();

    for href in &hrefs {
        assert!(
            ids.contains(href),
            "href='#file-{}' has no matching id='file-{}'",
            href,
            href
        );
    }
}

#[test]
fn test_collapsible_wrappers_use_data_section_ids() {
    let config = mock_config();
    let diffs = vec![mock_diff()];
    let checks = mock_checks();
    let ctx = mock_ctx();
    let dir = std::env::temp_dir();

    let html = build_html_test!(&config, &diffs, &checks, None, &ctx, "", &dir, "", None);

    assert!(html.contains(r#"data-section-id="section-checks""#));
    assert!(html.contains(r#"data-section-id="section-files""#));
    assert!(html.contains(r#"data-section-id="section-artifacts""#));
    assert!(html.contains(r#"id="section-checks__body""#));
    assert!(html.contains(r#"id="section-files__body""#));
}

#[test]
fn test_build_html_stays_self_contained_without_external_font_cdns() {
    let config = mock_config();
    let diffs = vec![mock_diff()];
    let checks = mock_checks();
    let ctx = mock_ctx();
    let dir = std::env::temp_dir();

    let html = build_html_test!(&config, &diffs, &checks, None, &ctx, "", &dir, "", None);

    assert!(html.contains("system stack only, no external CDN dependency"));
    assert!(!html.contains("fonts.googleapis.com"));
    assert!(!html.contains("fonts.gstatic.com"));
}

#[test]
fn test_checks_summary_shows_warn() {
    let checks = mock_checks(); // 1 passed, 1 warnings
    let ctx = mock_ctx();
    let html = build_checks_section(&checks, &ctx);

    assert!(html.contains("1 passed"), "Should show passed count");
    assert!(html.contains("1 warn"), "Should show warn count");
    assert!(
        !html.contains("2/2 passed"),
        "Should NOT show old 'X/Y passed' format"
    );
}

#[test]
fn test_skipped_checks_rendered() {
    let checks = mock_checks();
    let ctx = mock_ctx(); // has 1 skipped check
    let html = build_checks_section(&checks, &ctx);

    assert!(
        html.contains(r#"data-i18n="label.skipped""#),
        "Should localize skipped label"
    );
    assert!(html.contains("(1)"), "Should show skipped count");
    assert!(
        html.contains("Cargo Geiger"),
        "Should show skipped check name"
    );
    assert!(html.contains("not installed"), "Should show skip reason");
}

#[test]
fn test_file_rows_have_ids() {
    let diff = mock_diff();
    let ctx = mock_ctx();
    let html = build_files_section(Some(&diff), &ctx);

    assert!(
        html.contains(&format!(r#"id="file-{}""#, safe_id("src/main.rs"))),
        "src/main.rs should have safe id"
    );
    assert!(
        html.contains(&format!(r#"id="file-{}""#, safe_id("src/lib.rs"))),
        "src/lib.rs should have safe id"
    );
    assert!(
        html.contains(&format!(r#"id="file-{}""#, safe_id("src/old.rs"))),
        "src/old.rs should have safe id"
    );
}

#[test]
fn test_files_section_defaults_to_code_hotspots() {
    let diff = mock_diff();
    let ctx = mock_ctx();
    let html = build_files_section(Some(&diff), &ctx);

    assert!(
        html.contains(r#"class="toggle-btn active" data-show="code""#),
        "Files section should default to Code view"
    );
    assert!(
        html.contains("non-code"),
        "Files summary should expose non-code count"
    );
}

#[test]
fn test_file_with_patch_has_data_attr() {
    let diff = mock_diff();
    let ctx = mock_ctx(); // has abc12345__src~2Fmain.rs.patch
    let html = build_files_section(Some(&diff), &ctx);

    assert!(
        html.contains("data-patch-path"),
        "File with patch should have data-patch-path"
    );
    assert!(
        html.contains("abc12345__src~2Fmain.rs.patch"),
        "Patch path should reference correct file with sanitized encoding"
    );
}

#[test]
fn test_artifacts_explorer_has_per_file_patches() {
    let ctx = mock_ctx();
    let html = build_artifacts_section(&ctx);

    assert!(
        html.contains("Artifacts Explorer"),
        "Should have Explorer title"
    );
    assert!(
        html.contains("abc12345__src~2Fmain.rs.patch"),
        "Should include per-file patch with sanitized encoding"
    );
    assert!(html.contains("artifact-search"), "Should have search input");
    assert!(
        html.contains("artifact-kind-chip"),
        "Should have kind filter chips"
    );
}

#[test]
fn test_header_merge_chip_shows_review_caveat() {
    let config = mock_config();
    let diff = mock_diff();
    let mut ctx = mock_ctx();
    ctx.check_gates = vec![CheckGateEntry {
        name: "cargo check".into(),
        id: "cargo".into(),
        blocking: false,
        class: "pass",
        severity: "warn",
    }];
    ctx.breaking = vec![BreakingFinding {
        file: "src/lib.rs".into(),
        kind: BreakingKind::RemovedSymbol {
            symbol_type: "function".into(),
        },
        line: "pub fn old_api()".into(),
        risk_level: BreakingRisk::High,
    }];
    ctx.coverage = super::super::CoverageDelta {
        pct: 11,
        total_source: 43,
        covered_count: 5,
        uncovered: vec![],
        covered: vec![],
        non_code_count: 18,
        ghost_tests: vec![],
    };
    let html = build_header(&config, Some(&diff), &ctx, Path::new("20260308-031035"), "");

    assert!(html.contains("ALLOW WITH REVIEW"));
    assert!(html.contains(r#"data-i18n-template="hero.mergeAllowWithReview""#));
    // De-choinka v2: the header carries only a compact amber-dot marker plus a
    // muted count. The full caveat prose lives in the Merge Decision card below,
    // so it is intentionally NOT duplicated in the above-the-fold header.
    assert!(html.contains("review-signal-dot"));
    assert!(html.contains(r#"data-i18n-template="summary.reviewSignalCount""#));
    assert!(html.contains(r#"data-count="2""#));
    assert!(html.contains("2 review signals"));
    assert!(!html.contains("1 removed public symbol"));
}

#[test]
fn test_header_merge_chip_shows_hold_when_not_recommended() {
    let config = mock_config();
    let diff = mock_diff();
    let mut ctx = mock_ctx();
    ctx.check_gates = vec![CheckGateEntry {
        name: "cargo check".into(),
        id: "cargo".into(),
        blocking: false,
        class: "pass",
        severity: "warn",
    }];
    ctx.quality_pass = false;
    ctx.recommended_merge = false;
    ctx.allow_merge = true;
    ctx.policy_allow_merge = true;
    ctx.quality_failures = vec!["cargo clippy".into()];

    let html = build_header(&config, Some(&diff), &ctx, Path::new("20260308-031035"), "");

    assert!(html.contains("HOLD MERGE"));
    assert!(!html.contains("BLOCK MERGE"));
}

#[test]
fn test_header_contains_dashboard_context_bar() {
    let mut config = mock_config();
    config.gh_repo = Some("vetcoders/prview".into());
    config.pr_number = Some(42);
    config.pr_url = Some("https://github.com/vetcoders/prview/pull/42".into());
    let diff = mock_diff();
    let ctx = mock_ctx();

    let report_json = r#"{"meta":{"generated_at":"2026-03-08T03:10:35+01:00"}}"#;
    let html = build_header(
        &config,
        Some(&diff),
        &ctx,
        Path::new("dashboard-context-header-p0"),
        report_json,
    );

    assert!(html.contains("vetcoders/prview"));
    assert!(html.contains(r#"<span class="wordmark">prview<span class="dot""#));
    assert!(html.contains("feature/test"));
    assert!(html.contains("main"));
    assert!(html.contains("bbb"));
    assert!(html.contains("20260308-031035"));
    assert!(html.contains("2026-03-08 03:10:35"));
    assert!(html.contains("#42"));
}

#[test]
fn test_header_prefers_run_json_context_for_custom_output_dir() {
    let config = mock_config();
    let diff = mock_diff();
    let ctx = mock_ctx();
    let temp = tempfile::tempdir().unwrap();
    let summary_dir = temp.path().join("00_summary");
    fs::create_dir_all(&summary_dir).unwrap();
    fs::write(
        summary_dir.join("RUN.json"),
        serde_json::to_string_pretty(&json!({
            "repo": { "gh_repo": "vetcoders/rmcp-memex" },
            "refs": {
                "target": "agent/context-pass",
                "target_sha": "02aabca1234567890",
                "bases": [{ "name": "develop", "sha": "abc1234567890" }]
            },
            "run_finished_at": "2026-03-05T19:17:49+01:00"
        }))
        .unwrap(),
    )
    .unwrap();

    let html = build_header(&config, Some(&diff), &ctx, temp.path(), "{}");

    assert!(html.contains("vetcoders/rmcp-memex"));
    assert!(html.contains("agent/context-pass"));
    assert!(html.contains("develop"));
    assert!(html.contains("02aabca"));
    assert!(html.contains("20260305-191749"));
    assert!(html.contains("2026-03-05 19:17:49"));
}

// ---- PRV-101: Merge Decision Card ----

#[test]
fn test_merge_decision_card_all_pass() {
    let ctx = mock_ctx(); // recommended_merge=true, quality_pass=true, policy_allow_merge=true
    let html = build_merge_decision_card(&ctx);

    assert!(html.contains("Merge Decision"), "Should have title");
    assert!(html.contains("Policy: ALLOW"), "Policy should be ALLOW");
    assert!(html.contains("Quality: PASS"), "Quality should be PASS");
    assert!(html.contains("Merge: GO"), "Merge should be GO");
    assert!(html.contains("alert-success"), "Should be success class");
    assert!(
        html.contains("All quality gates passed"),
        "Should show success reason"
    );
}

#[test]
fn test_merge_decision_card_review_caveats() {
    let mut ctx = mock_ctx();
    ctx.breaking = vec![BreakingFinding {
        file: "src/lib.rs".into(),
        kind: BreakingKind::RemovedSymbol {
            symbol_type: "function".into(),
        },
        line: "pub fn old_api()".into(),
        risk_level: BreakingRisk::High,
    }];
    ctx.coverage = super::super::CoverageDelta {
        pct: 11,
        total_source: 43,
        covered_count: 5,
        uncovered: vec![],
        covered: vec![],
        non_code_count: 18,
        ghost_tests: vec![],
    };
    ctx.findings = vec![super::super::DashboardFinding {
        level: "warning",
        check_name: "heuristics_loctree".into(),
        check_id: "heuristics_loctree".into(),
        message: "review me".into(),
        in_diff: Some(true),
    }];
    let html = build_merge_decision_card(&ctx);

    assert!(html.contains("Merge: GO WITH REVIEW"));
    assert!(html.contains("alert-warning"));
    assert!(html.contains("Quality gates passed, but 3 review signals need attention"));
    assert!(html.contains(
        r#"data-decision-reason="Quality gates passed, but 3 review signals need attention""#
    ));
    assert!(html.contains(r#"data-i18n="message.reviewSignalsPrefix""#));
    assert!(html.contains(
            r#"data-review-signals="1 removed public symbol · 11% coverage heuristic · 1 inline finding""#
        ));
    assert!(html.contains("11% coverage heuristic"));
    assert!(html.contains("1 inline finding"));
}

#[test]
fn test_merge_decision_card_quality_fail() {
    let mut ctx = mock_ctx();
    ctx.quality_pass = false;
    ctx.recommended_merge = false;
    ctx.allow_merge = true; // policy allows but quality fails
    ctx.quality_failures = vec!["cargo_clippy".into(), "cargo_test".into()];
    let html = build_merge_decision_card(&ctx);

    assert!(html.contains("Quality: FAIL"), "Quality should be FAIL");
    assert!(
        html.contains("Merge: HOLD"),
        "Merge should be HOLD when policy allows but quality fails"
    );
    assert!(html.contains("alert-warning"), "Should be warning class");
    assert!(
        html.contains("2 quality checks failed"),
        "Should show failure count"
    );
}

#[test]
fn test_merge_decision_card_policy_block() {
    let mut ctx = mock_ctx();
    ctx.policy_allow_merge = false;
    ctx.recommended_merge = false;
    ctx.allow_merge = false;
    ctx.blocking_issues = vec!["cargo_clippy (fail)".into()];
    let html = build_merge_decision_card(&ctx);

    assert!(html.contains("Policy: BLOCK"), "Policy should be BLOCK");
    assert!(html.contains("Merge: BLOCK"), "Merge should be BLOCK");
    assert!(html.contains("alert-error"), "Should be error class");
    assert!(
        html.contains("1 blocking issue found"),
        "Should show blocking count"
    );
}

#[test]
fn test_action_center_traffic_light_chips() {
    let ctx = mock_ctx();
    let checks = mock_checks();
    let html = build_action_center(&ctx, &checks, None);

    assert!(
        html.contains("action-row"),
        "Action center should use action-row layout"
    );
    assert!(
        html.contains("action-chip"),
        "Action center should contain action chips"
    );
    // Should have Checks chip (all passed in mock)
    assert!(
        html.contains("Checks"),
        "Action center should contain Checks chip"
    );
    assert!(
        html.contains(r#"data-i18n-template="count.warnings""#),
        "Checks warning card should be localizable"
    );
    // Should have Breaking chip
    assert!(
        html.contains("Breaking"),
        "Action center should contain Breaking chip"
    );
    // Should have Findings chip
    assert!(
        html.contains("Findings"),
        "Action center should contain Findings chip"
    );
}

// ---- PRV-102: Top 3 Blockers ----

#[test]
fn test_blockers_section_no_blockers() {
    let ctx = mock_ctx();
    let checks = mock_checks(); // all passed/warnings, no failures
    let html = build_blockers_section(&ctx, &checks);

    assert!(
        html.is_empty(),
        "Clean runs should not emit a green blockers section"
    );
}

#[test]
fn test_blockers_section_with_failures() {
    let ctx = mock_ctx();
    let checks = vec![
        CheckResult {
            name: "cargo clippy".into(),
            status: CheckStatus::Failed,
            duration: Duration::from_secs(3),
            output: "error: unused variable\nhelp: remove it".into(),
            cached: false,
            provenance: None,
        },
        CheckResult {
            name: "cargo test".into(),
            status: CheckStatus::Error,
            duration: Duration::from_secs(5),
            output: "thread panicked\ntest result: FAILED".into(),
            cached: false,
            provenance: None,
        },
        CheckResult {
            name: "cargo check".into(),
            status: CheckStatus::Passed,
            duration: Duration::from_secs(2),
            output: "ok".into(),
            cached: false,
            provenance: None,
        },
    ];
    let html = build_blockers_section(&ctx, &checks);

    assert!(html.contains("Blockers"), "Should have Blockers title");
    assert!(html.contains("cargo test"), "Should show errored check");
    assert!(html.contains("cargo clippy"), "Should show failed check");
    assert!(
        !html.contains("cargo check"),
        "Should not show passed check"
    );
    // Error should appear before Fail
    let error_pos = html.find("cargo test").unwrap();
    let fail_pos = html.find("cargo clippy").unwrap();
    assert!(
        error_pos < fail_pos,
        "Error checks should appear before Failed checks"
    );
}

#[test]
fn test_blockers_section_max_three() {
    let ctx = mock_ctx();
    let checks = vec![
        CheckResult {
            name: "check_a".into(),
            status: CheckStatus::Failed,
            duration: Duration::from_secs(1),
            output: "fail a".into(),
            cached: false,
            provenance: None,
        },
        CheckResult {
            name: "check_b".into(),
            status: CheckStatus::Failed,
            duration: Duration::from_secs(1),
            output: "fail b".into(),
            cached: false,
            provenance: None,
        },
        CheckResult {
            name: "check_c".into(),
            status: CheckStatus::Failed,
            duration: Duration::from_secs(1),
            output: "fail c".into(),
            cached: false,
            provenance: None,
        },
        CheckResult {
            name: "check_d".into(),
            status: CheckStatus::Failed,
            duration: Duration::from_secs(1),
            output: "fail d".into(),
            cached: false,
            provenance: None,
        },
    ];
    let html = build_blockers_section(&ctx, &checks);

    assert!(html.contains("check_a"), "Should show check_a");
    assert!(html.contains("check_b"), "Should show check_b");
    assert!(html.contains("check_c"), "Should show check_c");
    assert!(!html.contains("check_d"), "Should NOT show check_d (max 3)");
    assert!(html.contains("section-count\">3"), "Count should be 3");
}

#[test]
fn test_blockers_debug_hints() {
    assert_eq!(debug_hint_for_check("cargo_clippy"), "cargo clippy --fix");
    assert_eq!(
        debug_hint_for_check("cargo_test"),
        "cargo test -- --nocapture"
    );
    assert_eq!(debug_hint_for_check("cargo_fmt"), "cargo fmt");
    assert_eq!(debug_hint_for_check("eslint"), "npx eslint --fix .");
    assert_eq!(debug_hint_for_check("ruff_check"), "ruff check --fix .");
    assert_eq!(
        debug_hint_for_check("unknown_check"),
        "Re-run locally with verbose output"
    );
}

// ---- PRV-103: Time Budget ----

#[test]
fn test_time_budget_basic() {
    let checks = mock_checks();
    let html = build_time_budget(&checks);

    assert!(
        html.contains("Time Budget"),
        "Should have Time Budget title"
    );
    assert!(
        html.contains("section-time-budget"),
        "Should have section id"
    );
    assert!(html.contains("cargo check"), "Should show check name");
    assert!(html.contains("cargo clippy"), "Should show check name");
    assert!(
        html.contains(r#"data-i18n="label.total""#),
        "Should localize total label"
    );
    assert!(html.contains("2 checks"), "Should show count");
}

#[test]
fn test_time_budget_slowest_badge() {
    let checks = vec![
        CheckResult {
            name: "fast_check".into(),
            status: CheckStatus::Passed,
            duration: Duration::from_secs(1),
            output: "ok".into(),
            cached: false,
            provenance: None,
        },
        CheckResult {
            name: "slow_check".into(),
            status: CheckStatus::Passed,
            duration: Duration::from_secs(10),
            output: "ok".into(),
            cached: false,
            provenance: None,
        },
    ];
    let html = build_time_budget(&checks);

    assert!(html.contains("tb-slowest"), "Should have slowest bar class");
    assert!(html.contains("slowest"), "Should have slowest badge");
    // Slowest should be on slow_check's row
    let slowest_pos = html.find("tb-slowest").unwrap();
    let slow_check_pos = html.find("slow_check").unwrap();
    assert!(
        slowest_pos > slow_check_pos,
        "Slowest bar should be on slow_check's row"
    );
}

#[test]
fn test_time_budget_cached_label() {
    let checks = vec![
        CheckResult {
            name: "cached_check".into(),
            status: CheckStatus::Passed,
            duration: Duration::from_secs(0),
            output: "ok".into(),
            cached: true,
            provenance: None,
        },
        CheckResult {
            name: "normal_check".into(),
            status: CheckStatus::Passed,
            duration: Duration::from_secs(5),
            output: "ok".into(),
            cached: false,
            provenance: None,
        },
    ];
    let html = build_time_budget(&checks);

    assert!(html.contains("tb-cached"), "Should have cached bar class");
    assert!(html.contains("(cached)"), "Should show cached label");
}

#[test]
fn test_time_budget_empty() {
    let checks: Vec<CheckResult> = vec![];
    let html = build_time_budget(&checks);
    assert!(html.is_empty(), "Empty checks should produce empty string");
}

#[test]
fn test_time_budget_single_check_no_slowest() {
    let checks = vec![CheckResult {
        name: "only_check".into(),
        status: CheckStatus::Passed,
        duration: Duration::from_secs(5),
        output: "ok".into(),
        cached: false,
        provenance: None,
    }];
    let html = build_time_budget(&checks);

    assert!(html.contains("only_check"), "Should show the check");
    assert!(
        !html.contains("slowest"),
        "Single check should not have slowest badge"
    );
    assert!(html.contains("1 check"), "Should show singular count");
}

// ---- Integration: sidebar nav ----

#[test]
fn test_sidebar_has_blockers_when_failures() {
    let config = mock_config();
    let diffs = vec![mock_diff()];
    let checks = vec![CheckResult {
        name: "failing_check".into(),
        status: CheckStatus::Failed,
        duration: Duration::from_secs(2),
        output: "fail".into(),
        cached: false,
        provenance: None,
    }];
    let mut ctx = mock_ctx();
    ctx.quality_pass = false;
    ctx.recommended_merge = false;
    let dir = std::env::temp_dir();

    let html = build_html_test!(&config, &diffs, &checks, None, &ctx, "", &dir, "", None);

    assert!(
        !html.contains(r##"href="#section-blockers""##),
        "Sidebar should stay compact and avoid a dedicated Blockers link"
    );
    assert!(
        html.contains(r##"href="#section-time-budget""##),
        "Sidebar should have Time Budget link"
    );
    assert!(
        html.contains(r##"href="#section-checks""##),
        "Checks should remain the main quality entry point"
    );
}

#[test]
fn test_sidebar_no_blockers_when_all_pass() {
    let config = mock_config();
    let diffs = vec![mock_diff()];
    let checks = mock_checks(); // no failures
    let ctx = mock_ctx();
    let dir = std::env::temp_dir();

    let html = build_html_test!(&config, &diffs, &checks, None, &ctx, "", &dir, "", None);

    assert!(
        !html.contains(r##"href="#section-blockers""##),
        "Sidebar should NOT have Blockers link when no failures"
    );
    assert!(
        html.contains(r##"href="#section-time-budget""##),
        "Sidebar should have Time Budget link"
    );
}

// ---- PRV-104: Delta vs Previous Run ----

#[test]
fn test_delta_section_empty_when_no_previous() {
    let ctx = mock_ctx(); // previous_run = None
    let checks = mock_checks();
    let html = build_delta_section(&ctx, &checks);
    assert!(
        html.is_empty(),
        "Delta section should be empty when no previous run"
    );
}

#[test]
fn test_delta_section_shows_improvement() {
    use super::super::PreviousRunDelta;
    let mut ctx = mock_ctx();
    ctx.previous_run = Some(PreviousRunDelta {
        checks_passed_before: 1,
        checks_failed_before: 2,
        findings_before: 3,
        breaking_before: 0,
        quality_pass_before: false,
    });
    let checks = mock_checks(); // 1 passed, 0 failed
    let html = build_delta_section(&ctx, &checks);

    assert!(html.contains("vs Previous Run"), "Should show delta header");
    assert!(
        html.contains("delta-better"),
        "Should show improvement badge"
    );
    assert!(html.contains("Passed"), "Should show Passed delta");
    assert!(html.contains("Quality"), "Should show Quality delta");
}

#[test]
fn test_delta_section_shows_regression() {
    use super::super::PreviousRunDelta;
    let mut ctx = mock_ctx();
    ctx.quality_pass = false;
    ctx.previous_run = Some(PreviousRunDelta {
        checks_passed_before: 5,
        checks_failed_before: 0,
        findings_before: 0,
        breaking_before: 0,
        quality_pass_before: true,
    });
    let checks = vec![CheckResult {
        name: "cargo check".into(),
        status: CheckStatus::Failed,
        duration: Duration::from_secs(2),
        output: "error".into(),
        cached: false,
        provenance: None,
    }];
    let html = build_delta_section(&ctx, &checks);

    assert!(html.contains("delta-worse"), "Should show regression badge");
}

#[test]
fn test_delta_nav_conditional() {
    let config = mock_config();
    let diffs = vec![mock_diff()];
    let checks = mock_checks();
    let ctx = mock_ctx(); // no previous_run
    let dir = std::env::temp_dir();

    let html = build_html_test!(&config, &diffs, &checks, None, &ctx, "", &dir, "", None);
    assert!(
        !html.contains(r##"href="#section-delta""##),
        "Nav should NOT have Delta link when no previous run"
    );
}

// ---- PRV-105: View Mode Toggle ----

#[test]
fn test_view_mode_toggle_present() {
    let config = mock_config();
    let diffs = vec![mock_diff()];
    let checks = mock_checks();
    let ctx = mock_ctx();
    let dir = std::env::temp_dir();

    let html = build_html_test!(&config, &diffs, &checks, None, &ctx, "", &dir, "", None);
    assert!(
        html.contains("view-mode-review"),
        "Should have review view toggle button"
    );
    assert!(
        html.contains("view-mode-author"),
        "Should have author view toggle button"
    );
    assert!(
        html.contains("Review View"),
        "Toggle should expose review mode label"
    );
    assert!(
        html.contains("Author View"),
        "Toggle should expose author mode label"
    );
}

#[test]
fn test_language_toggle_present() {
    let config = mock_config();
    let diffs = vec![mock_diff()];
    let checks = mock_checks();
    let ctx = mock_ctx();
    let dir = std::env::temp_dir();

    let html = build_html_test!(&config, &diffs, &checks, None, &ctx, "", &dir, "", None);
    assert!(
        html.contains("lang-toggle-en"),
        "Should have English language toggle"
    );
    assert!(
        html.contains("lang-toggle-pl"),
        "Should have Polish language toggle"
    );
    assert!(
        html.contains("data-i18n=\"button.copyPrComment\""),
        "Copy button should participate in i18n"
    );
    assert!(
        html.contains("dashboardLang"),
        "Dashboard should persist selected language"
    );
}

#[test]
fn test_author_mode_css_classes_on_sections() {
    let config = mock_config();
    let diffs = vec![mock_diff()];
    let checks = mock_checks();
    let ctx = mock_ctx();
    let dir = std::env::temp_dir();

    let html = build_html_test!(&config, &diffs, &checks, None, &ctx, "", &dir, "", None);
    assert!(
        html.contains("section-system"),
        "Should have section-system class"
    );
    assert!(
        html.contains("section-noise"),
        "Should have section-noise class"
    );
    assert!(
        html.contains("body.author-mode .section-system { display: none; }"),
        "CSS should hide system sections"
    );
    assert!(
        html.contains("body.author-mode .section-noise"),
        "CSS should hide noise sections"
    );
}

#[test]
fn test_collapsible_sections_keep_single_visible_title_row() {
    let config = mock_config();
    let diffs = vec![mock_diff()];
    let checks = mock_checks();
    let ctx = mock_ctx();
    let dir = std::env::temp_dir();

    let html = build_html_test!(&config, &diffs, &checks, None, &ctx, "", &dir, "", None);

    assert!(
        html.contains(".section-collapsible > .section-header"),
        "Collapsible styles should target only the outer header"
    );
    assert!(
        html.contains(
            ".section-collapsible .section-body > .section > .section-header .section-title"
        ),
        "Nested section titles should be compacted inside collapsibles"
    );
    assert!(
        html.contains("overflow-wrap: anywhere"),
        "Section summaries should wrap instead of pushing layout wide"
    );
}

// ---- PRV-202: Security Lens ----

#[test]
fn test_security_section_no_security_checks() {
    let checks = mock_checks(); // cargo check, cargo clippy — not security
    let ctx = mock_ctx();
    let html = build_security_section(&checks, &ctx);
    assert!(
        html.is_empty(),
        "No security checks should produce empty string"
    );
}

#[test]
fn test_security_section_audit_pass() {
    let checks = vec![CheckResult {
        name: "Cargo audit".into(),
        status: CheckStatus::Passed,
        duration: Duration::from_secs(1),
        output: r#"{"vulnerabilities":{"count":0,"list":[]}}"#.into(),
        cached: false,
        provenance: None,
    }];
    let ctx = mock_ctx();
    let html = build_security_section(&checks, &ctx);

    assert!(html.contains("section-security"), "Should have section id");
    assert!(html.contains("Security"), "Should have Security title");
    assert!(html.contains("PASS"), "Overall should be PASS");
    assert!(html.contains("badge-success"), "Should have success badge");
    assert!(
        html.contains("0 vulnerabilities"),
        "Should show 0 vulnerabilities metric"
    );
    assert!(html.contains("Cargo audit"), "Should show check name");
}

#[test]
fn test_security_section_audit_fail() {
    let checks = vec![CheckResult {
        name: "Cargo audit".into(),
        status: CheckStatus::Failed,
        duration: Duration::from_secs(1),
        output: r#"{"vulnerabilities":{"count":3,"list":[{},{},{}]}}"#.into(),
        cached: false,
        provenance: None,
    }];
    let ctx = mock_ctx();
    let html = build_security_section(&checks, &ctx);

    assert!(html.contains("FAIL"), "Overall should be FAIL");
    assert!(html.contains("badge-error"), "Should have error badge");
    assert!(
        html.contains("3 vulnerabilities"),
        "Should show 3 vulnerabilities"
    );
    assert!(html.contains("sec-fail"), "Card should have sec-fail class");
}

#[test]
fn test_security_section_mixed() {
    let checks = vec![
        CheckResult {
            name: "Cargo audit".into(),
            status: CheckStatus::Passed,
            duration: Duration::from_secs(1),
            output: r#"{"vulnerabilities":{"count":0,"list":[]}}"#.into(),
            cached: false,
            provenance: None,
        },
        CheckResult {
            name: "Cargo geiger".into(),
            status: CheckStatus::Warnings,
            duration: Duration::from_secs(5),
            output: "3/15 unsafe expressions in 2 crates".into(),
            cached: false,
            provenance: None,
        },
    ];
    let ctx = mock_ctx();
    let html = build_security_section(&checks, &ctx);

    assert!(
        html.contains("WARN"),
        "Overall should be WARN (worst of pass+warn)"
    );
    assert!(html.contains("Cargo audit"), "Should show audit");
    assert!(html.contains("Cargo geiger"), "Should show geiger");
    assert!(html.contains("0 vulnerabilities"), "Audit metric");
    assert!(
        html.contains("unsafe"),
        "Geiger metric should mention unsafe"
    );
    assert!(html.contains("sec-pass"), "Audit card should be pass");
    assert!(html.contains("sec-warn"), "Geiger card should be warn");
}

#[test]
fn test_security_section_skipped_excluded_from_fold() {
    let checks = vec![
        CheckResult {
            name: "Cargo audit".into(),
            status: CheckStatus::Passed,
            duration: Duration::from_secs(1),
            output: r#"{"vulnerabilities":{"count":0,"list":[]}}"#.into(),
            cached: false,
            provenance: None,
        },
        CheckResult {
            name: "Cargo geiger".into(),
            status: CheckStatus::Skipped,
            duration: Duration::from_secs(0),
            output: "skipped: not enabled".into(),
            cached: false,
            provenance: None,
        },
    ];
    let ctx = mock_ctx();
    let html = build_security_section(&checks, &ctx);

    // Overall should be PASS (skipped excluded from fold), not SKIP
    assert!(
        html.contains("PASS"),
        "Overall should be PASS when only non-skipped check passed"
    );
    assert!(
        html.contains("1 skipped"),
        "Should show skipped count badge"
    );
    assert!(
        html.contains("badge-muted"),
        "Skipped badge should use muted style"
    );
}

#[test]
fn test_security_section_all_skipped() {
    let checks = vec![
        CheckResult {
            name: "Cargo audit".into(),
            status: CheckStatus::Skipped,
            duration: Duration::from_secs(0),
            output: "skipped".into(),
            cached: false,
            provenance: None,
        },
        CheckResult {
            name: "Cargo geiger".into(),
            status: CheckStatus::Skipped,
            duration: Duration::from_secs(0),
            output: "skipped".into(),
            cached: false,
            provenance: None,
        },
    ];
    let ctx = mock_ctx();
    let html = build_security_section(&checks, &ctx);

    assert!(
        html.contains("SKIP"),
        "Overall should be SKIP when all checks are skipped"
    );
    assert!(html.contains("2 skipped"), "Should show 2 skipped");
}

#[test]
fn test_security_section_localizes_fallback_metric() {
    let checks = vec![CheckResult {
        name: "Security scan".into(),
        status: CheckStatus::Warnings,
        duration: Duration::from_secs(1),
        output: "details unavailable".into(),
        cached: false,
        provenance: None,
    }];
    let ctx = mock_ctx();
    let html = build_security_section(&checks, &ctx);

    assert!(
        html.contains(r#"data-i18n-template="message.seeLogForDetails""#),
        "Fallback helper copy should be localizable"
    );
}

#[test]
fn test_is_security_check() {
    assert!(is_security_check("Cargo audit"));
    assert!(is_security_check("Cargo geiger"));
    assert!(is_security_check("semgrep scan"));
    assert!(is_security_check("snyk test"));
    assert!(is_security_check("Security scan"));
    assert!(!is_security_check("Cargo check"));
    assert!(!is_security_check("Clippy"));
    assert!(!is_security_check("Cargo test"));
}

#[test]
fn test_extract_security_metric_audit_json() {
    let output = r#"{"vulnerabilities":{"count":2,"list":[{},{}]}}"#;
    let metric = extract_security_metric("Cargo audit", output);
    assert_eq!(metric, "2 vulnerabilities");
}

#[test]
fn test_extract_security_metric_audit_zero() {
    let output = r#"{"vulnerabilities":{"count":0,"list":[]}}"#;
    let metric = extract_security_metric("Cargo audit", output);
    assert_eq!(metric, "0 vulnerabilities");
}

#[test]
fn test_extract_security_metric_geiger() {
    let output = "3/15 unsafe expressions in 2 crates";
    let metric = extract_security_metric("Cargo geiger", output);
    assert!(metric.contains("unsafe"), "Should extract unsafe ratio");
}

// ---- PRV-206: Confidence Badges ----

#[test]
fn test_confidence_badge_high() {
    assert_eq!(heuristic_confidence("dead_exports", 3), "high");
    assert_eq!(heuristic_confidence("cycles", 5), "high");
}

#[test]
fn test_confidence_badge_medium_from_count() {
    assert_eq!(heuristic_confidence("dead_exports", 15), "medium");
    assert_eq!(heuristic_confidence("cycles", 20), "medium");
}

#[test]
fn test_confidence_badge_medium_twins() {
    assert_eq!(heuristic_confidence("twins", 3), "medium");
    assert_eq!(heuristic_confidence("dead_parrots", 5), "medium");
    assert_eq!(heuristic_confidence("unused_symbols", 5), "medium");
}

#[test]
fn test_confidence_badge_low() {
    assert_eq!(heuristic_confidence("twins", 15), "low");
    assert_eq!(heuristic_confidence("dead_parrots", 20), "low");
    assert_eq!(heuristic_confidence("unused_symbols", 20), "low");
    assert_eq!(heuristic_confidence("unknown", 1), "low");
}

#[test]
fn narrative_section_renders_markdown_through_mdrender() {
    let md = "## Findings\n\n> [!NOTE]\n> Look here.\n\n```rust\nfn a() {}\n```\n";
    let html = build_narrative_section(md);

    // Rendered through mdrender (scoped wrapper + GFM callout + code highlight).
    assert!(html.contains("<div class=\"mdr\">"), "mdr wrapper missing");
    assert!(
        html.contains("markdown-alert markdown-alert-note"),
        "note callout missing: {html}"
    );
    assert!(
        html.contains("<span style=\"color:#"),
        "syntect highlight spans missing"
    );
    // Raw markdown is still preserved for the copy-as-markdown control.
    assert!(html.contains("narrative-content"));
}

#[test]
fn test_pl_locale_merge_gate_caveat_translations() {
    // The MERGE_GATE runtime caveats are formulaic. They are emitted into the
    // dashboard as EN source strings (in data-review-signals) and localized
    // client-side by the JS locale toggle. This test locks both halves: the EN
    // source lands in the markup, and the PL transforms are wired in js().
    let mut ctx = mock_ctx();
    ctx.review_caveats = vec![
        "Semgrep scan returned warnings".to_string(),
        "cargo-geiger skipped: security disabled".to_string(),
        "heuristics_loctree needs manual review".to_string(),
        "2 inline findings".to_string(),
    ];
    let html = build_merge_decision_card(&ctx);

    // EN source is preserved verbatim in the translation attribute so the
    // client toggle can localize (or, for unknown shapes, leave EN as-is).
    assert!(html.contains(
        r#"data-review-signals="Semgrep scan returned warnings · cargo-geiger skipped: security disabled · heuristics_loctree needs manual review · 2 inline findings""#
    ));

    // PL transforms are wired in the locale toggle (JS runs client-side, so we
    // assert on the emitted script). Known shapes localize the wrapper while
    // keeping tool names original.
    let js = js();
    assert!(
        js.contains("returned warnings$"),
        "returned-warnings shape missing"
    );
    assert!(
        js.contains("': ostrzeżenia'"),
        "returned-warnings PL missing"
    );
    assert!(js.contains("skipped: "), "skipped shape missing");
    assert!(js.contains("' pominięty: '"), "skipped PL wrapper missing");
    assert!(
        js.contains("'lint disabled': 'lint wyłączony'"),
        "lint reason map missing"
    );
    assert!(
        js.contains("'tests disabled': 'testy wyłączone'"),
        "tests reason map missing"
    );
    assert!(
        js.contains("'security disabled': 'security wyłączone'"),
        "security reason map missing"
    );
    assert!(
        js.contains("needs manual review$"),
        "manual-review shape missing"
    );
    assert!(
        js.contains("' wymaga ręcznego przeglądu'"),
        "manual-review PL missing"
    );
    assert!(
        js.contains("blocking issue(?:s)? found: "),
        "blocking-with-detail shape missing"
    );
    assert!(
        js.contains("'znaleziono '"),
        "blocking-with-detail PL missing"
    );

    // Fallback discipline: unrecognized shapes return the original EN string
    // (never a guessed translation) in both caveat and reason transforms.
    assert!(
        js.contains("return trimmed;"),
        "review-signal EN fallback missing"
    );
    assert!(
        js.contains("return text;"),
        "decision-reason EN fallback missing"
    );
}
