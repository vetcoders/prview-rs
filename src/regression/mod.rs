//! Regression detection module (v2-v4)
//!
//! Computes regression signals across multiple dimensions:
//! - Diff: file churn, hotspots, status distribution
//! - Tests: coverage ratio, untested critical files
//! - Dependencies: cycles/dead_exports/unused_symbols deltas
//! - Performance: cheap pattern-based heuristics on patches
//! - Score: unified 0-100 score with explainability

pub mod deps;
pub mod diff;
pub mod perf;
pub mod score;
pub mod tests;

use serde::{Deserialize, Serialize};

/// Input context for regression computation.
///
/// Assembled from the pipeline after diffs, checks, heuristics, and coverage
/// have been computed.
#[derive(Debug, Clone, Default)]
pub struct RegressionContext {
    // Diff stats
    pub files_changed: usize,
    pub insertions: usize,
    pub deletions: usize,
    /// Per-file churn: (path, status_char, additions, deletions)
    pub file_stats: Vec<(String, char, usize, usize)>,

    // Coverage stats
    pub coverage_ratio: Option<f64>,
    pub base_coverage_ratio: Option<f64>,
    pub untested_critical_files: Vec<String>,
    pub base_untested_critical_count: Option<usize>,

    // Heuristics stats (current run)
    pub cycles: usize,
    pub dead_exports: usize,
    pub unused_symbols: usize,

    // Heuristics stats (base run, if available)
    pub base_cycles: Option<usize>,
    pub base_dead_exports: Option<usize>,
    pub base_unused_symbols: Option<usize>,

    // Heuristics detail lists (capped)
    pub top_cycles: Vec<String>,
    pub top_unused: Vec<String>,

    // Patch text for perf analysis
    pub patch_text: Option<String>,
}

/// Full regression report with sub-reports for each dimension.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RegressionReport {
    pub diff: diff::DiffRegression,
    pub tests: tests::TestRegression,
    pub deps: deps::DepsRegression,
    pub perf: perf::PerfRegression,
    pub score: score::RegressionScore,
}

/// Compute full regression report from context.
pub fn compute_regression(ctx: &RegressionContext) -> RegressionReport {
    let diff_reg = diff::analyze(ctx);
    let test_reg = tests::analyze(ctx);
    let deps_reg = deps::analyze(ctx);
    let perf_reg = perf::analyze(ctx);
    let score_reg = score::compute(&diff_reg, &test_reg, &deps_reg, &perf_reg);

    RegressionReport {
        diff: diff_reg,
        tests: test_reg,
        deps: deps_reg,
        perf: perf_reg,
        score: score_reg,
    }
}

#[cfg(test)]
mod regression_tests {
    use super::*;
    use score::Severity;

    #[test]
    fn test_compute_regression_diff_only() {
        let ctx = RegressionContext {
            files_changed: 5,
            insertions: 100,
            deletions: 50,
            file_stats: vec![
                ("src/main.rs".into(), 'M', 80, 20),
                ("src/lib.rs".into(), 'M', 10, 10),
                ("src/new.rs".into(), 'A', 10, 0),
                ("README.md".into(), 'M', 0, 10),
                ("old.rs".into(), 'D', 0, 10),
            ],
            ..Default::default()
        };

        let report = compute_regression(&ctx);

        assert_eq!(report.diff.files_changed, 5);
        assert_eq!(report.diff.insertions, 100);
        assert_eq!(report.diff.deletions, 50);
        assert_eq!(report.diff.max_file_churn, 100); // src/main.rs: 80+20
        assert!(!report.diff.diff_regression_detected);
        assert_eq!(report.diff.top_hotspots.len(), 5);
        assert_eq!(report.diff.top_hotspots[0].file, "src/main.rs");
        assert_eq!(report.diff.changed_files_by_status.get("M"), Some(&3));
        assert_eq!(report.diff.changed_files_by_status.get("A"), Some(&1));
        assert_eq!(report.diff.changed_files_by_status.get("D"), Some(&1));
    }

