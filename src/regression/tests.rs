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
const TEST_SUFFIX_MARKERS: &[&str] = &["_test", "_tests", ".test", "_spec", ".spec"];

/// Definition fingerprint of the source/test-change matching metric: what counts
/// as a test file, what counts as code, and how the ratio is formed. Bump it
/// whenever any of those change so that an earlier run's ratio is no longer
/// treated as comparable. It is NOT the generator SHA: two builds with the same
/// definition stay comparable, and a different review range alone does not
/// break comparability either (the trend is previous-run, never PR-base).
///
/// `xctest-v5`: Swift source classification and XCTest source/test matching
/// accept case-insensitive Swift extensions. Other language classification is unchanged.
pub const COVERAGE_METRIC_FINGERPRINT: &str = "source_test_change_match/xctest-v5";

// ---------------------------------------------------------------------------
// Classification helpers
// ---------------------------------------------------------------------------

/// Returns `true` if `path` looks like a source-code file based on extension.
pub fn is_code_file(path: &str) -> bool {
    Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|ext| CODE_EXTENSIONS.contains(&ext) || ext.eq_ignore_ascii_case("swift"))
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
/// - Swift XCTest targets (`<Target>Tests/` directory, `SomethingTests.swift`)
///   count only for `.swift` files; see [`is_xctest_file`]
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

    // 6. Swift XCTest convention (Swift files only)
    if is_xctest_file(path) {
        return true;
    }

    false
}

/// Swift XCTest convention: Xcode names a test target `<Target>Tests` (or
/// `<Target>UITests`) and that name is both the directory holding its sources
/// and the `SomethingTests.swift` basename of each test case.
///
/// `Tests` is matched as a CamelCase word boundary, so the check is
/// case-sensitive on purpose: `Protests.swift` or `requests/` are not tests,
/// while `OverlayChannelMapTests.swift` and `CodescribeTests/Helpers.swift`
/// are. The lowercase markers in [`is_test_file`] keep handling `tests/`,
/// `_tests` and the other cross-language conventions.
///
/// Only `.swift` files qualify. The directory marker alone would otherwise turn
/// a `README.md` or a fixture inside the test target into a test file, and
/// Rust/JS/Python keep their existing markers unchanged.
fn is_xctest_file(path: &str) -> bool {
    let (dirs, basename) = path.rsplit_once('/').unwrap_or(("", path));
    let basename = Path::new(basename);
    let is_swift = basename
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("swift"));
    if !is_swift {
        return false;
    }
    let stem = basename
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("");
    stem.ends_with("Tests") || dirs.split('/').any(|dir| dir.ends_with("Tests"))
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

/// Whether the previous-run metric can be compared with the current one.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CoverageComparisonStatus {
    /// A previous-run metric exists and carries the same definition fingerprint.
    Comparable,
    /// A previous-run metric exists but was produced under a different or
    /// unrecorded definition; no delta or historical regression is derived.
    NotComparable,
    /// No previous-run metric at all.
    #[default]
    Unavailable,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TestRegression {
    pub coverage_ratio: Option<f64>,
    /// Raw previous-run value, kept as provenance even when not comparable.
    pub base_coverage_ratio: Option<f64>,
    /// `current - base`, only when the comparison is [`CoverageComparisonStatus::Comparable`].
    pub coverage_delta: Option<f64>,

    /// Definition fingerprint of this run's metric ([`COVERAGE_METRIC_FINGERPRINT`]).
    /// Empty when read back from a report written before fingerprints existed.
    #[serde(default)]
    pub metric_fingerprint: String,
    #[serde(default)]
    pub comparison_status: CoverageComparisonStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comparison_reason: Option<String>,

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
    /// Current-change signal: few changed code files have a matching test
    /// change (ratio below 40%, or more than 10 untested below 50%). It
    /// describes this change only and needs no previous run.
    #[serde(default)]
    pub low_test_match_detected: bool,
    /// Historical signal: the comparable previous-run ratio dropped, or the
    /// untested count grew against a comparable baseline. Never `true` without
    /// a [`CoverageComparisonStatus::Comparable`] comparison.
    pub coverage_regression_detected: bool,
}

