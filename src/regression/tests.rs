//! Test/coverage regression analysis (v4)
//!
//! Detects coverage ratio drops and untested critical files.
//! Classifies files into code vs non-code to avoid false positives
//! from config/doc/script changes inflating the untested count.

use super::RegressionContext;
use serde::{Deserialize, Serialize};
use std::path::Path;

/// Max non-code files to include in the report (avoid bloat).
const NON_CODE_CAP: usize = 20;

/// Source code extensions we consider "must be tested".
const CODE_EXTENSIONS: &[&str] = &[
    "rs", "ts", "tsx", "js", "jsx", "mjs", "cjs", "cts", "mts", "py", "swift", "go", "java", "kt",
    "c", "cpp", "h", "hpp",
];

/// Directory markers that indicate a test directory.
/// Matched boundary-aware: must appear at path start or after `/`.
const TEST_DIR_MARKERS: &[&str] = &[
    "tests/",
    "test/",
    "__tests__/",
    "e2e/",
    "spec/",
    "__mocks__/",
    "__fixtures__/",
];

/// Basename suffix markers (before the extension) that indicate a test file.
const TEST_SUFFIX_MARKERS: &[&str] = &["_test", ".test", "_spec", ".spec"];

// ---------------------------------------------------------------------------
// Classification helpers
// ---------------------------------------------------------------------------

/// Returns `true` if `path` looks like a source-code file based on extension.
pub fn is_code_file(path: &str) -> bool {
    Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|ext| CODE_EXTENSIONS.contains(&ext))
}

/// Returns `true` if `path` is a config-like file that has a code extension
/// but should not be treated as "must be tested" source code.
///
/// Matches:
/// - `*.config.{ts,js,mjs,cjs,...}` — build/tool config files
/// - `*.setup.{ts,js}` — test setup files
/// - `*.d.ts` — TypeScript declaration files
/// - Basename exactly `index.{ts,js,tsx,jsx}` — barrel/re-export files
/// - Basename exactly `types.ts` or `types.d.ts` — pure type files
/// - Files in `devtools/`, `storybook/`, `.storybook/`, `stories/` directories
/// - Files with `mock` or `Mock` in basename (e.g. `browserMocks.ts`)
pub fn is_config_like(path: &str) -> bool {
    let p = Path::new(path);
    let basename = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
    let basename_lower = basename.to_lowercase();

    // TypeScript declaration files: *.d.ts
    if basename_lower.ends_with(".d.ts") {
        return true;
    }

    // *.config.EXT or *.setup.EXT (where EXT is a code extension)
    // Strip the final extension first, then check if the stem ends with .config or .setup
    if let Some(ext) = p.extension().and_then(|e| e.to_str())
        && CODE_EXTENSIONS.contains(&ext)
    {
        let stem = &basename_lower[..basename_lower.len() - ext.len() - 1];
        if stem.ends_with(".config") || stem.ends_with(".setup") {
            return true;
        }
    }

    // Barrel files: basename is exactly index.{ts,js,tsx,jsx}
    const BARREL_NAMES: &[&str] = &["index.ts", "index.js", "index.tsx", "index.jsx"];
    if BARREL_NAMES.contains(&basename_lower.as_str()) {
        return true;
    }

    // Pure type files: basename is exactly types.ts
    if basename_lower == "types.ts" {
        return true;
    }

    // Devtools / storybook directories (development tooling, not production code)
    let lower_path = path.to_lowercase();
    const DEV_DIRS: &[&str] = &["devtools/", "storybook/", ".storybook/", "stories/"];
    for dir in DEV_DIRS {
        if lower_path.starts_with(dir) || lower_path.contains(&format!("/{}", dir)) {
            return true;
        }
    }

    // Mock files: basename contains "mock" (case-insensitive) with code extension
    if is_code_file(path) && basename_lower.contains("mock") {
        return true;
    }

    false
}

