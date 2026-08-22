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
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
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

/// Return the code part of `line`, dropping a `//` comment wherever it starts.
///
/// Comments are not code: a marker mentioned in one must not open test context,
/// and braces typed in one must not move the scope depth. Only FULL-LINE `//`
/// used to be recognised, which left every trailing comment live.
///
/// String literals are respected so a `https://` URL is not mistaken for a
/// comment — truncating there would drop whatever braces follow it and corrupt
/// the depth in the other direction. Char literals are deliberately NOT tracked:
/// a char literal cannot contain `//`, and tracking `'` would misread Rust
/// lifetimes (`&'a str`). A stray `"` inside a char literal only suppresses
/// stripping for that line, which is the pre-existing behavior.
fn strip_line_comment(line: &str) -> &str {
    let bytes = line.as_bytes();
    let mut in_string = false;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            // Skip the escaped character so `\"` does not close the string.
            b'\\' if in_string => i += 1,
            b'"' => in_string = !in_string,
            b'/' if !in_string && bytes.get(i + 1) == Some(&b'/') => return &line[..i],
            _ => {}
        }
        i += 1;
    }
    line
}

/// Return `code` with the contents of string and char literals removed.
///
/// A brace typed inside a literal is data, not syntax: `const CLOSE: &str = "}"`
/// in a test module used to close the test scope, so every later hit in that
/// module was classified as production; an unmatched `{` in a literal held the
/// scope open the other way and muted real production hits. Blanking literals
/// also stops a marker quoted in a string from opening test context at all.
///
/// Normal strings (with `\` escapes), raw strings (`r"…"`, `r#"…"#`, `br##"…"##`)
/// and char literals (including `'\u{7b}'`) are recognised. A `'` that does not
/// close as a char literal is a lifetime and is left alone.
///
/// Best-effort, deliberately: this is a per-line scanner over diff text, so a
/// literal spanning several lines (or cut in half by a hunk boundary) is not
/// tracked across lines — its tail is read as code on the following line. That
/// residue can only affect brace depth inside a literal body, which is rarer
/// than the single-line case this fixes.
fn blank_literals(code: &str) -> Cow<'_, str> {
    let bytes = code.as_bytes();
    if !bytes.iter().any(|b| matches!(b, b'"' | b'\'')) {
        return Cow::Borrowed(code);
    }

    let mut out = String::with_capacity(code.len());
    let mut i = 0;
    while i < bytes.len() {
        if let Some(end) = raw_string_end(code, i) {
            i = end;
            continue;
        }
        match bytes[i] {
            b'"' => i = normal_string_end(code, i),
            b'\'' => match char_literal_end(code, i) {
                Some(end) => i = end,
                None => {
                    out.push('\'');
                    i += 1;
                }
            },
            _ => {
                let ch = code[i..]
                    .chars()
                    .next()
                    .expect("index sits on a char boundary");
                out.push(ch);
                i += ch.len_utf8();
            }
        }
    }
    Cow::Owned(out)
}

/// End index of a raw string starting at `start`, or `None` if none starts there.
fn raw_string_end(code: &str, start: usize) -> Option<usize> {
    let bytes = code.as_bytes();
    // The prefix must be a token start, otherwise `bar"` would look like `b` + `"`.
    if start > 0 && (bytes[start - 1].is_ascii_alphanumeric() || bytes[start - 1] == b'_') {
        return None;
    }

    let mut i = start;
    if bytes.get(i) == Some(&b'b') {
        i += 1;
    }
    if bytes.get(i) != Some(&b'r') {
        return None;
    }
    i += 1;

    let hash_start = i;
    while bytes.get(i) == Some(&b'#') {
        i += 1;
    }
    let hashes = i - hash_start;
    if bytes.get(i) != Some(&b'"') {
        return None;
    }
    i += 1;

    // Closing delimiter: a quote followed by exactly as many hashes.
    while i < bytes.len() {
        if bytes[i] == b'"' && bytes[i + 1..].iter().take(hashes).all(|b| *b == b'#') {
            let close = i + 1 + hashes;
            if close <= bytes.len() {
                return Some(close);
            }
        }
        i += 1;
    }
    // Unterminated on this line: the rest of the line is literal body.
    Some(bytes.len())
}

/// End index of the normal string literal opening at `start`.
fn normal_string_end(code: &str, start: usize) -> usize {
    let bytes = code.as_bytes();
    let mut i = start + 1;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 2,
            b'"' => return i + 1,
            _ => i += 1,
        }
    }
    bytes.len()
}

/// End index of the char literal opening at `start`, or `None` for a lifetime.
fn char_literal_end(code: &str, start: usize) -> Option<usize> {
    let bytes = code.as_bytes();
    if bytes.get(start + 1) == Some(&b'\\') {
        // `'\''`, `'\\'`, `'\u{7b}'`: skip the escaped byte, then find the close.
        // Bounded so a stray backslash cannot swallow the rest of the line.
        let mut i = start + 3;
        let limit = (start + 12).min(bytes.len());
        while i < limit {
            if bytes[i] == b'\'' {
                return Some(i + 1);
            }
            i += 1;
        }
        return None;
    }

    let ch = code.get(start + 1..)?.chars().next()?;
    let close = start + 1 + ch.len_utf8();
    (bytes.get(close) == Some(&b'\'')).then_some(close + 1)
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

        // Comments (whole-line, doc, or trailing) are not code: neither their
        // markers nor their braces may move the scope. A full-line comment
        // reduces to an empty slice here, which is inert on both counts.
        let code = blank_literals(strip_line_comment(payload));
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
            }
        }
    }

    flags
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
    fn test_double_slash_inside_string_literal_is_not_a_comment() {
        // Stripping `//` blindly would truncate a URL and drop the brace that
        // follows it, corrupting the scope depth in the other direction.
        assert_eq!(
            strip_line_comment("let url = \"https://example.com\"; // note"),
            "let url = \"https://example.com\"; "
        );
        assert_eq!(strip_line_comment("let x = 1;"), "let x = 1;");
        assert_eq!(strip_line_comment("// whole line"), "");
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
    fn test_blank_literals_removes_literal_contents_only() {
        assert_eq!(blank_literals("let s = \"} {\";"), "let s = ;");
        assert_eq!(blank_literals("let c = '}';"), "let c = ;");
        assert_eq!(blank_literals("let e = '\\u{7b}';"), "let e = ;");
        assert_eq!(blank_literals("let q = \"\\\"}\";"), "let q = ;");
        assert_eq!(blank_literals("let r = r#\"}\"#;"), "let r = ;");
        assert_eq!(blank_literals("let b = br##\"}\"##;"), "let b = ;");
        // A lifetime is not a char literal and must survive untouched.
        assert_eq!(
            blank_literals("fn f<'a>(x: &'a str) {"),
            "fn f<'a>(x: &'a str) {"
        );
        assert_eq!(blank_literals("if depth > 0 {"), "if depth > 0 {");
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
