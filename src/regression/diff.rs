//! Diff regression analysis
//!
//! Computes file churn metrics, hotspots, and status distribution.
//! Includes code-only metrics that exclude test/non-code/config files
//! for more accurate regression scoring.

use super::RegressionContext;
use super::tests::{is_code_file, is_config_like, is_test_file};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Maximum number of hotspot entries to keep.
const MAX_HOTSPOTS: usize = 10;

/// Thresholds for diff regression detection (applied to code-only metrics).
const MAX_FILE_CHURN_THRESHOLD: usize = 500;
const FILES_CHANGED_THRESHOLD: usize = 50;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DiffRegression {
    // Total metrics (all files)
    pub files_changed: usize,
    pub insertions: usize,
    pub deletions: usize,
    pub max_file_churn: usize,
    pub top_hotspots: Vec<HotspotEntry>,
    pub changed_files_by_status: BTreeMap<String, usize>,
    pub diff_regression_detected: bool,

    // Code-only metrics (excludes tests, non-code, config-like)
    #[serde(default)]
    pub code_files_changed: usize,
    #[serde(default)]
    pub code_insertions: usize,
    #[serde(default)]
    pub code_deletions: usize,
    #[serde(default)]
    pub max_code_file_churn: usize,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub top_code_hotspots: Vec<HotspotEntry>,
    #[serde(default)]
    pub test_files_changed: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HotspotEntry {
    pub file: String,
    pub churn: usize,
    pub additions: usize,
    pub deletions: usize,
    pub status: char,
}

/// Returns `true` if the file is production code (not test, not non-code, not config-like).
fn is_production_code(path: &str) -> bool {
    is_code_file(path) && !is_test_file(path) && !is_config_like(path)
}

pub fn analyze(ctx: &RegressionContext) -> DiffRegression {
    let mut status_counts: BTreeMap<String, usize> = BTreeMap::new();
    let mut max_churn: usize = 0;
    let mut max_code_churn: usize = 0;
    let mut code_files = 0usize;
    let mut code_adds = 0usize;
    let mut code_dels = 0usize;
    let mut test_files = 0usize;

    let mut all_entries: Vec<HotspotEntry> = Vec::new();
    let mut code_entries: Vec<HotspotEntry> = Vec::new();

    for (path, status, adds, dels) in &ctx.file_stats {
        let churn = adds + dels;
        let status_key = match status {
            'A' => "A",
            'M' => "M",
            'D' => "D",
            'R' => "R",
            'C' => "C",
            _ => "?",
        };
        *status_counts.entry(status_key.to_string()).or_insert(0) += 1;

        if churn > max_churn {
            max_churn = churn;
        }

        let entry = HotspotEntry {
            file: path.clone(),
            churn,
            additions: *adds,
            deletions: *dels,
            status: *status,
        };

        // Classify for code-only metrics
        if is_production_code(path) {
            code_files += 1;
            code_adds += adds;
            code_dels += dels;
            if churn > max_code_churn {
                max_code_churn = churn;
            }
            code_entries.push(entry.clone());
        } else if is_test_file(path) {
            test_files += 1;
        }

        all_entries.push(entry);
    }

    all_entries.sort_by_key(|entry| std::cmp::Reverse(entry.churn));
    all_entries.truncate(MAX_HOTSPOTS);

    code_entries.sort_by_key(|entry| std::cmp::Reverse(entry.churn));
    code_entries.truncate(MAX_HOTSPOTS);

    // Detection uses code-only metrics
    let detected =
        max_code_churn > MAX_FILE_CHURN_THRESHOLD || code_files > FILES_CHANGED_THRESHOLD;

    DiffRegression {
        files_changed: ctx.files_changed,
        insertions: ctx.insertions,
        deletions: ctx.deletions,
        max_file_churn: max_churn,
        top_hotspots: all_entries,
        changed_files_by_status: status_counts,
        diff_regression_detected: detected,
        code_files_changed: code_files,
        code_insertions: code_adds,
        code_deletions: code_dels,
        max_code_file_churn: max_code_churn,
        top_code_hotspots: code_entries,
        test_files_changed: test_files,
    }
}

#[cfg(test)]
mod diff_tests {
    use super::*;

    #[test]
    fn test_code_only_metrics_exclude_tests() {
        let ctx = RegressionContext {
            files_changed: 5,
            insertions: 500,
            deletions: 200,
            file_stats: vec![
                ("src/main.rs".into(), 'M', 100, 50),
                ("src/lib.rs".into(), 'M', 50, 30),
                ("tests/integration.rs".into(), 'A', 200, 0),
                ("e2e/login.spec.ts".into(), 'A', 100, 70),
                ("README.md".into(), 'M', 50, 50),
            ],
            ..Default::default()
        };

        let result = analyze(&ctx);

        // Total metrics include everything
        assert_eq!(result.files_changed, 5);
        assert_eq!(result.max_file_churn, 200); // tests/integration.rs

        // Code-only: only main.rs and lib.rs
        assert_eq!(result.code_files_changed, 2);
        assert_eq!(result.code_insertions, 150);
        assert_eq!(result.code_deletions, 80);
        assert_eq!(result.max_code_file_churn, 150); // main.rs: 100+50

        // Test count
        assert_eq!(result.test_files_changed, 2);

        // Code-only hotspots
        assert_eq!(result.top_code_hotspots.len(), 2);
        assert_eq!(result.top_code_hotspots[0].file, "src/main.rs");
    }

    #[test]
    fn test_detection_uses_code_only() {
        // Total files: 60, but code files: only 5
        // Total max churn: 600 (test file), code max churn: 100
        let mut file_stats: Vec<(String, char, usize, usize)> = (0..55)
            .map(|i| (format!("tests/test_{}.rs", i), 'A', 10, 0))
            .collect();
        file_stats.push(("tests/big_test.rs".into(), 'A', 500, 100));
        file_stats.extend((0..5).map(|i| (format!("src/mod_{}.rs", i), 'M', 20_usize, 10_usize)));

        let ctx = RegressionContext {
            files_changed: 61,
            file_stats,
            ..Default::default()
        };

        let result = analyze(&ctx);

        // Total: 61 files, max churn 600 → would trigger old detection
        assert_eq!(result.files_changed, 61);
        assert_eq!(result.max_file_churn, 600);

        // Code-only: 5 files, max churn 30 → no detection
        assert_eq!(result.code_files_changed, 5);
        assert_eq!(result.max_code_file_churn, 30);
        assert!(!result.diff_regression_detected);
    }

    #[test]
    fn test_code_detection_triggers_on_code_churn() {
        let ctx = RegressionContext {
            files_changed: 3,
            file_stats: vec![
                ("src/huge.rs".into(), 'M', 400, 200), // 600 churn > 500 threshold
                ("tests/test.rs".into(), 'A', 100, 0),
                ("README.md".into(), 'M', 10, 5),
            ],
            ..Default::default()
        };

        let result = analyze(&ctx);
        assert!(result.diff_regression_detected); // code max churn 600 > 500
        assert_eq!(result.code_files_changed, 1);
        assert_eq!(result.max_code_file_churn, 600);
    }

    #[test]
    fn test_config_like_excluded_from_code() {
        let ctx = RegressionContext {
            files_changed: 4,
            file_stats: vec![
                ("src/main.ts".into(), 'M', 50, 20),
                ("playwright.config.ts".into(), 'M', 100, 50),
                ("src/index.ts".into(), 'M', 5, 0), // barrel → config-like
                ("src/types.ts".into(), 'M', 30, 10), // pure types → config-like
            ],
            ..Default::default()
        };

        let result = analyze(&ctx);
        assert_eq!(result.code_files_changed, 1); // only main.ts
        assert_eq!(result.code_insertions, 50);
        assert_eq!(result.code_deletions, 20);
    }

    #[test]
    fn test_devtools_excluded_from_code() {
        let ctx = RegressionContext {
            files_changed: 3,
            file_stats: vec![
                ("src/main.rs".into(), 'M', 50, 20),
                ("src/devtools/browserMocks.ts".into(), 'M', 200, 50),
                ("src/devtools/panel.tsx".into(), 'A', 100, 0),
            ],
            ..Default::default()
        };

        let result = analyze(&ctx);
        assert_eq!(result.code_files_changed, 1); // only main.rs
        assert_eq!(result.max_code_file_churn, 70);
    }
}
