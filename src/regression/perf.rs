//! Performance regression detection (v4, "cheap wins")
//!
//! Scans patch text for simple anti-patterns:
//! - "query in loop": explicit loops / iterator callbacks + execute/query/fetch nearby
//! - Rust: clone()/collect() in explicit loops (P2 signal)
//!
//! Non-code files (docs, scripts, configs, assets) are skipped entirely.
//! Test/e2e files are also skipped — a query-in-loop in a Playwright test
//! is not a production performance regression signal.
//!
//! Inline Rust test context (`#[cfg(test)]` / `mod tests` / `#[test]`) is
//! resolved **per hit line**, not per hunk: a production hot path that merely
//! shares a hunk with a trailing test module still counts as a production
//! signal. When the context of a hit is ambiguous it is classified as
//! production — a false positive costs a reviewer a glance, a false negative
//! hides a real regression. The context is read from the patch's **target
//! state** only (added and context lines); removed lines describe what the
//! patch replaces and never open or close a scope. A hit is paired only with a
//! nearby loop in the *same* context, so a production statement cannot borrow a
//! loop from an adjacent test module (or the reverse).

use super::RegressionContext;
use crate::rust_source::SourceScanner;
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

        // Per-added-line inline test context, aligned index-by-index with
        // `added_lines`, so each hit is classified by its own line.
        let test_context = added_line_test_context(&file, &hunk);

        // Proximity-based detection: patterns must appear within PROXIMITY_WINDOW
        // added lines of each other, not just anywhere in the same hunk.
        let hits = check_proximity(&added_lines, &test_context);

        if hits.query_prod || hits.query_test || hits.clone_prod || hits.clone_test {
            let reasons = file_reasons.entry(file.clone()).or_default();

            if hits.query_prod {
                query_in_loop += 1;
                push_reason(&mut reasons.prod_reasons, "query in loop");
            }
            if hits.query_test {
                push_reason(&mut reasons.test_reasons, "query in loop");
            }
            if hits.clone_prod {
                clone_in_loop += 1;
                push_reason(&mut reasons.prod_reasons, "clone/collect in loop");
            }
            if hits.clone_test {
                push_reason(&mut reasons.test_reasons, "clone/collect in loop");
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

/// Append `reason` unless it is already recorded.
fn push_reason(reasons: &mut Vec<String>, reason: &str) {
    if !reasons.iter().any(|r| r == reason) {
        reasons.push(reason.to_string());
    }
}

/// Proximity hits of one hunk, split by the context of the hit itself.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct ProximityHits {
    query_prod: bool,
    query_test: bool,
    clone_prod: bool,
    clone_test: bool,
}

impl ProximityHits {
    fn is_saturated(&self) -> bool {
        self.query_prod && self.query_test && self.clone_prod && self.clone_test
    }
}

/// Check if query/clone patterns appear within [`PROXIMITY_WINDOW`] added lines
/// of a loop pattern, classifying **each hit** as production or test context.
///
/// `test_context[i]` describes `added_lines[i]`. A hit counts as test context
/// only when **its own line** sits in test context; anything else — including a
/// missing or unknown classification — counts as production.
///
/// The nearby loop must share the hit's context. A production statement sitting
/// just above a trailing `#[cfg(test)]` module is not "in" the loop of a test
/// that happens to be within [`PROXIMITY_WINDOW`] lines of it — pairing across
/// the boundary invents a loop that exists in neither context. Unknown context
/// still resolves to production on both sides, so ambiguity keeps pairing.
fn check_proximity(added_lines: &[&str], test_context: &[bool]) -> ProximityHits {
    // Missing context data means "unknown" — treat it as production.
    let context_of = |i: usize| test_context.get(i).copied().unwrap_or(false);

    let loop_lines: Vec<(usize, bool)> = added_lines
        .iter()
        .enumerate()
        .filter(|(_, l)| is_loop_line(l))
        .map(|(i, _)| (i, context_of(i)))
        .collect();

    let mut hits = ProximityHits::default();
    if loop_lines.is_empty() {
        return hits;
    }

    for (i, line) in added_lines.iter().enumerate() {
        let is_query = QUERY_PATTERN.is_match(line);
        let is_clone = CLONE_COLLECT_PATTERN.is_match(line);
        if !is_query && !is_clone {
            continue;
        }

        let in_test = context_of(i);

        let near_loop = loop_lines
            .iter()
            .any(|&(l, loop_in_test)| loop_in_test == in_test && i.abs_diff(l) <= PROXIMITY_WINDOW);
        if !near_loop {
            continue;
        }

        if is_query {
            hits.query_prod |= !in_test;
            hits.query_test |= in_test;
        }
        if is_clone {
            hits.clone_prod |= !in_test;
            hits.clone_test |= in_test;
        }
        if hits.is_saturated() {
            break;
        }
    }

    hits
}

fn is_loop_line(line: &str) -> bool {
    EXPLICIT_LOOP_PATTERN.is_match(line) || ITERATOR_LOOP_PATTERN.is_match(line)
}

/// Returns `true` for hunk lines that are diff bookkeeping rather than source.
fn is_diff_metadata_line(line: &str) -> bool {
    line.starts_with("@@")
        || line.starts_with("diff --git")
        || line.starts_with("+++")
        || line.starts_with("--- a/")
        || line == "---"
        || line.starts_with("index ")
        || line.starts_with("similarity index ")
        || line.starts_with("rename ")
        || line.starts_with("new file mode ")
        || line.starts_with("deleted file mode ")
}

/// Map each added line of `hunk` to "is this line inside inline Rust test
/// context?", in the same order as the added-line vector used for detection.
///
/// A test-context marker (`#[cfg(test)]`, `mod tests`, `#[test]`, `#[rstest]`)
/// opens the context; it closes again once the braces opened after that marker
/// balance out. Lines *before* the marker stay production — that is the whole
/// point of per-hit classification: a hot path sharing a hunk with a trailing
/// test module is still production code.
///
/// Ambiguity resolves toward production: non-Rust files, unrecognised braces
/// and commented-out markers all leave the line classified as production.
///
/// Only the **target state** shapes the scope: added (`+`) and context (` `)
/// lines. Removed (`-`) lines describe the state being replaced and are ignored
/// wholesale — both for markers and for brace tracking. A `#[cfg(test)]` deleted
/// by the patch does not exist afterwards, and a renamed declaration whose old
/// and new lines both open a brace would otherwise leave the test scope
/// permanently open and mute every production hit below it.
fn added_line_test_context(file: &str, hunk: &str) -> Vec<bool> {
    let added_lines = hunk
        .lines()
        .filter(|l| l.starts_with('+') && !l.starts_with("+++"));

    if !file.ends_with(".rs") {
        return added_lines.map(|_| false).collect();
    }

    let mut flags = Vec::new();
    let mut in_test = false;
    let mut depth: i32 = 0;
    let mut seen_open = false;
    // Block comments and string literals span lines, so the reader is stateful
    // for this hunk. Hunks are not contiguous, and this function is called per
    // hunk, so the scanner starts clean here and is never carried past the
    // hunk boundary.
    let mut scanner = SourceScanner::default();

    for line in hunk.lines() {
        if is_diff_metadata_line(line) {
            continue;
        }

        // Removed lines are not part of the state this patch produces.
        if line.starts_with('-') {
            continue;
        }

        let is_added = line.starts_with('+');
        let payload = line
            .strip_prefix('+')
            .or_else(|| line.strip_prefix(' '))
            .unwrap_or(line);

        // Comments (whole-line, doc, trailing, or a `/* … */` still open from
        // an earlier line) are not code: neither their markers nor their braces
        // may move the scope. A commented-out line reduces to an empty slice
        // here, which is inert on both counts.
        let code = scanner.code_only(payload);
        let trimmed = code.trim();

        // Only the outermost marker opens the context, so nested `#[test]`
        // attributes do not reset the enclosing `mod tests` brace tracking.
        if !in_test && INLINE_RUST_TEST_CONTEXT_PATTERN.is_match(trimmed) {
            in_test = true;
            depth = 0;
            seen_open = false;
        }

        if is_added {
            flags.push(in_test);
        }

        if in_test {
            for ch in code.chars() {
                match ch {
                    '{' => {
                        depth += 1;
                        seen_open = true;
                    }
                    '}' => depth -= 1,
                    _ => {}
                }
            }
            if seen_open && depth <= 0 {
                in_test = false;
                depth = 0;
                seen_open = false;
            } else if !seen_open && ends_the_annotated_item(trimmed) {
                // Not every test item has a body. `#[cfg(test)] mod tests;` and
                // `#[cfg(test)] use crate::helper;` never open a brace, so the
                // "balanced again" close above could not fire and the context
                // stayed open over the rest of the hunk — every production loop
                // and query below it was recorded as test-only and vanished from
                // the signal. Such an item ends at its `;`, and so does the
                // context it opened.
                in_test = false;
                depth = 0;
            }
        }
    }

    flags
}

/// Does this line finish a body-less item that a test marker annotated?
///
/// Only meaningful while the marker's item has not opened a brace: attributes
/// stack above their item, so a line that is nothing but attributes is not it
/// yet, and a line that opens a body is handled by the brace tracker instead.
fn ends_the_annotated_item(trimmed: &str) -> bool {
    let item = item_after_attributes(trimmed);
    !item.is_empty() && item.ends_with(';')
}

/// The line with any leading `#[…]` attributes removed.
///
/// `#[cfg(test)] mod tests;` states the marker and the item it annotates on one
/// line, so the item cannot be found by looking at how the line starts.
fn item_after_attributes(trimmed: &str) -> &str {
    let mut rest = trimmed;
    while rest.starts_with("#[") {
        let mut depth = 0usize;
        let mut end = None;
        for (index, ch) in rest.char_indices() {
            match ch {
                '[' => depth += 1,
                ']' => {
                    depth -= 1;
                    if depth == 0 {
                        end = Some(index + ch.len_utf8());
                        break;
                    }
                }
                _ => {}
            }
        }
        // An attribute whose `]` is on a later line annotates nothing here.
        let Some(end) = end else { return "" };
        rest = rest[end..].trim_start();
    }
    rest
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

    // ---- per-hit (not per-hunk) test-context classification ----

    #[test]
    fn test_prod_hit_in_mixed_hunk_is_not_muted_by_trailing_test_module() {
        // Single hunk: production hot path first, test module afterwards.
        // Per-hunk classification marked the whole hunk as test context and
        // hid the production signal entirely.
        let patch = r#"diff --git a/src/auth.rs b/src/auth.rs
+++ b/src/auth.rs
@@ -10,4 +10,18 @@
+for candidate in candidates.iter() {
+    let stored = db.query("SELECT hash FROM users WHERE id = ?");
+    argon2.verify_password(password, &stored)?;
+}

 #[cfg(test)]
 mod tests {
+    #[test]
+    fn verify_roundtrip() {
+        for candidate in candidates.iter() {
+            let ids: Vec<_> = candidate.ids.iter().collect();
+        }
+    }
 }
"#;
        let ctx = RegressionContext {
            patch_text: Some(patch.to_string()),
            ..Default::default()
        };

        let result = analyze(&ctx);
        assert!(
            result.perf_regression_suspected,
            "production hot path sharing a hunk with a test module must stay a signal"
        );
        assert_eq!(result.query_in_loop_count, 1);
        assert_eq!(result.suspected_files.len(), 1);
        assert_eq!(result.suspected_files[0].file, "src/auth.rs");
        assert!(!result.suspected_files[0].test_context_only);
        assert!(result.suspected_files[0].mixed_context);
        assert_eq!(
            result.suspected_files[0].reasons,
            vec!["query in loop".to_string()]
        );
    }

    #[test]
    fn test_prod_hit_after_closed_test_module_in_same_hunk_is_prod() {
        let patch = r#"diff --git a/src/auth.rs b/src/auth.rs
+++ b/src/auth.rs
@@ -10,4 +10,20 @@
 #[cfg(test)]
 mod tests {
+    #[test]
+    fn roundtrip() {
+        for candidate in candidates.iter() {
+            let ids: Vec<_> = candidate.ids.iter().collect();
+        }
+    }
 }
+
+pub fn verify_all(candidates: &[Candidate]) {
+    for candidate in candidates.iter() {
+        let stored = db.query("SELECT hash FROM users WHERE id = ?");
+    }
+}
"#;
        let ctx = RegressionContext {
            patch_text: Some(patch.to_string()),
            ..Default::default()
        };

        let result = analyze(&ctx);
        assert!(
            result.perf_regression_suspected,
            "code after the test module closes is production again"
        );
        assert_eq!(result.query_in_loop_count, 1);
        assert!(!result.suspected_files[0].test_context_only);
        assert!(result.suspected_files[0].mixed_context);
    }

    #[test]
    fn test_commented_test_marker_does_not_mute_prod_hit() {
        let patch = r#"diff --git a/src/auth.rs b/src/auth.rs
+++ b/src/auth.rs
@@ -10,4 +10,8 @@
+// covered by #[cfg(test)] mod tests below
+for candidate in candidates.iter() {
+    let stored = db.query("SELECT hash FROM users WHERE id = ?");
+}
"#;
        let ctx = RegressionContext {
            patch_text: Some(patch.to_string()),
            ..Default::default()
        };

        let result = analyze(&ctx);
        assert!(
            result.perf_regression_suspected,
            "a comment mentioning test attributes is not test context"
        );
        assert_eq!(result.query_in_loop_count, 1);
        assert!(!result.suspected_files[0].test_context_only);
    }

    #[test]
    fn test_same_reason_in_both_contexts_counts_once_as_prod() {
        let patch = r#"diff --git a/src/auth.rs b/src/auth.rs
+++ b/src/auth.rs
@@ -10,4 +10,18 @@
+for candidate in candidates.iter() {
+    let stored = db.query("SELECT hash FROM users WHERE id = ?");
+}

 #[cfg(test)]
 mod tests {
+    #[test]
+    fn roundtrip() {
+        for candidate in candidates.iter() {
+            let stored = db.query("SELECT 1");
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
        assert_eq!(
            result.query_in_loop_count, 1,
            "the test-context hit must not inflate the production counter"
        );
        assert!(!result.suspected_files[0].test_context_only);
        assert!(
            result.suspected_files[0].mixed_context,
            "the same reason in both contexts is still a mixed-context file"
        );
        assert_eq!(
            result.suspected_files[0].reasons,
            vec!["query in loop".to_string()]
        );
    }

    #[test]
    fn test_pure_test_hunk_stays_test_context_only() {
        let patch = r#"diff --git a/src/auth.rs b/src/auth.rs
+++ b/src/auth.rs
@@ -40,3 +40,10 @@
 #[cfg(test)]
 mod tests {
+    #[test]
+    fn roundtrip() {
+        for candidate in candidates.iter() {
+            let stored = db.query("SELECT 1");
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
        assert_eq!(result.suspected_files.len(), 1);
        assert!(result.suspected_files[0].test_context_only);
        assert!(!result.suspected_files[0].mixed_context);
        assert_eq!(result.skipped_test_hits_count, 1);
    }

    #[test]
    fn test_hit_in_rust_test_file_is_skipped_by_path() {
        let patch = r#"diff --git a/src/auth_test.rs b/src/auth_test.rs
+++ b/src/auth_test.rs
@@ -1,1 +1,5 @@
+pub fn helper(candidates: &[Candidate]) {
+    for candidate in candidates.iter() {
+        let stored = db.query("SELECT 1");
+    }
+}
"#;
        let ctx = RegressionContext {
            patch_text: Some(patch.to_string()),
            ..Default::default()
        };

        let result = analyze(&ctx);
        assert!(
            !result.perf_regression_suspected,
            "a hit in a test file is not a production signal, even outside #[cfg(test)]"
        );
        assert_eq!(result.query_in_loop_count, 0);
        assert_eq!(result.skipped_test_hits_count, 1);
        assert!(result.suspected_files.is_empty());
    }

    // ---- target-state only: removed lines never shape the scope ----

    #[test]
    fn test_removed_test_marker_does_not_open_test_context() {
        // The diff DELETES `#[cfg(test)]`, promoting the module to production.
        // A marker that exists only on a removed line describes the *before*
        // state and must not classify added lines as test context.
        let patch = r#"diff --git a/src/auth.rs b/src/auth.rs
+++ b/src/auth.rs
@@ -1,6 +1,10 @@
-#[cfg(test)]
 pub mod helpers {
+    pub fn verify_all(candidates: &[Candidate]) {
+        for candidate in candidates.iter() {
+            let stored = db.query("SELECT hash FROM users WHERE id = ?");
+        }
+    }
 }
"#;
        let ctx = RegressionContext {
            patch_text: Some(patch.to_string()),
            ..Default::default()
        };

        let result = analyze(&ctx);
        assert!(
            result.perf_regression_suspected,
            "a removed #[cfg(test)] marker must not mute the added production hit"
        );
        assert_eq!(result.query_in_loop_count, 1);
        assert_eq!(result.suspected_files.len(), 1);
        assert!(!result.suspected_files[0].test_context_only);
    }

    #[test]
    fn test_removed_declaration_does_not_unbalance_test_scope() {
        // Renaming a fn inside `mod tests` emits `-fn old_name() {` and
        // `+fn new_name() {`. Counting braces on BOTH sides leaves one extra
        // opening brace, so the test scope never closes and every production
        // hit later in the hunk was muted.
        let patch = r#"diff --git a/src/auth.rs b/src/auth.rs
+++ b/src/auth.rs
@@ -1,10 +1,14 @@
 #[cfg(test)]
 mod tests {
-    fn old_name() {
+    fn new_name() {
         assert!(true);
     }
 }
+
+pub fn verify_all(candidates: &[Candidate]) {
+    for candidate in candidates.iter() {
+        let stored = db.query("SELECT hash FROM users WHERE id = ?");
+    }
+}
"#;
        let ctx = RegressionContext {
            patch_text: Some(patch.to_string()),
            ..Default::default()
        };

        let result = analyze(&ctx);
        assert!(
            result.perf_regression_suspected,
            "a renamed test fn must not leave the test scope open over prod code"
        );
        assert_eq!(result.query_in_loop_count, 1);
        assert_eq!(result.suspected_files.len(), 1);
        assert!(!result.suspected_files[0].test_context_only);
    }

    // ---- proximity pairing respects the context of the loop ----

    #[test]
    fn test_prod_hit_is_not_paired_with_a_loop_inside_a_test_module() {
        // The only loop in range lives in the trailing test module; pairing it
        // with a production statement invented a query-in-loop that does not
        // exist in either context.
        let patch = r#"diff --git a/src/auth.rs b/src/auth.rs
+++ b/src/auth.rs
@@ -1,4 +1,12 @@
+let stored = db.query("SELECT hash FROM users WHERE id = ?");
+
 #[cfg(test)]
 mod tests {
+    #[test]
+    fn roundtrip() {
+        for candidate in candidates.iter() {
+            assert!(candidate.ok);
+        }
+    }
 }
"#;
        let ctx = RegressionContext {
            patch_text: Some(patch.to_string()),
            ..Default::default()
        };

        let result = analyze(&ctx);
        assert!(
            !result.perf_regression_suspected,
            "a production statement must not borrow a loop from the test module"
        );
        assert_eq!(result.query_in_loop_count, 0);
        assert!(result.suspected_files.is_empty());
    }

    #[test]
    fn test_test_hit_is_not_paired_with_a_production_loop() {
        // Mirror direction: a `collect()` inside the test module has no test
        // loop nearby, so the production loop above must not manufacture a
        // test-context suspect either.
        let patch = r#"diff --git a/src/auth.rs b/src/auth.rs
+++ b/src/auth.rs
@@ -1,4 +1,12 @@
+for candidate in candidates.iter() {
+    verify(candidate);
+}
 #[cfg(test)]
 mod tests {
+    #[test]
+    fn roundtrip() {
+        let ids: Vec<_> = candidate.ids.iter().collect();
+    }
 }
"#;
        let ctx = RegressionContext {
            patch_text: Some(patch.to_string()),
            ..Default::default()
        };

        let result = analyze(&ctx);
        assert!(!result.perf_regression_suspected);
        assert_eq!(result.clone_collect_in_loop_count, 0);
        assert!(
            result.suspected_files.is_empty(),
            "a test-context hit must not borrow a production loop, got: {:?}",
            result.suspected_files
        );
        assert_eq!(result.skipped_test_hits_count, 0);
    }

    // ---- trailing `//` comments are not code ----

    #[test]
    fn test_inline_trailing_comment_marker_does_not_open_test_context() {
        // Only FULL-LINE `//` comments were treated as non-code, and the marker
        // pattern is unanchored — so a marker mentioned at the end of a real
        // statement opened test context and muted the production hit below it.
        let patch = r#"diff --git a/src/auth.rs b/src/auth.rs
+++ b/src/auth.rs
@@ -1,4 +1,8 @@
+let verify_all = true; // mirrored by #[cfg(test)] mod tests below
+for candidate in candidates.iter() {
+    let stored = db.query("SELECT hash FROM users WHERE id = ?");
+}
"#;
        let ctx = RegressionContext {
            patch_text: Some(patch.to_string()),
            ..Default::default()
        };

        let result = analyze(&ctx);
        assert!(
            result.perf_regression_suspected,
            "a marker inside a trailing comment is not test context"
        );
        assert_eq!(result.query_in_loop_count, 1);
        assert!(!result.suspected_files[0].test_context_only);
    }

    #[test]
    fn test_braces_in_trailing_comment_do_not_close_test_context() {
        // The brace tracker counted braces inside a trailing comment, so a
        // comment mentioning `}}` closed the test scope early and reported a
        // genuine test-only hit as production.
        let patch = r#"diff --git a/src/auth.rs b/src/auth.rs
+++ b/src/auth.rs
@@ -1,4 +1,10 @@
 #[cfg(test)]
 mod tests {
+    let n = 1; // not real braces: }}
+    for candidate in candidates.iter() {
+        let stored = db.query("SELECT 1");
+    }
 }
"#;
        let ctx = RegressionContext {
            patch_text: Some(patch.to_string()),
            ..Default::default()
        };

        let result = analyze(&ctx);
        assert!(
            !result.perf_regression_suspected,
            "braces inside a comment must not leak a test hit into production"
        );
        assert_eq!(result.query_in_loop_count, 0);
        assert_eq!(result.suspected_files.len(), 1);
        assert!(result.suspected_files[0].test_context_only);
    }

    #[test]
    fn test_cfg_gated_external_module_closes_its_test_context() {
        // `#[cfg(test)] mod tests;` annotates an item with no body, so no brace
        // ever opened and the "balanced again" close could not fire. The test
        // context stayed open for the rest of the hunk and recorded the
        // production query-in-loop below it as test-only, dropping it from the
        // performance signal entirely.
        let patch = r#"diff --git a/src/auth.rs b/src/auth.rs
+++ b/src/auth.rs
@@ -1,4 +1,10 @@
 #[cfg(test)]
 mod tests;
+for candidate in candidates.iter() {
+    let stored = db.query("SELECT 1");
+}
"#;
        let ctx = RegressionContext {
            patch_text: Some(patch.to_string()),
            ..Default::default()
        };

        let result = analyze(&ctx);
        assert!(
            result.perf_regression_suspected,
            "a body-less test item must not mute the production code below it"
        );
        assert_eq!(result.query_in_loop_count, 1);
        assert!(!result.suspected_files[0].test_context_only);
    }

    #[test]
    fn test_cfg_gated_import_closes_its_test_context() {
        // The same shape with a `use`: the annotated item ends at its `;`, so
        // the context it opened ends there too.
        let patch = r#"diff --git a/src/auth.rs b/src/auth.rs
+++ b/src/auth.rs
@@ -1,4 +1,10 @@
 #[cfg(test)]
 use crate::helper;
+for candidate in candidates.iter() {
+    let stored = db.query("SELECT 1");
+}
"#;
        let ctx = RegressionContext {
            patch_text: Some(patch.to_string()),
            ..Default::default()
        };

        let result = analyze(&ctx);
        assert!(
            result.perf_regression_suspected,
            "a cfg-gated import must not mute the production code below it"
        );
        assert_eq!(result.query_in_loop_count, 1);
        assert!(!result.suspected_files[0].test_context_only);
    }

    #[test]
    fn test_cfg_gated_module_with_a_body_still_holds_its_context() {
        // The other direction: an item that DOES open a body keeps the context
        // until its brace balances, exactly as before. Closing at the first
        // `;` inside it would report genuine test-only work as production.
        let patch = r#"diff --git a/src/auth.rs b/src/auth.rs
+++ b/src/auth.rs
@@ -1,4 +1,10 @@
 #[cfg(test)]
 mod tests {
+    let n = 1;
+    for candidate in candidates.iter() {
+        let stored = db.query("SELECT 1");
+    }
 }
"#;
        let ctx = RegressionContext {
            patch_text: Some(patch.to_string()),
            ..Default::default()
        };

        let result = analyze(&ctx);
        assert!(
            !result.perf_regression_suspected,
            "a statement inside a test module must not close its context"
        );
        assert_eq!(result.query_in_loop_count, 0);
        assert!(result.suspected_files[0].test_context_only);
    }

    #[test]
    fn test_braces_in_string_literals_do_not_move_test_scope() {
        // A brace typed inside a literal is data, not syntax. Counting it closed
        // the test scope early and reported the test-only query-in-loop below it
        // as a production perf suspect.
        let patch = r##"diff --git a/src/auth.rs b/src/auth.rs
+++ b/src/auth.rs
@@ -1,4 +1,12 @@
 #[cfg(test)]
 mod tests {
+    const CLOSE: &str = "}";
+    const RAW: &str = r#"}"#;
+    const CH: char = '}';
+    for candidate in candidates.iter() {
+        let stored = db.query("SELECT 1");
+    }
 }
"##;
        let ctx = RegressionContext {
            patch_text: Some(patch.to_string()),
            ..Default::default()
        };

        let result = analyze(&ctx);
        assert!(
            !result.perf_regression_suspected,
            "a brace inside a literal must not leak a test hit into production"
        );
        assert_eq!(result.query_in_loop_count, 0);
        assert_eq!(result.suspected_files.len(), 1);
        assert!(result.suspected_files[0].test_context_only);
    }

    #[test]
    fn test_open_brace_in_string_literal_does_not_hold_test_scope_open() {
        // The mirror failure: an unmatched `{` inside a literal kept the scope
        // open past the end of the test module and muted the production hit
        // that followed it.
        let patch = r#"diff --git a/src/auth.rs b/src/auth.rs
+++ b/src/auth.rs
@@ -1,4 +1,12 @@
 #[cfg(test)]
 mod tests {
+    const OPEN: &str = "{";
+}
+for candidate in candidates.iter() {
+    let stored = db.query("SELECT 1");
+}
"#;
        let ctx = RegressionContext {
            patch_text: Some(patch.to_string()),
            ..Default::default()
        };

        let result = analyze(&ctx);
        assert!(
            result.perf_regression_suspected,
            "a production query-in-loop after the test module must stay reported"
        );
        assert_eq!(result.query_in_loop_count, 1);
    }

    #[test]
    fn test_braces_in_block_comments_do_not_move_test_scope() {
        // Commenting out a block of code is what block comments are FOR, so a
        // `}` inside one is ordinary. Counting it closed the test scope early
        // and reported the test-only query-in-loop below it as production.
        let patch = r#"diff --git a/src/auth.rs b/src/auth.rs
+++ b/src/auth.rs
@@ -1,4 +1,12 @@
 #[cfg(test)]
 mod tests {
+    /* removed the tail of the old case:
+    }
+    */
+    for candidate in candidates.iter() {
+        let stored = db.query("SELECT 1");
+    }
 }
"#;
        let ctx = RegressionContext {
            patch_text: Some(patch.to_string()),
            ..Default::default()
        };

        let result = analyze(&ctx);
        assert!(
            !result.perf_regression_suspected,
            "a brace inside a block comment must not leak a test hit into production"
        );
        assert_eq!(result.query_in_loop_count, 0);
    }

    #[test]
    fn test_open_brace_in_block_comment_does_not_hold_test_scope_open() {
        // The mirror failure: a `{` inside a block comment kept the test scope
        // open past the end of the module and muted the production hit below.
        let patch = r#"diff --git a/src/auth.rs b/src/auth.rs
+++ b/src/auth.rs
@@ -1,4 +1,12 @@
 #[cfg(test)]
 mod tests {
+    /* old shape:
+    fn f() {
+    */
+}
+for candidate in candidates.iter() {
+    let stored = db.query("SELECT 1");
+}
"#;
        let ctx = RegressionContext {
            patch_text: Some(patch.to_string()),
            ..Default::default()
        };

        let result = analyze(&ctx);
        assert!(
            result.perf_regression_suspected,
            "a production query-in-loop after the test module must stay reported"
        );
        assert_eq!(result.query_in_loop_count, 1);
    }

    #[test]
    fn test_glob_pattern_is_not_a_block_comment() {
        // `format!("{}/*.{}")` carries `/*` inside a string literal. Reading it
        // as a comment opener would swallow the rest of the hunk and mute every
        // production hit after it — a worse failure than the one block-comment
        // tracking fixes, and a far more common line in real diffs.
        let patch = r#"diff --git a/src/auth.rs b/src/auth.rs
+++ b/src/auth.rs
@@ -1,4 +1,12 @@
+let pattern = format!("{}/*.{}", dir, ext);
+for candidate in candidates.iter() {
+    let stored = db.query("SELECT 1");
+}
"#;
        let ctx = RegressionContext {
            patch_text: Some(patch.to_string()),
            ..Default::default()
        };

        let result = analyze(&ctx);
        assert!(
            result.perf_regression_suspected,
            "a glob pattern must not open a block comment over the rest of the hunk"
        );
        assert_eq!(result.query_in_loop_count, 1);
    }

    #[test]
    fn test_added_line_test_context_is_aligned_with_added_lines() {
        let hunk = "@@ -1,4 +1,9 @@\n+let prod = 1;\n-let removed = 2;\n #[cfg(test)]\n mod tests {\n+    let inside = 3;\n }\n+let after = 4;\n";
        assert_eq!(
            added_line_test_context("src/auth.rs", hunk),
            vec![false, true, false],
            "flags must line up with added lines only, in order"
        );
        assert_eq!(
            added_line_test_context("src/auth.ts", hunk),
            vec![false, false, false],
            "inline Rust test context does not apply to non-Rust files"
        );
    }
}
