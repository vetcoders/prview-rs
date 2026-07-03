//! Performance regression detection (v4, "cheap wins")
//!
//! Scans patch text for simple anti-patterns:
//! - "query in loop": explicit loops / iterator callbacks + execute/query/fetch nearby
//! - Rust: clone()/collect() in explicit loops (P2 signal)
//!
//! Non-code files (docs, scripts, configs, assets) are skipped entirely.
//! Test/e2e files are also skipped — a query-in-loop in a Playwright test
//! is not a production performance regression signal.

use super::RegressionContext;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

/// Maximum suspected files to report.
const MAX_SUSPECTED_FILES: usize = 20;

/// Non-code file extensions to ignore.
const NON_CODE_EXTENSIONS: &[&str] = &[
    ".md", ".html", ".yml", ".yaml", ".txt", ".json", ".toml", ".css", ".scss", ".svg", ".png",
    ".jpg", ".lock",
];

/// Non-code directory prefixes to ignore.
const NON_CODE_PREFIXES: &[&str] = &[
    "docs/",
    "doc/",
    "scripts/",
    "script/",
    "devtools/",
    "storybook/",
    ".storybook/",
    "stories/",
];

/// Lines within this distance (in added-lines) count as "near a loop."
const PROXIMITY_WINDOW: usize = 8;

static EXPLICIT_LOOP_PATTERN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\b(for|while)\b").unwrap());

static ITERATOR_LOOP_PATTERN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\.for_each\s*\(\s*(move\s+)?\|").unwrap());

static QUERY_PATTERN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(execute|query|fetch|fetch_one|fetch_all|fetch_optional|find_by|find_all)\b")
        .unwrap()
});

static CLONE_COLLECT_PATTERN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\.(clone|collect)\s*\(\s*\)").unwrap());