    #[test]
    fn test_compute_regression_high_churn() {
        let ctx = RegressionContext {
            files_changed: 60,
            insertions: 3000,
            deletions: 1000,
            file_stats: vec![("big.rs".into(), 'M', 600, 200)],
            ..Default::default()
        };

        let report = compute_regression(&ctx);
        assert!(report.diff.diff_regression_detected);
        assert_eq!(report.diff.max_file_churn, 800);
    }

    #[test]
    fn test_score_severity_ok() {
        let ctx = RegressionContext {
            files_changed: 3,
            insertions: 10,
            deletions: 5,
            file_stats: vec![("a.rs".into(), 'M', 10, 5)],
            ..Default::default()
        };

        let report = compute_regression(&ctx);
        assert_eq!(report.score.severity, Severity::OK);
        assert_eq!(report.score.score, 0);
    }

    #[test]
    fn test_score_severity_med() {
        let ctx = RegressionContext {
            files_changed: 55,
            insertions: 2600,
            deletions: 400,
            file_stats: vec![("big.rs".into(), 'M', 600, 200)],
            untested_critical_files: vec!["a.rs".into(), "b.rs".into(), "c.rs".into()],
            cycles: 5,
            base_cycles: Some(2),
            ..Default::default()
        };

        let report = compute_regression(&ctx);
        assert!(report.score.score > 10);
        assert!(matches!(
            report.score.severity,
            Severity::LOW | Severity::MED | Severity::HIGH
        ));
        assert!(!report.score.score_reasons.is_empty());
    }

    #[test]
    fn test_score_severity_critical() {
        let ctx = RegressionContext {
            files_changed: 200,
            insertions: 10000,
            deletions: 5000,
            file_stats: vec![("huge.rs".into(), 'M', 5000, 3000)],
            coverage_ratio: Some(0.2),
            base_coverage_ratio: Some(0.8),
            untested_critical_files: (0..10)
                .map(|i| format!("file_{}.rs", i))
                .collect(),
            cycles: 20,
            base_cycles: Some(5),
            dead_exports: 30,
            base_dead_exports: Some(10),
            unused_symbols: 40,
            base_unused_symbols: Some(10),
            patch_text: Some(
                "diff --git a/big.rs b/big.rs\n+++ b/big.rs\n@@ -1,3 +1,5 @@\n+for item in items {\n+    db.execute(query);\n+}\n"
                    .to_string(),
            ),
            ..Default::default()
        };

        let report = compute_regression(&ctx);
        assert!(report.score.score >= 76);
        assert_eq!(report.score.severity, Severity::CRITICAL);
    }

    #[test]
    fn test_coverage_delta_detection() {
        let ctx = RegressionContext {
            coverage_ratio: Some(0.5),
            base_coverage_ratio: Some(0.8),
            ..Default::default()
        };

        let report = compute_regression(&ctx);
        assert!(report.tests.coverage_regression_detected);
        assert!(report.tests.coverage_delta.unwrap() < -0.01);
    }

    #[test]
    fn test_deps_regression_detection() {
        let ctx = RegressionContext {
            cycles: 5,
            base_cycles: Some(2),
            dead_exports: 10,
            base_dead_exports: Some(10),
            unused_symbols: 8,
            base_unused_symbols: Some(3),
            ..Default::default()
        };

        let report = compute_regression(&ctx);
        assert!(report.deps.dependency_regression_detected);
        assert_eq!(report.deps.cycles_delta, 3);
        assert_eq!(report.deps.dead_exports_delta, 0);
        assert_eq!(report.deps.unused_symbols_delta, 5);
    }

    #[test]
    fn test_perf_query_in_loop() {
        let patch = r#"diff --git a/src/handler.rs b/src/handler.rs
+++ b/src/handler.rs
@@ -10,3 +10,6 @@
+for user in users {
+    let result = db.query("SELECT * FROM orders WHERE user_id = ?");
+}
"#;

        let ctx = RegressionContext {
            patch_text: Some(patch.to_string()),
            ..Default::default()
        };

        let report = compute_regression(&ctx);
        assert!(report.perf.perf_regression_suspected);
        assert_eq!(report.perf.query_in_loop_count, 1);
    }