/// Returns `true` if `path` looks like a test file.
///
/// Uses boundary-aware matching to avoid false positives:
/// - Directory markers must be at path start or after `/` (no `contest/` match)
/// - Suffix markers checked against the file stem (no `latest_version.ts` match)
/// - `test_` prefix checked only on the basename (no `src/contest_handler.rs` match)
pub fn is_test_file(path: &str) -> bool {
    let lower = path.to_lowercase();

    // 1. Directory markers (boundary-aware)
    for marker in TEST_DIR_MARKERS {
        if lower.starts_with(marker) || lower.contains(&format!("/{}", marker)) {
            return true;
        }
    }

    // 2. Extract basename and stem for precise matching
    let basename = lower.rsplit('/').next().unwrap_or(&lower);
    let stem = match basename.rfind('.') {
        Some(pos) => &basename[..pos],
        None => basename,
    };

    // 3. Suffix markers: stem must END with `_test`, `.test`, `_spec`, `.spec`
    for marker in TEST_SUFFIX_MARKERS {
        if stem.ends_with(marker) {
            return true;
        }
    }

    // 4. Prefix marker: basename starts with `test_`
    if basename.starts_with("test_") {
        return true;
    }

    // 5. Exact stem matches (e.g. `tests.rs`, `test.py`)
    if stem == "tests" || stem == "test" {
        return true;
    }

    false
}

// ---------------------------------------------------------------------------
// Rust inline test detection
// ---------------------------------------------------------------------------

/// Check if a `.rs` file has inline test evidence (`#[cfg(test)]` or `#[test]`)
/// visible in the patch hunks (added, removed, or context lines).
fn has_inline_test_in_patch(file: &str, patch: &str) -> bool {
    if !file.ends_with(".rs") {
        return false;
    }

    let mut in_file = false;
    for line in patch.lines() {
        if line.starts_with("+++ b/") {
            in_file = &line[6..] == file;
            continue;
        }
        if in_file && (line.contains("#[cfg(test)]") || line.contains("#[test]")) {
            return true;
        }
    }
    false
}

// ---------------------------------------------------------------------------
// Struct
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TestRegression {
    pub coverage_ratio: Option<f64>,
    pub base_coverage_ratio: Option<f64>,
    pub coverage_delta: Option<f64>,

    /// Code-only untested files (backward-compat: was all files before v4).
    pub untested_critical_files: Vec<String>,
    /// Count of code-only untested files.
    pub untested_critical_count: usize,

    /// Code files without matching test change.
    #[serde(default)]
    pub untested_code_files: Vec<String>,
    #[serde(default)]
    pub untested_code_count: usize,

    /// Non-code files (config, docs, scripts, etc.) — capped.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub untested_non_code_files: Vec<String>,
    #[serde(default)]
    pub untested_non_code_count: usize,

    /// Rust files excluded from untested count because they have inline
    /// `#[cfg(test)]` / `#[test]` evidence in the patch.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rust_inline_test_files: Vec<String>,

    /// Count of `.rs` files still in the untested list that *may* have inline
    /// tests not visible in the patch (caveat for reviewers).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub rust_inline_test_caveat_count: usize,

    #[serde(default)]
    pub coverage_improvement_detected: bool,
    pub coverage_regression_detected: bool,
}

fn is_zero(v: &usize) -> bool {
    *v == 0
}

// ---------------------------------------------------------------------------
// Analysis
// ---------------------------------------------------------------------------