static INLINE_RUST_TEST_CONTEXT_PATTERN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"#\[\s*(?:cfg\s*\([^]]*\btest\b[^]]*\)|(?:[\w:]+::)*test|rstest)\s*\]|\bmod\s+tests\b",
    )
    .unwrap()
});

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PerfRegression {
    pub perf_regression_suspected: bool,
    pub suspected_files: Vec<PerfSuspect>,
    pub query_in_loop_count: usize,
    pub clone_collect_in_loop_count: usize,
    #[serde(default)]
    pub ignored_non_code_hits_count: usize,
    #[serde(default)]
    pub skipped_test_hits_count: usize,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PerfSuspect {
    pub file: String,
    #[serde(default)]
    pub reasons: Vec<String>,
    #[serde(default, alias = "is_test")]
    pub test_context_only: bool,
    #[serde(default)]
    pub mixed_context: bool,
}

/// Returns `true` if the path is a non-code file that should be skipped.
pub fn is_non_code_path(path: &str) -> bool {
    // Directory markers (boundary-aware: at start or after `/`)
    for prefix in NON_CODE_PREFIXES {
        if path.starts_with(prefix) || path.contains(&format!("/{prefix}")) {
            return true;
        }
    }

    // .env* files and extensionless non-code files
    let basename = path.rsplit('/').next().unwrap_or(path);
    if basename.starts_with(".env")
        || basename.starts_with(".git")
        || matches!(
            basename,
            "Makefile" | "Dockerfile" | "Rakefile" | "Justfile" | "LICENSE" | "CHANGELOG"
        )
    {
        return true;
    }

    // File extensions
    for ext in NON_CODE_EXTENSIONS {
        if path.ends_with(ext) {
            return true;
        }
    }

    false
}

/// Returns `true` if the path is a test/e2e file that should be skipped.
///
/// Delegates to the single canonical test-file taxonomy in `regression::tests`
/// so perf regression classification cannot drift from the rest of the codebase.
pub fn is_test_path(path: &str) -> bool {
    super::tests::is_test_file(path)
}

pub fn analyze(ctx: &RegressionContext) -> PerfRegression {
    let Some(ref patch) = ctx.patch_text else {
        return PerfRegression::default();
    };

    #[derive(Default)]
    struct FilePerfSignals {
        prod_reasons: Vec<String>,
        test_reasons: Vec<String>,
    }

    let mut file_reasons: HashMap<String, FilePerfSignals> = HashMap::new();
    let mut query_in_loop = 0usize;
    let mut clone_in_loop = 0usize;
    let mut ignored_non_code_files: HashSet<String> = HashSet::new();
    let mut skipped_test_files: HashSet<String> = HashSet::new();

    // Parse unified diff: track current file and hunks
    let mut current_file: Option<String> = None;

    for hunk in split_hunks(patch) {
        // Detect file from diff header
        if let Some(file) = extract_file_from_hunk(&hunk) {
            current_file = Some(file);
        }

        let file = match current_file {
            Some(ref f) => f.clone(),
            None => continue,
        };

        // Skip non-code files entirely (count unique files, not hunks)
        if is_non_code_path(&file) {
            ignored_non_code_files.insert(file);
            continue;
        }

        // Skip test/e2e files (not a production perf signal)
        if is_test_path(&file) {
            skipped_test_files.insert(file);
            continue;
        }

        // Only look at added lines (starting with +)
        let added_lines: Vec<&str> = hunk
            .lines()
            .filter(|l| l.starts_with('+') && !l.starts_with("+++"))
            .map(|l| &l[1..]) // strip leading '+'
            .collect();

        // Proximity-based detection: patterns must appear within PROXIMITY_WINDOW
        // added lines of each other, not just anywhere in the same hunk.
        let (has_query_near_loop, has_clone_near_loop) = check_proximity(&added_lines);
        let is_inline_test_context = is_inline_test_context(&file, &hunk);

        if has_query_near_loop {
            let reasons = file_reasons.entry(file.clone()).or_default();
            if is_inline_test_context {
                if !reasons
                    .test_reasons
                    .iter()
                    .any(|r| r.contains("query in loop"))
                {
                    reasons.test_reasons.push("query in loop".to_string());
                }
            } else {
                query_in_loop += 1;
                if !reasons
                    .prod_reasons
                    .iter()
                    .any(|r| r.contains("query in loop"))
                {
                    reasons.prod_reasons.push("query in loop".to_string());
                }
            }
        }

        if has_clone_near_loop {
            let reasons = file_reasons.entry(file.clone()).or_default();
            if is_inline_test_context {
                if !reasons
                    .test_reasons
                    .iter()
                    .any(|r| r.contains("clone/collect"))
                {
                    reasons
                        .test_reasons
                        .push("clone/collect in loop".to_string());
                }
            } else {
                clone_in_loop += 1;
                if !reasons
                    .prod_reasons
                    .iter()
                    .any(|r| r.contains("clone/collect"))
                {
                    reasons
                        .prod_reasons
                        .push("clone/collect in loop".to_string());
                }
            }
        }
    }

    let mut inline_test_only_hits = 0usize;

    // Build deduped suspects from the HashMap
    let mut suspects: Vec<PerfSuspect> = file_reasons
        .into_iter()
        .filter_map(|(file, reasons)| {
            let test_context_only =
                reasons.prod_reasons.is_empty() && !reasons.test_reasons.is_empty();
            let mixed_context =
                !reasons.prod_reasons.is_empty() && !reasons.test_reasons.is_empty();
            let signal_reasons = if reasons.prod_reasons.is_empty() {
                reasons.test_reasons
            } else {
                reasons.prod_reasons
            };

            if signal_reasons.is_empty() {
                return None;
            }

            if test_context_only {
                inline_test_only_hits += 1;
            }

            Some(PerfSuspect {
                file,
                reasons: signal_reasons,
                test_context_only,
                mixed_context,
            })
        })
        .collect();

    // Sort for deterministic output
    suspects.sort_by(|a, b| a.file.cmp(&b.file));
    suspects.truncate(MAX_SUSPECTED_FILES);

    let detected = suspects.iter().any(|suspect| !suspect.test_context_only);

    PerfRegression {
        perf_regression_suspected: detected,
        suspected_files: suspects,
        query_in_loop_count: query_in_loop,
        clone_collect_in_loop_count: clone_in_loop,
        ignored_non_code_hits_count: ignored_non_code_files.len(),
        skipped_test_hits_count: skipped_test_files.len() + inline_test_only_hits,
    }
}

/// Check if query/clone patterns appear within [`PROXIMITY_WINDOW`] added lines
/// of a loop pattern. Returns `(query_near_loop, clone_near_loop)`.
fn check_proximity(added_lines: &[&str]) -> (bool, bool) {
    let loop_lines: Vec<usize> = added_lines
        .iter()
        .enumerate()
        .filter(|(_, l)| is_loop_line(l))
        .map(|(i, _)| i)
        .collect();

    if loop_lines.is_empty() {
        return (false, false);
    }

    let mut query_near_loop = false;
    let mut clone_near_loop = false;

    for (i, line) in added_lines.iter().enumerate() {
        let near_loop = loop_lines
            .iter()
            .any(|&l| i.abs_diff(l) <= PROXIMITY_WINDOW);
        if !near_loop {
            continue;
        }
        if !query_near_loop && QUERY_PATTERN.is_match(line) {
            query_near_loop = true;
        }
        if !clone_near_loop && CLONE_COLLECT_PATTERN.is_match(line) {
            clone_near_loop = true;
        }
        if query_near_loop && clone_near_loop {
            break;
        }
    }

    (query_near_loop, clone_near_loop)
}

fn is_loop_line(line: &str) -> bool {
    EXPLICIT_LOOP_PATTERN.is_match(line) || ITERATOR_LOOP_PATTERN.is_match(line)
}

fn is_inline_test_context(file: &str, hunk: &str) -> bool {
    if !file.ends_with(".rs") {
        return false;
    }

    hunk.lines().any(|line| {
        let trimmed = line
            .strip_prefix('+')
            .or_else(|| line.strip_prefix('-'))
            .or_else(|| line.strip_prefix(' '))
            .unwrap_or(line)
            .trim();

        INLINE_RUST_TEST_CONTEXT_PATTERN.is_match(trimmed)
    })
}

/// Split patch text into hunks (each starting with @@ or diff --git).
fn split_hunks(patch: &str) -> Vec<String> {
    let mut hunks = Vec::new();
    let mut current = String::new();

    for line in patch.lines() {
        if (line.starts_with("@@ ") || line.starts_with("diff --git")) && !current.is_empty() {
            hunks.push(std::mem::take(&mut current));
        }
        current.push_str(line);
        current.push('\n');
    }
    if !current.is_empty() {
        hunks.push(current);
    }
    hunks
}

/// Extract filename from a diff --git or +++ header.
fn extract_file_from_hunk(hunk: &str) -> Option<String> {
    for line in hunk.lines() {
        if let Some(rest) = line.strip_prefix("+++ b/") {
            return Some(rest.to_string());
        }
        if let Some(rest) = line.strip_prefix("diff --git a/")
            && let Some(path) = rest.split(" b/").nth(1)
        {
            return Some(path.to_string());
        }
    }
    None
}

#[cfg(test)]
mod perf_tests {
    use super::*;

    #[test]
    fn test_is_non_code_path_docs() {
        assert!(is_non_code_path("docs/README.md"));
        assert!(is_non_code_path("doc/api.md"));
        assert!(is_non_code_path("docs/guide/setup.html"));
    }

    #[test]
    fn test_is_non_code_path_scripts() {
        assert!(is_non_code_path("scripts/deploy.sh"));
        assert!(is_non_code_path("script/migrate.py"));
    }

    #[test]
    fn test_is_non_code_path_env() {
        assert!(is_non_code_path(".env"));
        assert!(is_non_code_path(".env.local"));
        assert!(is_non_code_path(".env.production"));
    }

    #[test]
    fn test_is_non_code_path_extensions() {
        assert!(is_non_code_path("README.md"));
        assert!(is_non_code_path("config.yml"));
        assert!(is_non_code_path("data.json"));
        assert!(is_non_code_path("style.css"));
        assert!(is_non_code_path("icon.svg"));
        assert!(is_non_code_path("logo.png"));
        assert!(is_non_code_path("photo.jpg"));
        assert!(is_non_code_path("settings.toml"));
        assert!(is_non_code_path("notes.txt"));
        assert!(is_non_code_path("page.html"));
        assert!(is_non_code_path("theme.scss"));
        assert!(is_non_code_path("config.yaml"));
    }

    #[test]
    fn test_is_non_code_path_code_files() {
        assert!(!is_non_code_path("src/main.rs"));
        assert!(!is_non_code_path("src/lib.rs"));
        assert!(!is_non_code_path("handler.ts"));
        assert!(!is_non_code_path("app.py"));
        // Makefile/Dockerfile are non-code for perf scanning purposes
        assert!(is_non_code_path("Makefile"));
        assert!(is_non_code_path("Dockerfile"));
    }

    #[test]
    fn test_non_code_files_ignored() {
        let patch = r#"diff --git a/docs/perf.md b/docs/perf.md
+++ b/docs/perf.md
@@ -1,3 +1,5 @@
+for item in items {
+    db.execute(query);
+}
diff --git a/src/handler.rs b/src/handler.rs
+++ b/src/handler.rs
@@ -10,3 +10,6 @@
+for user in users {
+    let result = db.query("SELECT * FROM orders");
+}
"#;
        let ctx = RegressionContext {
            patch_text: Some(patch.to_string()),
            ..Default::default()
        };

        let result = analyze(&ctx);
        assert_eq!(result.query_in_loop_count, 1, "only code file should count");
        assert_eq!(result.ignored_non_code_hits_count, 1);
        assert_eq!(result.suspected_files.len(), 1);
        assert_eq!(result.suspected_files[0].file, "src/handler.rs");
    }

    #[test]
    fn test_dedupe_reasons_same_file() {
        // A file that triggers both query-in-loop AND clone/collect-in-loop
        let patch = r#"diff --git a/src/process.rs b/src/process.rs
+++ b/src/process.rs
@@ -5,3 +5,7 @@
+for item in items.iter() {
+    let r = db.query("SELECT 1");
+    let cloned = item.clone();
+    let v: Vec<_> = data.iter().collect();
+}
"#;
        let ctx = RegressionContext {
            patch_text: Some(patch.to_string()),
            ..Default::default()
        };

        let result = analyze(&ctx);
        assert_eq!(
            result.suspected_files.len(),
            1,
            "same file should be deduped"
        );
        assert_eq!(result.suspected_files[0].file, "src/process.rs");
        assert_eq!(result.suspected_files[0].reasons.len(), 2);
        assert!(
            result.suspected_files[0]
                .reasons
                .contains(&"query in loop".to_string())
        );
        assert!(
            result.suspected_files[0]
                .reasons
                .contains(&"clone/collect in loop".to_string())
        );
    }

    #[test]
    fn test_env_file_ignored() {
        let patch = r#"diff --git a/.env.local b/.env.local
+++ b/.env.local
@@ -1,1 +1,3 @@
+for x in items.iter() {
+    db.execute(q);
+}
"#;
        let ctx = RegressionContext {
            patch_text: Some(patch.to_string()),
            ..Default::default()
        };

        let result = analyze(&ctx);
        assert!(!result.perf_regression_suspected);
        assert_eq!(result.ignored_non_code_hits_count, 1);
        assert!(result.suspected_files.is_empty());
    }

    #[test]
    fn test_script_dir_ignored() {
        let patch = r#"diff --git a/scripts/bench.py b/scripts/bench.py
+++ b/scripts/bench.py
@@ -1,1 +1,3 @@
+for x in items:
+    db.execute(query)
"#;
        let ctx = RegressionContext {
            patch_text: Some(patch.to_string()),
            ..Default::default()
        };

        let result = analyze(&ctx);
        assert!(!result.perf_regression_suspected);
        assert_eq!(result.ignored_non_code_hits_count, 1);
    }

    #[test]
    fn test_empty_patch() {
        let ctx = RegressionContext {
            patch_text: None,
            ..Default::default()
        };
        let result = analyze(&ctx);
        assert!(!result.perf_regression_suspected);
        assert_eq!(result.ignored_non_code_hits_count, 0);
    }

    // ---- is_test_path tests ----

    #[test]
    fn test_is_test_path_e2e_fixture() {
        assert!(is_test_path("e2e/fixtures/ensureRailData.ts"));
    }

    #[test]
    fn test_is_test_path_jest_test_file() {
        assert!(is_test_path(
            "src/hooks/__tests__/useTranscriptDock.test.ts"
        ));
    }

    #[test]
    fn test_is_test_path_regular_code_not_test() {
        assert!(!is_test_path("src/hooks/useTranscriptDock.ts"));
    }

    #[test]
    fn test_is_test_path_various() {
        // Directory patterns
        assert!(is_test_path("tests/unit/handler.rs"));
        assert!(is_test_path("test/helpers.ts"));
        assert!(is_test_path("src/__tests__/App.test.tsx"));
        assert!(is_test_path("e2e/login.spec.ts"));
        assert!(is_test_path("spec/models/user_spec.rb"));

        // Basename patterns
        assert!(is_test_path("src/handler_test.go"));
        assert!(is_test_path("src/handler.test.ts"));
        assert!(is_test_path("src/handler_spec.rb"));
        assert!(is_test_path("src/handler.spec.ts"));
        assert!(is_test_path("test_handler.py"));
        assert!(is_test_path("lib/test_utils.py"));

        // Stem exactly test/tests
        assert!(is_test_path("test.rs"));
        assert!(is_test_path("tests.py"));

        // NOT test paths
        assert!(!is_test_path("src/main.rs"));
        assert!(!is_test_path("src/testing/utils.ts"));
        assert!(!is_test_path("src/contest.rs"));
        assert!(!is_test_path("src/attest.py"));
    }

    #[test]
    fn test_test_files_skipped_in_analyze() {
        let patch = r#"diff --git a/e2e/fixtures/ensureRailData.ts b/e2e/fixtures/ensureRailData.ts
+++ b/e2e/fixtures/ensureRailData.ts
@@ -1,1 +1,4 @@
+for (const item of items) {
+    await db.execute(query);
+}
diff --git a/src/hooks/__tests__/useTranscriptDock.test.ts b/src/hooks/__tests__/useTranscriptDock.test.ts
+++ b/src/hooks/__tests__/useTranscriptDock.test.ts
@@ -1,1 +1,4 @@
+for (const x of list) {
+    const r = await api.fetch(url);
+}
diff --git a/tests/integration/db_test.rs b/tests/integration/db_test.rs
+++ b/tests/integration/db_test.rs
@@ -1,1 +1,4 @@
+for item in items.iter() {
+    db.query("SELECT 1");
+}
"#;
        let ctx = RegressionContext {
            patch_text: Some(patch.to_string()),
            ..Default::default()
        };

        let result = analyze(&ctx);
        assert!(
            !result.perf_regression_suspected,
            "all hits are test files, should not flag"
        );
        assert_eq!(result.query_in_loop_count, 0);
        assert_eq!(result.clone_collect_in_loop_count, 0);
        assert_eq!(result.skipped_test_hits_count, 3);
        assert!(result.suspected_files.is_empty());
    }

    #[test]
    fn test_mixed_test_and_code_files() {
        let patch = r#"diff --git a/src/handler.rs b/src/handler.rs
+++ b/src/handler.rs
@@ -10,3 +10,6 @@
+for user in users {
+    let result = db.query("SELECT * FROM orders");
+}
diff --git a/tests/handler_test.rs b/tests/handler_test.rs
+++ b/tests/handler_test.rs
@@ -1,1 +1,4 @@
+for user in users {
+    let result = db.query("SELECT * FROM orders");
+}
"#;
        let ctx = RegressionContext {
            patch_text: Some(patch.to_string()),
            ..Default::default()
        };

        let result = analyze(&ctx);
        assert!(result.perf_regression_suspected);
        assert_eq!(
            result.query_in_loop_count, 1,
            "only the code file should count"
        );
        assert_eq!(result.skipped_test_hits_count, 1);
        assert_eq!(result.suspected_files.len(), 1);
        assert_eq!(result.suspected_files[0].file, "src/handler.rs");
    }

    #[test]
    fn test_iterator_map_chain_does_not_flag_clone_collect_in_loop() {
        let patch = r#"diff --git a/src/search.rs b/src/search.rs
+++ b/src/search.rs
@@ -10,3 +10,4 @@
+let names: Vec<_> = users.iter().map(|user| user.name.clone()).collect();
"#;
        let ctx = RegressionContext {
            patch_text: Some(patch.to_string()),
            ..Default::default()
        };

        let result = analyze(&ctx);
        assert!(
            !result.perf_regression_suspected,
            "plain iterator chains should not be reported as loop regressions"
        );
        assert_eq!(result.clone_collect_in_loop_count, 0);
        assert_eq!(result.query_in_loop_count, 0);
        assert!(result.suspected_files.is_empty());
    }

    #[test]
    fn test_inline_rust_test_context_is_marked_without_perf_regression() {
        let patch = r#"diff --git a/src/portal.rs b/src/portal.rs
+++ b/src/portal.rs
@@ -20,3 +20,10 @@
 #[cfg(test)]
 mod tests {
+    #[test]
+    fn portal_roundtrip() {
+        for user in users.iter() {
+            let ids: Vec<_> = values.iter().collect();
+            db.query("SELECT 1");
+        }
+    }
 }
"#;
        let ctx = RegressionContext {
            patch_text: Some(patch.to_string()),
            ..Default::default()
        };

        let result = analyze(&ctx);
        assert!(!result.perf_regression_suspected);
        assert_eq!(result.query_in_loop_count, 0);
        assert_eq!(result.clone_collect_in_loop_count, 0);
        assert_eq!(result.skipped_test_hits_count, 1);
        assert_eq!(result.suspected_files.len(), 1);
        assert!(result.suspected_files[0].test_context_only);
        assert!(!result.suspected_files[0].mixed_context);
    }

    #[test]
    fn test_inline_test_context_does_not_pollute_prod_perf_reasons() {
        let patch = r#"diff --git a/src/portal.rs b/src/portal.rs
+++ b/src/portal.rs
@@ -10,3 +10,6 @@
+for user in users.iter() {
+    db.query("SELECT 1");
+}
@@ -40,3 +43,9 @@
 #[cfg(test)]
 mod tests {
+    #[test]
+    fn portal_roundtrip() {
+        for user in users.iter() {
+            let ids: Vec<_> = values.iter().collect();
+        }
+    }
 }
"#;
        let ctx = RegressionContext {
            patch_text: Some(patch.to_string()),
            ..Default::default()
        };

        let result = analyze(&ctx);
        assert!(result.perf_regression_suspected);
        assert_eq!(result.query_in_loop_count, 1);
        assert_eq!(result.clone_collect_in_loop_count, 0);
        assert_eq!(result.suspected_files.len(), 1);
        assert!(!result.suspected_files[0].test_context_only);
        assert!(result.suspected_files[0].mixed_context);
        assert_eq!(
            result.suspected_files[0].reasons,
            vec!["query in loop".to_string()]
        );
    }
}