    #[test]
    fn test_perf_clone_collect_in_loop() {
        let patch = r#"diff --git a/src/process.rs b/src/process.rs
+++ b/src/process.rs
@@ -5,3 +5,5 @@
+for item in items.iter() {
+    let cloned = item.clone();
+    let collected: Vec<_> = data.iter().collect();
+}
"#;

        let ctx = RegressionContext {
            patch_text: Some(patch.to_string()),
            ..Default::default()
        };

        let report = compute_regression(&ctx);
        assert!(report.perf.perf_regression_suspected);
        assert!(report.perf.clone_collect_in_loop_count > 0);
    }

    #[test]
    fn test_empty_context() {
        let ctx = RegressionContext::default();
        let report = compute_regression(&ctx);

        assert_eq!(report.score.severity, Severity::OK);
        assert_eq!(report.score.score, 0);
        assert!(!report.diff.diff_regression_detected);
        assert!(!report.tests.coverage_regression_detected);
        assert!(!report.deps.dependency_regression_detected);
        assert!(!report.perf.perf_regression_suspected);
    }

    #[test]
    fn test_regression_report_round_trip() {
        let ctx = RegressionContext {
            files_changed: 3,
            insertions: 50,
            deletions: 20,
            file_stats: vec![("a.rs".into(), 'M', 50, 20)],
            ..Default::default()
        };

        let report = compute_regression(&ctx);
        let json = serde_json::to_string_pretty(&report).unwrap();
        assert!(json.contains("\"score\""));
        assert!(json.contains("\"severity\""));

        // Round-trip: deserialize back and verify
        let deserialized: RegressionReport = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.score.score, report.score.score);
        assert_eq!(deserialized.score.severity, report.score.severity);
        assert_eq!(deserialized.diff.files_changed, report.diff.files_changed);
        assert_eq!(
            deserialized.perf.perf_regression_suspected,
            report.perf.perf_regression_suspected
        );
    }

    #[test]
    fn test_score_cap_at_100() {
        // All dimensions maxed far beyond caps
        let ctx = RegressionContext {
            files_changed: 500,
            insertions: 50000,
            deletions: 20000,
            file_stats: vec![("huge.rs".into(), 'M', 50000, 20000)],
            coverage_ratio: Some(0.0),
            base_coverage_ratio: Some(1.0),
            untested_critical_files: (0..50)
                .map(|i| format!("f{}.rs", i))
                .collect(),
            cycles: 100,
            base_cycles: Some(0),
            dead_exports: 100,
            base_dead_exports: Some(0),
            unused_symbols: 100,
            base_unused_symbols: Some(0),
            patch_text: Some(
                "diff --git a/x.rs b/x.rs\n+++ b/x.rs\n@@ -1,1 +1,3 @@\n+for x in items.iter() {\n+    db.execute(q).clone();\n+    db.query(q2).collect();\n+}\n"
                    .to_string(),
            ),
            ..Default::default()
        };

        let report = compute_regression(&ctx);
        // Score is capped at 100; with all dimensions maxed we get ~87
        // (each dimension has individual caps that sum to ~95)
        assert!(
            report.score.score > 75,
            "Expected high score, got {}",
            report.score.score
        );
        assert!(report.score.score <= 100, "Score must not exceed 100");
        assert_eq!(report.score.severity, Severity::CRITICAL);
    }

    #[test]
    fn test_deps_no_base_returns_zero_deltas() {
        let ctx = RegressionContext {
            cycles: 10,
            dead_exports: 5,
            unused_symbols: 3,
            // All base_* fields are None (default)
            ..Default::default()
        };

        let report = compute_regression(&ctx);
        assert_eq!(report.deps.cycles_delta, 0);
        assert_eq!(report.deps.dead_exports_delta, 0);
        assert_eq!(report.deps.unused_symbols_delta, 0);
        assert!(!report.deps.dependency_regression_detected);
        assert_eq!(report.deps.current_cycles, 10);
    }
}