/// Decide whether the previous-run metric in `ctx` may be compared with the
/// current one: both runs must name the same repository (`meta.repo.full_name`,
/// inferred from `origin`; an explicit `--output-dir` can hold runs of several
/// repositories), both must carry a measured ratio in `0..=1` (a count-only
/// or 0/0 record is not a measurement), and the previous run must carry the
/// identical definition fingerprint. A different review range alone does not
/// break comparability.
fn comparison_status(ctx: &RegressionContext) -> (CoverageComparisonStatus, Option<String>) {
    if ctx.base_coverage_ratio.is_none() && ctx.base_untested_critical_count.is_none() {
        return (
            CoverageComparisonStatus::Unavailable,
            Some("no previous-run source/test-change metric recorded".into()),
        );
    }
    match (
        ctx.repo_identity.as_deref(),
        ctx.base_repo_identity.as_deref(),
    ) {
        (Some(current), Some(previous)) if current.eq_ignore_ascii_case(previous) => {}
        (Some(current), Some(previous)) => {
            return (
                CoverageComparisonStatus::NotComparable,
                Some(format!(
                    "previous run reviewed repository `{previous}`, current run reviews `{current}`"
                )),
            );
        }
        _ => {
            return (
                CoverageComparisonStatus::NotComparable,
                Some(
                    "repository identity of the previous or the current run is not \
                     established (no `meta.repo.full_name`)"
                        .into(),
                ),
            );
        }
    }
    // Both sides must be measured ratios over changed source files. A
    // count-only or 0/0 previous record (a docs-only run) and a current run
    // without changed source files have no population to compare, whatever
    // the fingerprint says.
    let (Some(current), Some(previous)) = (ctx.coverage_ratio, ctx.base_coverage_ratio) else {
        return (
            CoverageComparisonStatus::NotComparable,
            Some(
                "previous or current run has no measured source/test-change ratio \
                 (no changed source files, or a count-only record)"
                    .into(),
            ),
        );
    };
    if !(0.0..=1.0).contains(&previous) || !(0.0..=1.0).contains(&current) {
        return (
            CoverageComparisonStatus::NotComparable,
            Some(format!(
                "source/test-change ratio out of range: previous {previous}, current {current}"
            )),
        );
    }
    match ctx.base_metric_fingerprint.as_deref() {
        None => (
            CoverageComparisonStatus::NotComparable,
            Some(
                "previous run carries no metric fingerprint (recorded before the \
                 test-file classifier and matching definition were pinned)"
                    .into(),
            ),
        ),
        Some(previous) if previous != COVERAGE_METRIC_FINGERPRINT => (
            CoverageComparisonStatus::NotComparable,
            Some(format!(
                "previous run metric fingerprint `{previous}` differs from current `{COVERAGE_METRIC_FINGERPRINT}`"
            )),
        ),
        Some(_) => (CoverageComparisonStatus::Comparable, None),
    }
}

fn is_zero(v: &usize) -> bool {
    *v == 0
}

// ---------------------------------------------------------------------------
// Analysis
// ---------------------------------------------------------------------------