pub fn analyze(ctx: &RegressionContext) -> TestRegression {
    let coverage_delta = match (ctx.coverage_ratio, ctx.base_coverage_ratio) {
        (Some(current), Some(base)) => Some(current - base),
        _ => None,
    };

    // Classify incoming untested files
    let mut untested_code: Vec<String> = Vec::new();
    let mut untested_non_code: Vec<String> = Vec::new();

    for file in &ctx.untested_critical_files {
        if is_test_file(file) {
            // Test files are neither "code that needs testing" nor "non-code";
            // they are tests themselves — skip entirely.
            continue;
        }
        if is_code_file(file) && !is_config_like(file) {
            untested_code.push(file.clone());
        } else {
            untested_non_code.push(file.clone());
        }
    }

    let mut rust_inline_test_files = Vec::new();
    if let Some(patch_text) = ctx.patch_text.as_deref() {
        let mut remaining = Vec::with_capacity(untested_code.len());
        for file in untested_code {
            if has_inline_test_in_patch(&file, patch_text) {
                rust_inline_test_files.push(file);
            } else {
                remaining.push(file);
            }
        }
        untested_code = remaining;
    }

    let rust_inline_test_caveat_count = untested_code
        .iter()
        .filter(|file| file.ends_with(".rs"))
        .count();
    let untested_code_count = untested_code.len();
    let untested_non_code_count = untested_non_code.len();

    // Cap non-code list to avoid report bloat
    untested_non_code.truncate(NON_CODE_CAP);

    // Regression detection — based on CODE files only
    let ratio_dropped = coverage_delta.is_some_and(|d| d < -0.01);
    let untested_increased = ctx
        .base_untested_critical_count
        .is_some_and(|base| untested_code_count > base);

    // Adaptive threshold: compute code-specific test coverage ratio from file_stats.
    // total_code_in_pr = code files in the PR (excluding tests and config-like).
    // coverage_ratio = (total_code - untested_code) / total_code.
    let total_code_from_stats = ctx
        .file_stats
        .iter()
        .filter(|(path, _, _, _)| {
            is_code_file(path) && !is_test_file(path) && !is_config_like(path)
        })
        .count();

    // When file_stats is unavailable, fall back to untested_code_count as the
    // total (worst-case: 0% coverage). This keeps the detection conservative.
    let total_code_in_pr = if total_code_from_stats > 0 {
        total_code_from_stats
    } else {
        untested_code_count
    };

    let code_coverage_ratio = if total_code_in_pr > 0 {
        (total_code_in_pr.saturating_sub(untested_code_count)) as f64 / total_code_in_pr as f64
    } else {
        1.0 // no code files at all → fully covered by definition
    };

    let coverage_delta_drop = coverage_delta.is_some_and(|d| d < -0.05);
    let low_coverage = untested_code_count > 0 && code_coverage_ratio < 0.4;
    let hard_cap = untested_code_count > 10 && code_coverage_ratio < 0.5;

    // Suppress soft signals (low_coverage, hard_cap, untested_increased) when
    // coverage actually improved — those indicate current state, not regression.
    let coverage_improved = coverage_delta.is_some_and(|d| d > 0.01);

    let detected = ratio_dropped
        || coverage_delta_drop
        || (untested_increased && !coverage_improved)
        || (!coverage_improved && (low_coverage || hard_cap));

    TestRegression {
        coverage_ratio: ctx.coverage_ratio,
        base_coverage_ratio: ctx.base_coverage_ratio,
        coverage_delta,
        // Backward compat: these now only contain code files
        untested_critical_files: untested_code.clone(),
        untested_critical_count: untested_code_count,
        // New granular fields
        untested_code_files: untested_code,
        untested_code_count,
        untested_non_code_files: untested_non_code,
        untested_non_code_count,
        rust_inline_test_files,
        rust_inline_test_caveat_count,
        coverage_improvement_detected: coverage_improved,
        coverage_regression_detected: detected,
    }
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests_unit {
    use super::*;

    // -- Classification tests --

    #[test]
    fn test_is_code_file_rust() {
        assert!(is_code_file("src/main.rs"));
        assert!(is_code_file("lib/foo.rs"));
    }

    #[test]
    fn test_is_code_file_js_ts() {
        assert!(is_code_file("src/app.ts"));
        assert!(is_code_file("src/index.tsx"));
        assert!(is_code_file("src/util.js"));
        assert!(is_code_file("src/worker.mjs"));
        assert!(is_code_file("src/App.jsx"));
    }

    #[test]
    fn test_is_code_file_other_langs() {
        assert!(is_code_file("main.py"));
        assert!(is_code_file("main.go"));
        assert!(is_code_file("Main.java"));
        assert!(is_code_file("Main.kt"));
        assert!(is_code_file("lib.swift"));
        assert!(is_code_file("util.c"));
        assert!(is_code_file("util.cpp"));
        assert!(is_code_file("util.h"));
        assert!(is_code_file("util.hpp"));
    }

    #[test]
    fn test_is_not_code_file() {
        assert!(!is_code_file(".env"));
        assert!(!is_code_file(".gitignore"));
        assert!(!is_code_file("Makefile"));
        assert!(!is_code_file("README.md"));
        assert!(!is_code_file("Cargo.toml"));
        assert!(!is_code_file("package.json"));
        assert!(!is_code_file("docs/guide.md"));
        assert!(!is_code_file("scripts/deploy.sh"));
        assert!(!is_code_file(".env.production"));
        assert!(!is_code_file("Dockerfile"));
        assert!(!is_code_file("docker-compose.yml"));
    }

    #[test]
    fn test_is_test_file() {
        assert!(is_test_file("src/tests/foo.rs"));
        assert!(is_test_file("tests/integration.rs"));
        assert!(is_test_file("__tests__/App.test.tsx"));
        assert!(is_test_file("e2e/login.spec.ts"));
        assert!(is_test_file("src/foo_test.go"));
        assert!(is_test_file("src/foo.test.js"));
        assert!(is_test_file("src/foo_spec.rb"));
        assert!(is_test_file("src/foo.spec.ts"));
        assert!(is_test_file("src/regression/tests.rs"));
        assert!(is_test_file("test_utils.py"));
        // Mock, fixture, and spec dirs
        assert!(is_test_file("__mocks__/api.ts"));
        assert!(is_test_file("src/__mocks__/service.ts"));
        assert!(is_test_file("__fixtures__/data.json"));
        assert!(is_test_file("spec/api_handler.ts"));
        assert!(is_test_file("src/spec/models/user.ts"));
    }

    #[test]
    fn test_is_not_test_file() {
        assert!(!is_test_file("src/main.rs"));
        assert!(!is_test_file("src/lib.rs"));
        assert!(!is_test_file("src/app.ts"));
    }

    // -- Analyze: classification --

    #[test]
    fn test_analyze_separates_code_and_non_code() {
        let ctx = RegressionContext {
            untested_critical_files: vec![
                "src/main.rs".into(),
                "src/lib.rs".into(),
                ".env".into(),
                "README.md".into(),
                "Makefile".into(),
                "scripts/deploy.sh".into(),
            ],
            // 2 untested out of 8 code files → 75% coverage → no regression
            file_stats: (0..8)
                .map(|i| (format!("src/mod_{}.rs", i), 'M', 10, 5))
                .collect(),
            ..Default::default()
        };

        let result = analyze(&ctx);

        assert_eq!(result.untested_code_count, 2);
        assert_eq!(result.untested_non_code_count, 4);
        assert_eq!(result.untested_critical_count, 2); // backward compat = code only
        assert_eq!(
            result.untested_critical_files,
            vec!["src/main.rs", "src/lib.rs"]
        );
        assert!(!result.coverage_regression_detected); // 2/8 = 75% coverage, no ratio drop
    }

    #[test]
    fn test_analyze_test_files_excluded() {
        let ctx = RegressionContext {
            untested_critical_files: vec![
                "src/main.rs".into(),
                "tests/integration.rs".into(),
                "__tests__/App.test.tsx".into(),
            ],
            ..Default::default()
        };

        let result = analyze(&ctx);
        assert_eq!(result.untested_code_count, 1); // only main.rs
        assert_eq!(result.untested_non_code_count, 0);
    }

    #[test]
    fn test_analyze_rust_inline_tests_remove_file_from_untested_code() {
        let ctx = RegressionContext {
            untested_critical_files: vec!["src/lib.rs".into()],
            file_stats: vec![("src/lib.rs".into(), 'M', 20, 3)],
            patch_text: Some(
                "diff --git a/src/lib.rs b/src/lib.rs\n\
                 --- a/src/lib.rs\n\
                 +++ b/src/lib.rs\n\
                 @@ -1,3 +1,9 @@\n\
                  pub fn hello() {}\n\
                 +#[cfg(test)]\n\
                 +mod tests {\n\
                 +    #[test]\n\
                 +    fn it_works() {}\n\
                 +}\n"
                    .into(),
            ),
            ..Default::default()
        };

        let result = analyze(&ctx);

        assert_eq!(result.untested_code_count, 0);
        assert_eq!(result.untested_critical_count, 0);
        assert_eq!(result.rust_inline_test_files, vec!["src/lib.rs"]);
        assert_eq!(result.rust_inline_test_caveat_count, 0);
    }

    #[test]
    fn test_analyze_non_code_capped() {
        let mut files: Vec<String> = (0..30).map(|i| format!("docs/page_{}.md", i)).collect();
        files.push("src/real.rs".into());

        let ctx = RegressionContext {
            untested_critical_files: files,
            ..Default::default()
        };

        let result = analyze(&ctx);
        assert_eq!(result.untested_code_count, 1);
        assert_eq!(result.untested_non_code_count, 30); // full count
        assert_eq!(result.untested_non_code_files.len(), NON_CODE_CAP); // capped in vec
    }

    // -- Regression detection --

    #[test]
    fn test_no_regression_for_non_code_only() {
        let ctx = RegressionContext {
            untested_critical_files: (0..20).map(|i| format!("docs/page_{}.md", i)).collect(),
            ..Default::default()
        };

        let result = analyze(&ctx);
        assert!(!result.coverage_regression_detected);
        assert_eq!(result.untested_code_count, 0);
        assert_eq!(result.untested_non_code_count, 20);
    }

    #[test]
    fn test_regression_detected_many_code_files() {
        let ctx = RegressionContext {
            untested_critical_files: (0..8).map(|i| format!("src/module_{}.rs", i)).collect(),
            ..Default::default()
        };

        let result = analyze(&ctx);
        assert!(result.coverage_regression_detected); // 8 untested, 0% coverage (fallback)
        assert_eq!(result.untested_code_count, 8);
    }

    #[test]
    fn test_regression_detected_ratio_drop() {
        let ctx = RegressionContext {
            coverage_ratio: Some(0.5),
            base_coverage_ratio: Some(0.8),
            ..Default::default()
        };

        let result = analyze(&ctx);
        assert!(result.coverage_regression_detected);
        assert!(result.coverage_delta.unwrap() < -0.01);
    }

    #[test]
    fn test_regression_detected_untested_increased() {
        let ctx = RegressionContext {
            untested_critical_files: vec!["a.rs".into(), "b.rs".into(), "c.rs".into()],
            base_untested_critical_count: Some(1),
            ..Default::default()
        };

        let result = analyze(&ctx);
        assert!(result.coverage_regression_detected); // 3 > base 1
    }

    #[test]
    fn test_no_regression_few_code_files_good_ratio() {
        // 2 untested out of 10 code files → 80% coverage → no regression
        let ctx = RegressionContext {
            untested_critical_files: vec!["a.rs".into(), "b.rs".into()],
            file_stats: (0..10)
                .map(|i| (format!("src/mod_{}.rs", i), 'M', 10, 5))
                .collect(),
            ..Default::default()
        };

        let result = analyze(&ctx);
        assert!(!result.coverage_regression_detected); // 2/10 = 80% coverage, no ratio drop
    }

    // -- Serde --

    #[test]
    fn test_serde_skip_empty_non_code() {
        let result = TestRegression::default();
        let json = serde_json::to_string(&result).unwrap();
        assert!(!json.contains("untested_non_code_files"));
    }

    #[test]
    fn test_serde_includes_non_code_when_present() {
        let result = TestRegression {
            untested_non_code_files: vec!["README.md".into()],
            untested_non_code_count: 1,
            ..Default::default()
        };
        let json = serde_json::to_string(&result).unwrap();
        assert!(json.contains("untested_non_code_files"));
    }

    #[test]
    fn test_round_trip_serde() {
        let ctx = RegressionContext {
            coverage_ratio: Some(0.7),
            base_coverage_ratio: Some(0.8),
            untested_critical_files: vec![
                "src/main.rs".into(),
                "README.md".into(),
                "tests/foo.rs".into(),
            ],
            base_untested_critical_count: Some(0),
            ..Default::default()
        };

        let result = analyze(&ctx);
        let json = serde_json::to_string_pretty(&result).unwrap();
        let deserialized: TestRegression = serde_json::from_str(&json).unwrap();

        assert_eq!(deserialized.untested_code_count, result.untested_code_count);
        assert_eq!(
            deserialized.untested_non_code_count,
            result.untested_non_code_count
        );
        assert_eq!(
            deserialized.untested_critical_count,
            result.untested_critical_count
        );
        assert_eq!(
            deserialized.coverage_regression_detected,
            result.coverage_regression_detected
        );
        assert_eq!(
            deserialized.coverage_improvement_detected,
            result.coverage_improvement_detected
        );
    }

    // -- Config-like classification tests --

    #[test]
    fn test_config_like_playwright_config() {
        assert!(is_config_like("playwright.config.ts"));
        assert!(is_config_like("src/playwright.config.ts"));
    }

    #[test]
    fn test_config_like_vite_config() {
        assert!(is_config_like("vite.config.ts"));
        assert!(is_config_like("vite.config.js"));
        assert!(is_config_like("vite.config.mjs"));
    }

    #[test]
    fn test_config_like_other_configs() {
        assert!(is_config_like("jest.config.js"));
        assert!(is_config_like("tailwind.config.js"));
        assert!(is_config_like("tailwind.config.cjs"));
        assert!(is_config_like("vitest.config.ts"));
        assert!(is_config_like("webpack.config.js"));
    }

    #[test]
    fn test_config_like_setup_files() {
        assert!(is_config_like("jest.setup.ts"));
        assert!(is_config_like("vitest.setup.js"));
    }

    #[test]
    fn test_config_like_barrel_index() {
        assert!(is_config_like("index.ts"));
        assert!(is_config_like("index.js"));
        assert!(is_config_like("index.tsx"));
        assert!(is_config_like("index.jsx"));
    }

    #[test]
    fn test_config_like_nested_barrel() {
        assert!(is_config_like("src/utils/index.ts"));
        assert!(is_config_like("packages/core/index.js"));
    }

    #[test]
    fn test_config_like_types() {
        assert!(is_config_like("types.ts"));
        assert!(is_config_like("src/types.ts"));
    }

    #[test]
    fn test_config_like_declaration_files() {
        assert!(is_config_like("types.d.ts"));
        assert!(is_config_like("src/env.d.ts"));
        assert!(is_config_like("global.d.ts"));
    }

    #[test]
    fn test_not_config_like_regular_source() {
        assert!(!is_config_like("src/main.ts"));
        assert!(!is_config_like("src/main.rs"));
        assert!(!is_config_like("src/app.tsx"));
        assert!(!is_config_like("src/utils.ts"));
        assert!(!is_config_like("src/handler.js"));
        assert!(!is_config_like("src/config.rs")); // .rs config is real code
    }

    #[test]
    fn test_config_like_devtools() {
        assert!(is_config_like("src/devtools/browserMocks.ts"));
        assert!(is_config_like("devtools/panel.tsx"));
    }

    #[test]
    fn test_config_like_storybook() {
        assert!(is_config_like(".storybook/main.ts"));
        assert!(is_config_like("storybook/preview.ts"));
        assert!(is_config_like("src/stories/Button.stories.tsx"));
    }

    #[test]
    fn test_config_like_mock_files() {
        assert!(is_config_like("src/devtools/browserMocks.ts"));
        assert!(is_config_like("src/utils/mockData.ts"));
        assert!(is_config_like("src/__mocks__/apiMock.ts")); // also caught by is_test_file
        // Non-mock files with "mock" substring should NOT match unless basename has it
        assert!(!is_config_like("src/hammock/utils.ts"));
    }

    #[test]
    fn test_config_like_routed_to_non_code() {
        let ctx = RegressionContext {
            untested_critical_files: vec![
                "playwright.config.ts".into(),
                "vite.config.ts".into(),
                "src/index.ts".into(),
                "src/types.ts".into(),
                "src/main.ts".into(), // real code
            ],
            file_stats: vec![
                ("playwright.config.ts".into(), 'M', 5, 2),
                ("vite.config.ts".into(), 'M', 3, 1),
                ("src/index.ts".into(), 'M', 2, 0),
                ("src/types.ts".into(), 'M', 10, 3),
                ("src/main.ts".into(), 'M', 20, 5),
                // Additional tested code files for good ratio
                ("src/app.ts".into(), 'M', 10, 2),
                ("src/utils.ts".into(), 'M', 5, 1),
            ],
            ..Default::default()
        };

        let result = analyze(&ctx);
        // Only main.ts is real untested code; configs/barrel/types → non-code
        assert_eq!(result.untested_code_count, 1);
        assert_eq!(result.untested_non_code_count, 4);
        assert!(result.untested_code_files.contains(&"src/main.ts".into()));
    }

    // -- Adaptive threshold tests --

    #[test]
    fn test_adaptive_threshold_good_ratio_not_detected() {
        // 6 untested out of 15 code files → 60% coverage → NOT detected
        let untested: Vec<String> = (0..6).map(|i| format!("src/mod_{}.rs", i)).collect();
        let file_stats: Vec<(String, char, usize, usize)> = (0..15)
            .map(|i| (format!("src/mod_{}.rs", i), 'M', 10, 5))
            .collect();

        let ctx = RegressionContext {
            untested_critical_files: untested,
            file_stats,
            ..Default::default()
        };

        let result = analyze(&ctx);
        assert!(!result.coverage_regression_detected);
        assert_eq!(result.untested_code_count, 6);
    }

    #[test]
    fn test_adaptive_threshold_low_ratio_detected() {
        // 6 untested out of 8 code files → 25% coverage → detected (< 40%)
        let untested: Vec<String> = (0..6).map(|i| format!("src/mod_{}.rs", i)).collect();
        let file_stats: Vec<(String, char, usize, usize)> = (0..8)
            .map(|i| (format!("src/mod_{}.rs", i), 'M', 10, 5))
            .collect();

        let ctx = RegressionContext {
            untested_critical_files: untested,
            file_stats,
            ..Default::default()
        };

        let result = analyze(&ctx);
        assert!(result.coverage_regression_detected);
        assert_eq!(result.untested_code_count, 6);
    }

    #[test]
    fn test_adaptive_threshold_hard_cap_with_good_ratio() {
        // 12 untested out of 30 code files → 60% coverage → hard_cap requires < 50%
        let untested: Vec<String> = (0..12).map(|i| format!("src/mod_{}.rs", i)).collect();
        let file_stats: Vec<(String, char, usize, usize)> = (0..30)
            .map(|i| (format!("src/mod_{}.rs", i), 'M', 10, 5))
            .collect();

        let ctx = RegressionContext {
            untested_critical_files: untested,
            file_stats,
            ..Default::default()
        };

        let result = analyze(&ctx);
        assert!(!result.coverage_regression_detected); // 60% > 50% threshold, no hard_cap
        assert!(!result.coverage_improvement_detected);
    }

    #[test]
    fn test_adaptive_threshold_hard_cap_with_low_ratio() {
        // 12 untested out of 15 code files → 20% coverage → hard_cap fires
        let untested: Vec<String> = (0..12).map(|i| format!("src/mod_{}.rs", i)).collect();
        let file_stats: Vec<(String, char, usize, usize)> = (0..15)
            .map(|i| (format!("src/mod_{}.rs", i), 'M', 10, 5))
            .collect();

        let ctx = RegressionContext {
            untested_critical_files: untested,
            file_stats,
            ..Default::default()
        };

        let result = analyze(&ctx);
        assert!(result.coverage_regression_detected); // 20% < 50%, hard_cap fires
        assert!(!result.coverage_improvement_detected);
    }

    #[test]
    fn test_adaptive_threshold_coverage_delta_drop() {
        // Good code ratio but coverage_delta dropped > 5%
        let ctx = RegressionContext {
            coverage_ratio: Some(0.5),
            base_coverage_ratio: Some(0.6),
            file_stats: vec![("src/main.rs".into(), 'M', 10, 5)],
            ..Default::default()
        };

        let result = analyze(&ctx);
        assert!(result.coverage_regression_detected); // delta = -0.1 < -0.05
        assert!(!result.coverage_improvement_detected);
    }

    #[test]
    fn test_adaptive_threshold_large_pr_high_ratio_not_detected() {
        // Real-world scenario: 87-file PR, 12 code files untested but 60% ratio
        let untested: Vec<String> = (0..12).map(|i| format!("src/feature_{}.ts", i)).collect();
        // 30 code files total → 12/30 untested = 60% coverage
        let mut file_stats: Vec<(String, char, usize, usize)> = (0..30)
            .map(|i| (format!("src/feature_{}.ts", i), 'M', 10, 5))
            .collect();
        // Plus 57 non-code files (configs, docs, etc.)
        for i in 0..57 {
            file_stats.push((format!("docs/page_{}.md", i), 'M', 5, 2));
        }

        let ctx = RegressionContext {
            untested_critical_files: untested,
            file_stats,
            ..Default::default()
        };

        let result = analyze(&ctx);
        // 12/30 = 60% coverage > 50% → hard_cap doesn't fire, no regression
        assert!(!result.coverage_regression_detected);
        assert!(!result.coverage_improvement_detected);
    }

    #[test]
    fn test_adaptive_threshold_boundary_at_ten() {
        // Exactly 10 untested (not > 10) with good ratio → NOT detected
        let untested: Vec<String> = (0..10).map(|i| format!("src/mod_{}.rs", i)).collect();
        let file_stats: Vec<(String, char, usize, usize)> = (0..25)
            .map(|i| (format!("src/mod_{}.rs", i), 'M', 10, 5))
            .collect();

        let ctx = RegressionContext {
            untested_critical_files: untested,
            file_stats,
            ..Default::default()
        };

        let result = analyze(&ctx);
        assert!(!result.coverage_regression_detected); // 10/25 = 60% coverage, 10 not > 10
        assert!(!result.coverage_improvement_detected);
    }

    #[test]
    fn test_coverage_improved_suppresses_soft_signals() {
        // Real scenario: PR adds many files, coverage improves from 12% to 21%
        // Despite many untested files, this is NOT a regression.
        let untested: Vec<String> = (0..30).map(|i| format!("src/new_{}.rs", i)).collect();
        let file_stats: Vec<(String, char, usize, usize)> = (0..40)
            .map(|i| (format!("src/new_{}.rs", i), 'M', 50, 20))
            .collect();

        let ctx = RegressionContext {
            untested_critical_files: untested,
            base_untested_critical_count: Some(12),
            coverage_ratio: Some(0.213),
            base_coverage_ratio: Some(0.116),
            file_stats,
            ..Default::default()
        };

        let result = analyze(&ctx);
        // Coverage improved by ~10pp — soft signals (low_coverage, hard_cap,
        // untested_increased) are suppressed because this is not a regression.
        assert!(!result.coverage_regression_detected);
        assert!(result.coverage_improvement_detected);
    }

    #[test]
    fn test_coverage_improvement_detected_with_higher_ratio_and_more_untested_files() {
        let untested: Vec<String> = (0..12).map(|i| format!("src/new_{}.rs", i)).collect();
        let file_stats: Vec<(String, char, usize, usize)> = (0..47)
            .map(|i| (format!("src/new_{}.rs", i), 'M', 25, 5))
            .collect();

        let ctx = RegressionContext {
            untested_critical_files: untested,
            base_untested_critical_count: Some(5),
            coverage_ratio: Some(0.213),
            base_coverage_ratio: Some(0.116),
            file_stats,
            ..Default::default()
        };

        let result = analyze(&ctx);
        assert!(
            result
                .coverage_delta
                .is_some_and(|delta| (delta - 0.097).abs() < 1e-9)
        );
        assert!(!result.coverage_regression_detected);
        assert!(result.coverage_improvement_detected);
    }
}