pub fn analyze(ctx: &RegressionContext) -> TestRegression {
    let (comparison_status, comparison_reason) = comparison_status(ctx);
    let comparable = comparison_status == CoverageComparisonStatus::Comparable;
    let coverage_delta = match (ctx.coverage_ratio, ctx.base_coverage_ratio) {
        (Some(current), Some(base)) if comparable => Some(current - base),
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

    // Historical regression — only against a comparable previous run
    let ratio_dropped = coverage_delta.is_some_and(|d| d < -0.01);
    let untested_increased = comparable
        && ctx
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

    // Current-change signal: describes this change's matching only.
    let low_coverage = untested_code_count > 0 && code_coverage_ratio < 0.4;
    let hard_cap = untested_code_count > 10 && code_coverage_ratio < 0.5;
    let low_test_match_detected = low_coverage || hard_cap;

    // A grown untested count is not a regression when the comparable ratio
    // actually improved (a PR that adds many files can still raise the ratio).
    let coverage_improved = coverage_delta.is_some_and(|d| d > 0.01);

    let detected =
        ratio_dropped || coverage_delta_drop || (untested_increased && !coverage_improved);

    TestRegression {
        coverage_ratio: ctx.coverage_ratio,
        base_coverage_ratio: ctx.base_coverage_ratio,
        coverage_delta,
        metric_fingerprint: COVERAGE_METRIC_FINGERPRINT.to_string(),
        comparison_status,
        comparison_reason,
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
        low_test_match_detected,
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
        assert!(is_test_file("src/artifacts/dashboard/evidence_tests.rs"));
        assert!(is_test_file("src/artifacts/dashboard/trends_tests.rs"));
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
        assert!(!is_test_file("src/testsupport.rs"));
        assert!(!is_test_file("src/evidence_tests_support.rs"));
    }

    /// XCTest: the `<Target>Tests` directory and the `SomethingTests.swift`
    /// basename are both test markers for Swift sources, including helper
    /// Swift files inside the target that carry no suffix of their own.
    #[test]
    fn test_is_test_file_xctest_convention() {
        assert!(is_test_file(
            "macos/CodescribeTests/OverlayChannelMapTests.swift"
        ));
        assert!(is_test_file(
            "macos/CodescribeTests/NativeAccessibilityProbe.swift"
        ));
        assert!(is_test_file(
            "macos/CodescribeTests/Support/XCTestHelpers.swift"
        ));
        assert!(is_test_file("macos/CodescribeUITests/LaunchTests.swift"));
        assert!(is_test_file("Sources/Feature/SomethingTests.swift"));
        assert!(is_test_file("SomethingTests.swift"));
        assert!(is_test_file("App/Tests.swift"));
        // Lowercase conventions keep working for Swift as before
        assert!(is_test_file("scripts/tests/native-ax-probe.swift"));
        assert!(is_test_file("Sources/Feature/something_tests.swift"));
    }

    /// The XCTest marker is Swift-only and `Tests` is a CamelCase boundary:
    /// documentation and data inside the target, production Swift next to it,
    /// ordinary names that merely contain `test`, and other languages stay out.
    #[test]
    fn test_is_not_test_file_xctest_lookalikes() {
        assert!(!is_test_file("macos/CodescribeTests/README.md"));
        assert!(!is_test_file("macos/CodescribeTests/Fixtures/sample.json"));
        assert!(!is_test_file(
            "macos/Codescribe/Screens/Overlay/OverlayChannelMapModel.swift"
        ));
        assert!(!is_test_file("macos/Codescribe/Latest.swift"));
        assert!(!is_test_file("macos/Codescribe/Contest.swift"));
        assert!(!is_test_file("macos/Codescribe/TestSupport.swift"));
        assert!(!is_test_file("macos/Codescribe/Protests.swift"));
        assert!(!is_test_file("macos/Codescribe/Requests.swift"));
        assert!(!is_test_file("macos/Codescribe/Manifests.swift"));
        assert!(!is_test_file("macos/Protests/Banner.swift"));
        assert!(!is_test_file("scripts/test-swift.sh"));
        assert!(!is_test_file("src/FooTests.rs"));
        assert!(!is_test_file("packages/CodescribeTests/index.ts"));
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
        assert!(!result.coverage_regression_detected); // no previous run at all
        assert!(!result.low_test_match_detected); // 2/8 = 75% matched
        assert_eq!(
            result.comparison_status,
            CoverageComparisonStatus::Unavailable
        );
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

    /// Eight untested code files and no previous run: that is low current
    /// matching, not a historical regression (PRV-02).
    #[test]
    fn test_low_match_without_baseline_is_not_a_regression() {
        let ctx = RegressionContext {
            untested_critical_files: (0..8).map(|i| format!("src/module_{}.rs", i)).collect(),
            ..Default::default()
        };

        let result = analyze(&ctx);
        assert!(result.low_test_match_detected); // 8 untested, 0% matched (fallback)
        assert!(!result.coverage_regression_detected);
        assert_eq!(
            result.comparison_status,
            CoverageComparisonStatus::Unavailable
        );
        assert!(result.comparison_reason.is_some());
        assert_eq!(result.coverage_delta, None);
        assert_eq!(result.untested_code_count, 8);
        assert_eq!(result.metric_fingerprint, COVERAGE_METRIC_FINGERPRINT);
    }

    #[test]
    fn test_regression_detected_ratio_drop() {
        let ctx = RegressionContext {
            coverage_ratio: Some(0.5),
            base_coverage_ratio: Some(0.8),
            base_metric_fingerprint: Some(COVERAGE_METRIC_FINGERPRINT.into()),
            repo_identity: Some("acme/widgets".into()),
            base_repo_identity: Some("acme/widgets".into()),
            ..Default::default()
        };

        let result = analyze(&ctx);
        assert_eq!(
            result.comparison_status,
            CoverageComparisonStatus::Comparable
        );
        assert_eq!(result.comparison_reason, None);
        assert!(result.coverage_regression_detected);
        assert!(result.coverage_delta.unwrap() < -0.01);
    }

    /// Same previous-run value, but recorded before fingerprints existed: the
    /// raw baseline stays as provenance, no delta and no regression is derived.
    #[test]
    fn test_baseline_without_fingerprint_is_not_comparable() {
        let ctx = RegressionContext {
            coverage_ratio: Some(0.2),
            base_coverage_ratio: Some(0.8),
            repo_identity: Some("acme/widgets".into()),
            base_repo_identity: Some("acme/widgets".into()),
            ..Default::default()
        };

        let result = analyze(&ctx);
        assert_eq!(
            result.comparison_status,
            CoverageComparisonStatus::NotComparable
        );
        assert!(
            result
                .comparison_reason
                .as_deref()
                .unwrap()
                .contains("no metric fingerprint")
        );
        assert_eq!(result.base_coverage_ratio, Some(0.8));
        assert_eq!(result.coverage_delta, None);
        assert!(!result.coverage_regression_detected);
        assert!(!result.coverage_improvement_detected);
    }

    /// A previous run under another classifier/matching definition (here: the
    /// pre-XCTest fingerprint) must not be compared, however large the drop.
    #[test]
    fn test_baseline_with_other_fingerprint_is_not_comparable() {
        let ctx = RegressionContext {
            coverage_ratio: Some(0.2),
            base_coverage_ratio: Some(0.8),
            base_untested_critical_count: Some(1),
            untested_critical_files: vec!["a.rs".into(), "b.rs".into(), "c.rs".into()],
            base_metric_fingerprint: Some("source_test_change_match/v1".into()),
            repo_identity: Some("acme/widgets".into()),
            base_repo_identity: Some("acme/widgets".into()),
            ..Default::default()
        };

        let result = analyze(&ctx);
        assert_eq!(
            result.comparison_status,
            CoverageComparisonStatus::NotComparable
        );
        assert!(
            result
                .comparison_reason
                .as_deref()
                .unwrap()
                .contains("source_test_change_match/v1")
        );
        assert_eq!(result.coverage_delta, None);
        assert!(!result.coverage_regression_detected); // neither ratio nor count may be compared
        assert!(result.low_test_match_detected); // 3 untested of 3 is still low current matching
    }

    #[test]
    fn review80_round2_swift_code_classification_is_case_insensitive_only_for_swift() {
        for path in [
            "Sources/App/Parser.swift",
            "Sources/App/Parser.SWIFT",
            "Sources/App/Parser.SwIfT",
        ] {
            assert!(is_code_file(path), "{path}");
            assert!(!is_test_file(path), "{path}");
        }
        for path in [
            "src/Parser.RS",
            "src/Parser.JS",
            "Sources/Parser.SWIFT.txt",
            "Sources/Parser.swift.md",
        ] {
            assert!(
                !is_code_file(path),
                "unrelated extension behavior changed: {path}"
            );
        }
    }

    #[test]
    fn review80_round2_swift_source_classification_rejects_v4_history() {
        assert_eq!(
            COVERAGE_METRIC_FINGERPRINT,
            "source_test_change_match/xctest-v5"
        );
        let ctx = RegressionContext {
            coverage_ratio: Some(0.5),
            base_coverage_ratio: Some(0.8),
            base_metric_fingerprint: Some("source_test_change_match/xctest-v4".into()),
            repo_identity: Some("acme/widgets".into()),
            base_repo_identity: Some("acme/widgets".into()),
            ..Default::default()
        };
        let result = analyze(&ctx);
        assert_eq!(
            result.comparison_status,
            CoverageComparisonStatus::NotComparable
        );
        assert_eq!(result.base_coverage_ratio, Some(0.8));
        assert_eq!(result.coverage_delta, None);
        assert!(!result.coverage_regression_detected);
        assert!(!result.coverage_improvement_detected);
    }

    #[test]
    fn review80_case_insensitive_swift_matching_rejects_v3_history() {
        let ctx = RegressionContext {
            coverage_ratio: Some(0.5),
            base_coverage_ratio: Some(0.8),
            base_metric_fingerprint: Some("source_test_change_match/xctest-v3".into()),
            repo_identity: Some("acme/widgets".into()),
            base_repo_identity: Some("acme/widgets".into()),
            ..Default::default()
        };
        let result = analyze(&ctx);
        assert_eq!(
            result.comparison_status,
            CoverageComparisonStatus::NotComparable
        );
        assert_eq!(result.base_coverage_ratio, Some(0.8));
        assert_eq!(result.coverage_delta, None);
        assert!(!result.coverage_regression_detected);
    }

    #[test]
    fn test_xctest_matching_v2_baseline_is_not_compared() {
        let ctx = RegressionContext {
            coverage_ratio: Some(0.5),
            base_coverage_ratio: Some(0.8),
            base_metric_fingerprint: Some("source_test_change_match/xctest-v2".into()),
            repo_identity: Some("acme/widgets".into()),
            base_repo_identity: Some("acme/widgets".into()),
            ..Default::default()
        };
        let result = analyze(&ctx);
        assert_eq!(
            result.comparison_status,
            CoverageComparisonStatus::NotComparable
        );
        assert_eq!(result.base_coverage_ratio, Some(0.8));
        assert_eq!(result.coverage_delta, None);
        assert!(!result.coverage_regression_detected);
        assert!(
            result
                .comparison_reason
                .as_deref()
                .unwrap()
                .contains("xctest-v2")
        );
    }

    /// Comparable inputs without a drop: no regression, and the status says
    /// the comparison really happened.
    #[test]
    fn test_comparable_baseline_without_drop_is_not_a_regression() {
        let ctx = RegressionContext {
            coverage_ratio: Some(0.5),
            base_coverage_ratio: Some(0.5),
            untested_critical_files: vec!["a.rs".into(), "b.rs".into()],
            base_untested_critical_count: Some(2),
            file_stats: (0..10)
                .map(|i| (format!("src/mod_{}.rs", i), 'M', 10, 5))
                .collect(),
            base_metric_fingerprint: Some(COVERAGE_METRIC_FINGERPRINT.into()),
            repo_identity: Some("acme/widgets".into()),
            base_repo_identity: Some("acme/widgets".into()),
            ..Default::default()
        };

        let result = analyze(&ctx);
        assert_eq!(
            result.comparison_status,
            CoverageComparisonStatus::Comparable
        );
        assert_eq!(result.coverage_delta, Some(0.0));
        assert!(!result.coverage_regression_detected);
        assert!(!result.low_test_match_detected);
    }

    /// No changed source files at all: nothing is measured, so neither a 100%
    /// ratio nor a regression may appear.
    #[test]
    fn test_zero_denominator_reports_nothing() {
        let ctx = RegressionContext::default();

        let result = analyze(&ctx);
        assert_eq!(result.coverage_ratio, None);
        assert_eq!(result.coverage_delta, None);
        assert_eq!(result.untested_code_count, 0);
        assert!(!result.low_test_match_detected);
        assert!(!result.coverage_regression_detected);
        assert_eq!(
            result.comparison_status,
            CoverageComparisonStatus::Unavailable
        );
    }

    /// Reports written before PRV-02 deserialize with honest defaults.
    #[test]
    fn test_serde_defaults_for_reports_without_comparison_fields() {
        let legacy = r#"{"coverage_ratio":0.1,"base_coverage_ratio":null,"coverage_delta":null,
            "untested_critical_files":[],"untested_critical_count":0,
            "coverage_regression_detected":true}"#;
        let result: TestRegression = serde_json::from_str(legacy).unwrap();
        assert_eq!(result.metric_fingerprint, "");
        assert_eq!(
            result.comparison_status,
            CoverageComparisonStatus::Unavailable
        );
        assert_eq!(result.comparison_reason, None);
        assert!(!result.low_test_match_detected);
        assert!(result.coverage_regression_detected); // legacy value is kept, not reinterpreted
    }

    /// A previous run that recorded only a count (ratio null: a docs-only run
    /// with no changed source files) measured nothing; its count must not turn
    /// the current untested files into a historical regression.
    #[test]
    fn test_count_only_baseline_is_not_comparable() {
        let ctx = RegressionContext {
            coverage_ratio: Some(0.0),
            base_coverage_ratio: None,
            base_untested_critical_count: Some(0),
            untested_critical_files: vec!["src/a.rs".into()],
            file_stats: vec![("src/a.rs".into(), 'M', 10, 5)],
            base_metric_fingerprint: Some(COVERAGE_METRIC_FINGERPRINT.into()),
            repo_identity: Some("acme/widgets".into()),
            base_repo_identity: Some("acme/widgets".into()),
            ..Default::default()
        };

        let result = analyze(&ctx);
        assert_eq!(
            result.comparison_status,
            CoverageComparisonStatus::NotComparable
        );
        assert!(
            result
                .comparison_reason
                .as_deref()
                .unwrap()
                .contains("count-only")
        );
        assert_eq!(result.coverage_delta, None);
        assert!(!result.coverage_regression_detected);
        assert!(result.low_test_match_detected);
    }

    /// A current run with no changed source files has no ratio; a measured
    /// previous ratio alone does not make the pair comparable.
    #[test]
    fn test_unmeasured_current_run_is_not_comparable() {
        let ctx = RegressionContext {
            coverage_ratio: None,
            base_coverage_ratio: Some(0.9),
            base_untested_critical_count: Some(2),
            base_metric_fingerprint: Some(COVERAGE_METRIC_FINGERPRINT.into()),
            repo_identity: Some("acme/widgets".into()),
            base_repo_identity: Some("acme/widgets".into()),
            ..Default::default()
        };

        let result = analyze(&ctx);
        assert_eq!(
            result.comparison_status,
            CoverageComparisonStatus::NotComparable
        );
        assert_eq!(result.coverage_delta, None);
        assert!(!result.coverage_regression_detected);
        assert!(!result.coverage_improvement_detected);
        assert!(!result.low_test_match_detected);
    }

    /// A ratio outside `0..=1` is not a measurement this metric produces.
    #[test]
    fn test_out_of_range_baseline_ratio_is_not_comparable() {
        let ctx = RegressionContext {
            coverage_ratio: Some(0.2),
            base_coverage_ratio: Some(1.5),
            base_metric_fingerprint: Some(COVERAGE_METRIC_FINGERPRINT.into()),
            repo_identity: Some("acme/widgets".into()),
            base_repo_identity: Some("acme/widgets".into()),
            ..Default::default()
        };

        let result = analyze(&ctx);
        assert_eq!(
            result.comparison_status,
            CoverageComparisonStatus::NotComparable
        );
        assert!(
            result
                .comparison_reason
                .as_deref()
                .unwrap()
                .contains("out of range")
        );
        assert!(!result.coverage_regression_detected);
    }

    /// GitHub slugs are case-insensitive: `--gh-repo Acme/Widgets` and an
    /// origin inferred as `acme/widgets` name the same repository.
    #[test]
    fn test_repository_identity_compares_case_insensitively() {
        let ctx = RegressionContext {
            coverage_ratio: Some(0.5),
            base_coverage_ratio: Some(0.8),
            base_metric_fingerprint: Some(COVERAGE_METRIC_FINGERPRINT.into()),
            repo_identity: Some("Acme/Widgets".into()),
            base_repo_identity: Some("acme/widgets".into()),
            ..Default::default()
        };

        let result = analyze(&ctx);
        assert_eq!(
            result.comparison_status,
            CoverageComparisonStatus::Comparable
        );
        assert!(result.coverage_regression_detected);
    }

    /// Same fingerprint, other repository (an explicit `--output-dir` shared by
    /// two projects): no comparison, however similar the numbers look.
    #[test]
    fn test_baseline_from_another_repository_is_not_comparable() {
        let ctx = RegressionContext {
            coverage_ratio: Some(0.2),
            base_coverage_ratio: Some(0.8),
            base_metric_fingerprint: Some(COVERAGE_METRIC_FINGERPRINT.into()),
            repo_identity: Some("acme/widgets".into()),
            base_repo_identity: Some("acme/gadgets".into()),
            ..Default::default()
        };

        let result = analyze(&ctx);
        assert_eq!(
            result.comparison_status,
            CoverageComparisonStatus::NotComparable
        );
        assert!(
            result
                .comparison_reason
                .as_deref()
                .unwrap()
                .contains("acme/gadgets")
        );
        assert_eq!(result.coverage_delta, None);
        assert!(!result.coverage_regression_detected);
    }

    /// No repository identity on one side (no `origin`, or a previous report
    /// without `meta.repo`): identity is unknown, so nothing is compared.
    #[test]
    fn test_unknown_repository_identity_is_not_comparable() {
        let ctx = RegressionContext {
            coverage_ratio: Some(0.2),
            base_coverage_ratio: Some(0.8),
            base_metric_fingerprint: Some(COVERAGE_METRIC_FINGERPRINT.into()),
            repo_identity: Some("acme/widgets".into()),
            ..Default::default()
        };

        let result = analyze(&ctx);
        assert_eq!(
            result.comparison_status,
            CoverageComparisonStatus::NotComparable
        );
        assert!(result.comparison_reason.as_deref().unwrap().contains("not"));
        assert_eq!(result.coverage_delta, None);
        assert!(!result.coverage_regression_detected);
    }

    #[test]
    fn test_regression_detected_untested_increased() {
        let ctx = RegressionContext {
            coverage_ratio: Some(0.5),
            base_coverage_ratio: Some(0.5),
            untested_critical_files: vec!["a.rs".into(), "b.rs".into(), "c.rs".into()],
            base_untested_critical_count: Some(1),
            base_metric_fingerprint: Some(COVERAGE_METRIC_FINGERPRINT.into()),
            repo_identity: Some("acme/widgets".into()),
            base_repo_identity: Some("acme/widgets".into()),
            ..Default::default()
        };

        let result = analyze(&ctx);
        assert!(result.coverage_regression_detected); // 3 > base 1, comparable
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
        assert!(result.low_test_match_detected);
        assert!(!result.coverage_regression_detected); // current signal, no previous run
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
        assert!(!result.low_test_match_detected); // 60% > 50% threshold, no hard_cap
        assert!(!result.coverage_regression_detected);
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
        assert!(result.low_test_match_detected); // 20% < 50%, hard_cap fires
        assert!(!result.coverage_regression_detected); // still no previous run
        assert!(!result.coverage_improvement_detected);
    }

    #[test]
    fn review80_hard_cap_fires_for_eleven_untested_at_47_percent() {
        // 11/21 untested leaves 10/21 matched (47.6%): above 40%, below 50%,
        // with more than ten untested files, so only the hard cap should fire.
        let untested: Vec<String> = (0..11).map(|i| format!("src/mod_{i}.rs")).collect();
        let file_stats: Vec<(String, char, usize, usize)> = (0..21)
            .map(|i| (format!("src/mod_{i}.rs"), 'M', 10, 5))
            .collect();

        let result = analyze(&RegressionContext {
            untested_critical_files: untested,
            file_stats,
            ..Default::default()
        });

        assert_eq!(result.untested_code_count, 11);
        assert!(result.low_test_match_detected);
        assert!(!result.coverage_regression_detected);
    }

    #[test]
    fn review80_hard_cap_stays_off_for_ten_untested_at_47_percent() {
        // 10/19 untested leaves 9/19 matched (47.4%): below 50%, but exactly
        // ten misses does not meet the strict more-than-ten hard-cap condition.
        let untested: Vec<String> = (0..10).map(|i| format!("src/mod_{i}.rs")).collect();
        let file_stats: Vec<(String, char, usize, usize)> = (0..19)
            .map(|i| (format!("src/mod_{i}.rs"), 'M', 10, 5))
            .collect();

        let result = analyze(&RegressionContext {
            untested_critical_files: untested,
            file_stats,
            ..Default::default()
        });

        assert_eq!(result.untested_code_count, 10);
        assert!(!result.low_test_match_detected);
        assert!(!result.coverage_regression_detected);
    }

    #[test]
    fn test_adaptive_threshold_coverage_delta_drop() {
        // Good code ratio but coverage_delta dropped > 5%
        let ctx = RegressionContext {
            coverage_ratio: Some(0.5),
            base_coverage_ratio: Some(0.6),
            file_stats: vec![("src/main.rs".into(), 'M', 10, 5)],
            base_metric_fingerprint: Some(COVERAGE_METRIC_FINGERPRINT.into()),
            repo_identity: Some("acme/widgets".into()),
            base_repo_identity: Some("acme/widgets".into()),
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
        assert!(!result.low_test_match_detected);
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
        assert!(!result.low_test_match_detected); // 10/25 = 60% coverage, 10 not > 10
        assert!(!result.coverage_regression_detected);
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
            base_metric_fingerprint: Some(COVERAGE_METRIC_FINGERPRINT.into()),
            repo_identity: Some("acme/widgets".into()),
            base_repo_identity: Some("acme/widgets".into()),
            ..Default::default()
        };

        let result = analyze(&ctx);
        // Coverage improved by ~10pp — the grown untested count is not a
        // regression. The current matching (10/40) is still low, and that
        // stays visible as the separate current signal.
        assert!(!result.coverage_regression_detected);
        assert!(result.coverage_improvement_detected);
        assert!(result.low_test_match_detected);
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
            base_metric_fingerprint: Some(COVERAGE_METRIC_FINGERPRINT.into()),
            repo_identity: Some("acme/widgets".into()),
            base_repo_identity: Some("acme/widgets".into()),
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
