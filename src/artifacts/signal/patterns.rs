//! Pattern scan — detect risky patterns in added lines.

use super::common::parse_patch_new_start;
use crate::git::{Diff, Repository};
use crate::regression::tests::is_test_file;
use anyhow::Result;
use std::collections::HashSet;
use std::fs;
use std::path::Path;

/// A pattern match found in a diff patch.
#[derive(Debug, Clone, serde::Serialize)]
pub struct PatternHit {
    pub file: String,
    pub line: usize,
    pub pattern: &'static str,
    pub context: String,
    pub test_code: bool,
    /// `println!`/`print!`/`eprintln!` hits in CLI binary entry points are
    /// intended user-facing output, not debug leftovers. Such hits are flagged
    /// here and kept out of the alarming prod count (P2-05).
    pub cli_intended: bool,
}

/// Heuristic: is `path` a CLI binary entry point where `println!`/`print!` is
/// intended user-facing output rather than a debug leftover?
///
/// Deliberately a small, conservative allowlist of common entry/output modules
/// — not a full call-graph analysis.
fn is_cli_entry_point(path: &str) -> bool {
    let norm = path.trim_start_matches("./");
    matches!(
        norm,
        "src/main.rs" | "src/cli/mod.rs" | "src/cli.rs" | "src/output/mod.rs" | "src/output.rs"
    ) || norm.ends_with("/src/main.rs")
}

/// Patterns to scan for in added lines of patches.
const SCAN_PATTERNS: &[(&str, &[&str])] = &[
    ("unwrap", &[".unwrap()"]),
    ("println", &["println!(", "print!("]),
    ("dbg", &["dbg!("]),
    ("todo", &["todo!(", "TODO", "FIXME", "HACK", "XXX"]),
    (
        "ts-ignore",
        &["@ts-ignore", "@ts-expect-error", "@ts-nocheck"],
    ),
    ("eslint-disable", &["eslint-disable", "// eslint-disable"]),
    (
        "console",
        &["console.log(", "console.error(", "console.warn("],
    ),
    ("bare-catch", &["catch {", "catch()", ".catch(() =>"]),
    ("unsafe", &["unsafe {", "unsafe fn"]),
    ("allow-lint", &["#[allow(", "#![allow("]),
    ("type-cast", &["as unknown as", "as any"]),
];

/// Build a set of 1-indexed line numbers that fall inside `#[cfg(test)]` blocks.
///
/// Used by `generate_pattern_scan` to correctly classify additions inside
/// pre-existing test modules, even when the `#[cfg(test)]` annotation is
/// outside the patch hunk.
fn build_test_line_set(content: &str) -> HashSet<usize> {
    let mut test_lines = HashSet::new();
    let mut in_test_block = false;
    let mut in_test_fn = false;
    let mut pending_cfg_test = false;
    let mut pending_test_fn = false;
    let mut brace_depth: i32 = 0;
    let mut test_block_start: i32 = 0;
    let mut test_fn_start: i32 = 0;

    for (idx, line) in content.lines().enumerate() {
        let trimmed = line.trim();

        // Detect #[cfg(test)] module blocks
        if trimmed.contains("#[cfg(test)]") {
            if trimmed.contains("mod ") && trimmed.ends_with(';') {
                // Single-line test module declaration — body is in another file
            } else {
                pending_cfg_test = true;
            }
        }

        // Detect standalone #[test] / #[tokio::test] / #[rstest] / etc.
        if !in_test_block
            && (trimmed == "#[test]"
                || trimmed.starts_with("#[test]")
                || trimmed.contains("::test]"))
        {
            pending_test_fn = true;
        }

        // Count braces with basic string/comment awareness to avoid
        // mistracking depth from braces inside literals.
        let mut in_string = false;
        let mut prev_ch = '\0';
        for ch in trimmed.chars() {
            if ch == '/' && prev_ch == '/' && !in_string {
                break; // rest of line is comment
            }
            if ch == '"' && prev_ch != '\\' {
                in_string = !in_string;
            }
            if in_string {
                prev_ch = ch;
                continue;
            }
            match ch {
                '{' => {
                    brace_depth += 1;
                    if pending_cfg_test {
                        in_test_block = true;
                        test_block_start = brace_depth;
                        pending_cfg_test = false;
                    }
                    if pending_test_fn {
                        in_test_fn = true;
                        test_fn_start = brace_depth;
                        pending_test_fn = false;
                    }
                }
                '}' => {
                    brace_depth -= 1;
                    if in_test_block && brace_depth < test_block_start {
                        in_test_block = false;
                    }
                    if in_test_fn && brace_depth < test_fn_start {
                        in_test_fn = false;
                    }
                }
                _ => {}
            }
            prev_ch = ch;
        }

        if in_test_block || in_test_fn {
            test_lines.insert(idx + 1); // 1-indexed to match hunk header line numbers
        }
    }

    test_lines
}

/// Scan diff patches for risky patterns in added lines.
/// Returns hits grouped by pattern type.
pub fn generate_pattern_scan(dir: &Path, diffs: &[Diff], repo: &Repository) -> Result<()> {
    let mut hits: Vec<PatternHit> = Vec::new();

    for diff in diffs {
        for file in &diff.files {
            // Skip non-code files (docs, changelogs, config) — they inflate pattern counts
            let path_lower = file.path.to_lowercase();
            if path_lower.ends_with(".md")
                || path_lower.ends_with(".txt")
                || path_lower.ends_with(".yml")
                || path_lower.ends_with(".yaml")
                || path_lower.ends_with(".json")
                || path_lower.ends_with(".toml")
                || path_lower.starts_with("docs/")
                || path_lower.starts_with(".github/")
            {
                continue;
            }

            let patch =
                match repo.file_diff(&diff.base_commit_id, &diff.target_commit_id, &file.path) {
                    Ok(p) => p,
                    Err(_) => continue,
                };

            // Skip binary files (git marks them with "Binary files differ")
            if patch.starts_with("Binary files") || patch.contains("\0") {
                continue;
            }

            // Build test-line set from the FULL file at the target commit.
            // This correctly classifies additions inside pre-existing #[cfg(test)]
            // modules, even when the #[cfg(test)] line itself is outside the hunk.
            let file_is_test = is_test_file(&file.path);
            let file_is_entry_point = is_cli_entry_point(&file.path);
            let test_lines = if file.path.ends_with(".rs") && !file_is_test {
                repo.file_at_commit(&diff.target_commit_id, &file.path)
                    .ok()
                    .map(|content| build_test_line_set(&content))
                    .unwrap_or_default()
            } else {
                HashSet::new()
            };
            let mut target_line: Option<usize> = None;

            for line in patch.lines() {
                if let Some(start) = parse_patch_new_start(line) {
                    target_line = Some(start.saturating_sub(1));
                    continue;
                }

                let is_added = line.starts_with('+') && !line.starts_with("+++");
                let is_removed = line.starts_with('-') && !line.starts_with("---");
                let is_context = line.starts_with(' ');

                if line.starts_with("diff ")
                    || line.starts_with("index ")
                    || line.starts_with("---")
                    || line.starts_with("+++")
                    || line.starts_with('\\')
                {
                    continue;
                }
                if is_removed {
                    continue;
                }

                if is_added || is_context {
                    target_line = Some(target_line.unwrap_or(0) + 1);
                }

                if !is_added {
                    continue;
                }

                let content = &line[1..];

                let line_no = target_line.unwrap_or(0);
                let is_test_code = file_is_test || test_lines.contains(&line_no);

                for &(pattern_name, needles) in SCAN_PATTERNS {
                    if needles.iter().any(|n| content.contains(n)) {
                        // `println`/`print` hits in a CLI entry point are
                        // intended output, not a debug leftover (P2-05).
                        let cli_intended =
                            pattern_name == "println" && file_is_entry_point && !is_test_code;
                        hits.push(PatternHit {
                            file: file.path.clone(),
                            line: line_no,
                            pattern: pattern_name,
                            context: content.trim().chars().take(120).collect(),
                            test_code: is_test_code,
                            cli_intended,
                        });
                        break; // one hit per line
                    }
                }
            }
        }
    }

    if hits.is_empty() {
        return Ok(());
    }

    // Aggregate by pattern
    let mut by_pattern: std::collections::BTreeMap<&str, Vec<&PatternHit>> =
        std::collections::BTreeMap::new();
    for hit in &hits {
        by_pattern.entry(hit.pattern).or_default().push(hit);
    }

    // `prod_hits` excludes both test code and CLI-intended output so the count
    // reflects genuine debug-leftover risk (P2-05).
    let prod_hits: usize = hits
        .iter()
        .filter(|h| !h.test_code && !h.cli_intended)
        .count();
    let test_hits: usize = hits.iter().filter(|h| h.test_code).count();
    let cli_intended_hits: usize = hits.iter().filter(|h| h.cli_intended).count();

    let pattern_entries: Vec<serde_json::Value> = by_pattern
        .iter()
        .map(|(k, v)| {
            let prod = v.iter().filter(|h| !h.test_code && !h.cli_intended).count();
            let test = v.iter().filter(|h| h.test_code).count();
            let cli_intended = v.iter().filter(|h| h.cli_intended).count();
            // All files where this pattern appears (prod + test), so `files` is
            // never empty while `count > 0` (BUG-5). `prod_count`/`test_count`
            // still carry the breakdown.
            let mut files: Vec<&String> = v.iter().map(|h| &h.file).collect();
            files.sort();
            files.dedup();
            // Prefer production samples; fall back to test hits so a test-only
            // pattern still shows examples instead of an empty list.
            let sample =
                |h: &&PatternHit| serde_json::json!({ "file": h.file, "context": h.context });
            let mut samples: Vec<serde_json::Value> = v
                .iter()
                .filter(|h| !h.test_code)
                .take(5)
                .map(sample)
                .collect();
            if samples.is_empty() {
                samples = v.iter().take(5).map(sample).collect();
            }
            serde_json::json!({
                "pattern": k,
                "count": v.len(),
                "prod_count": prod,
                "test_count": test,
                "cli_intended_count": cli_intended,
                "files": files,
                "samples": samples,
            })
        })
        .collect();

    let summary: serde_json::Value = serde_json::json!({
        "total_hits": hits.len(),
        "prod_hits": prod_hits,
        "test_hits": test_hits,
        "cli_intended_hits": cli_intended_hits,
        "by_pattern": pattern_entries,
    });

    fs::write(
        dir.join("PATTERN_SCAN.json"),
        serde_json::to_string_pretty(&summary)?,
    )?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::test_helpers::{make_diff_with_ids, make_test_repo, mock_file_change};
    use super::*;
    use crate::git::FileStatus;
    use tempfile::TempDir;

    #[test]
    fn pattern_scan_classifies_println_in_entry_point_as_cli_intended() {
        // A println! added in a CLI binary entry point is intended output, not a
        // debug leftover. It must be flagged cli_intended and kept OUT of the
        // alarming prod count.
        let (_tmp, repo, base_id, target_id) = make_test_repo(&[(
            "src/main.rs",
            "fn main() {}\n",
            "fn main() {\n    println!(\"hello\");\n}\n",
        )]);
        let out = TempDir::new().unwrap();
        let diff = make_diff_with_ids(
            base_id,
            target_id,
            vec![mock_file_change("src/main.rs", FileStatus::Modified, 1, 0)],
        );

        generate_pattern_scan(out.path(), &[diff], &repo).unwrap();

        let scan = fs::read_to_string(out.path().join("PATTERN_SCAN.json")).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&scan).unwrap();
        assert_eq!(
            parsed["prod_hits"], 0,
            "println! in CLI entry point must not inflate prod count"
        );
        assert!(
            parsed["cli_intended_hits"].as_u64().unwrap() > 0,
            "println! in entry point must be cli_intended"
        );
        let println_entry = parsed["by_pattern"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["pattern"] == "println")
            .expect("println pattern present");
        assert_eq!(println_entry["prod_count"].as_u64(), Some(0));
        assert!(println_entry["cli_intended_count"].as_u64().unwrap() > 0);
    }

    #[test]
    fn pattern_scan_println_outside_entry_point_still_prod() {
        // A println! in a normal (non-entry-point, non-test) module is still a
        // production debug-leftover hit.
        let (_tmp, repo, base_id, target_id) = make_test_repo(&[(
            "src/engine.rs",
            "fn run() {}\n",
            "fn run() {\n    println!(\"debug leftover\");\n}\n",
        )]);
        let out = TempDir::new().unwrap();
        let diff = make_diff_with_ids(
            base_id,
            target_id,
            vec![mock_file_change(
                "src/engine.rs",
                FileStatus::Modified,
                1,
                0,
            )],
        );

        generate_pattern_scan(out.path(), &[diff], &repo).unwrap();

        let scan = fs::read_to_string(out.path().join("PATTERN_SCAN.json")).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&scan).unwrap();
        assert_eq!(
            parsed["prod_hits"], 1,
            "println! outside entry point must still count as prod"
        );
        assert_eq!(parsed["cli_intended_hits"].as_u64(), Some(0));
    }

    #[test]
    fn pattern_scan_detects_unwrap() {
        let (_tmp, repo, base_id, target_id) = make_test_repo(&[(
            "src/lib.rs",
            "fn main() {}\n",
            "fn main() {\n    let x = foo.unwrap();\n}\n",
        )]);
        let out = TempDir::new().unwrap();
        let diff = make_diff_with_ids(
            base_id,
            target_id,
            vec![mock_file_change("src/lib.rs", FileStatus::Modified, 1, 0)],
        );

        generate_pattern_scan(out.path(), &[diff], &repo).unwrap();

        let scan = fs::read_to_string(out.path().join("PATTERN_SCAN.json")).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&scan).unwrap();
        assert_eq!(parsed["total_hits"], 1);
        let patterns = parsed["by_pattern"].as_array().unwrap();
        assert!(
            patterns.iter().any(|p| p["pattern"] == "unwrap"),
            "Should detect .unwrap() pattern"
        );
    }

    #[test]
    fn pattern_scan_empty_diff() {
        let (_tmp, repo, _base_id, _target_id) = make_test_repo(&[("README.md", "old\n", "new\n")]);
        let out = TempDir::new().unwrap();

        generate_pattern_scan(out.path(), &[], &repo).unwrap();

        assert!(
            !out.path().join("PATTERN_SCAN.json").exists(),
            "Should not create file for empty diffs"
        );
    }

    #[test]
    fn pattern_scan_classifies_additions_inside_preexisting_test_module() {
        // The #[cfg(test)] block exists in both base and target, but the unwrap()
        // is ADDED inside it. The hunk does NOT contain #[cfg(test)] — only the
        // added line. Full-file seeding must classify it as test code.
        let old = "\
fn prod() {}

#[cfg(test)]
mod tests {
    fn existing_test() {}
}
";
        let new = "\
fn prod() {}

#[cfg(test)]
mod tests {
    fn existing_test() {}
    fn new_test() { some.unwrap(); }
}
";
        let (_tmp, repo, base_id, target_id) = make_test_repo(&[("src/lib.rs", old, new)]);
        let out = TempDir::new().unwrap();
        let diff = make_diff_with_ids(
            base_id,
            target_id,
            vec![mock_file_change("src/lib.rs", FileStatus::Modified, 1, 0)],
        );

        generate_pattern_scan(out.path(), &[diff], &repo).unwrap();

        // File MUST exist — the unwrap() hit is test_code=true, which still
        // gets recorded (test_hits > 0). Unconditional assertion prevents
        // silent regression if the hit is dropped entirely.
        let scan = fs::read_to_string(out.path().join("PATTERN_SCAN.json"))
            .expect("PATTERN_SCAN.json must be generated for test-only hits");
        let parsed: serde_json::Value = serde_json::from_str(&scan).unwrap();
        assert_eq!(
            parsed["prod_hits"], 0,
            "unwrap() inside #[cfg(test)] must be test_code"
        );
        assert!(
            parsed["test_hits"].as_u64().unwrap() > 0,
            "test hit must be recorded"
        );
    }

    #[test]
    fn pattern_scan_lists_files_for_test_only_hits() {
        // Regression (BUG-5): a pattern whose hits are all test_code must still
        // populate `files` (previously empty while `count` was non-zero).
        let old = "fn prod() {}\n";
        let new = "fn prod() {}\n#[cfg(test)]\nmod tests {\n    fn t() { x.unwrap(); }\n}\n";
        let (_tmp, repo, base_id, target_id) = make_test_repo(&[("src/lib.rs", old, new)]);
        let out = TempDir::new().unwrap();
        let diff = make_diff_with_ids(
            base_id,
            target_id,
            vec![mock_file_change("src/lib.rs", FileStatus::Modified, 3, 0)],
        );
        generate_pattern_scan(out.path(), &[diff], &repo).unwrap();

        let scan = fs::read_to_string(out.path().join("PATTERN_SCAN.json")).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&scan).unwrap();
        let unwrap_entry = parsed["by_pattern"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["pattern"] == "unwrap")
            .expect("unwrap pattern present");
        assert_eq!(unwrap_entry["prod_count"].as_u64(), Some(0));
        assert!(unwrap_entry["test_count"].as_u64().unwrap() > 0);
        let files = unwrap_entry["files"].as_array().unwrap();
        assert!(
            !files.is_empty(),
            "files must be populated even for test-only hits"
        );
        assert!(files.iter().any(|f| f == "src/lib.rs"));
        assert!(
            !unwrap_entry["samples"].as_array().unwrap().is_empty(),
            "samples should fall back to test hits"
        );
    }

    #[test]
    fn pattern_scan_handles_cfg_test_mod_declaration() {
        // `#[cfg(test)] mod tests;` is a declaration, not a block — lines after it
        // should be classified as production code.
        let old = "fn prod() {}\n";
        let new = "\
fn prod() {}
#[cfg(test)]
mod tests;
fn after() { x.unwrap(); }
";
        let (_tmp, repo, base_id, target_id) = make_test_repo(&[("src/lib.rs", old, new)]);
        let out = TempDir::new().unwrap();
        let diff = make_diff_with_ids(
            base_id,
            target_id,
            vec![mock_file_change("src/lib.rs", FileStatus::Modified, 1, 0)],
        );

        generate_pattern_scan(out.path(), &[diff], &repo).unwrap();

        let scan = fs::read_to_string(out.path().join("PATTERN_SCAN.json")).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&scan).unwrap();
        // unwrap() after `mod tests;` is production code
        assert!(parsed["prod_hits"].as_u64().unwrap() > 0);
    }

    #[test]
    fn pattern_scan_mixed_prod_and_test_patch() {
        // A single file diff that adds unwrap() in both prod and test sections.
        let old = "fn prod() {}\n\n#[cfg(test)]\nmod tests {\n}\n";
        let new = "\
fn prod() { danger.unwrap(); }

#[cfg(test)]
mod tests {
    fn t() { safe.unwrap(); }
}
";
        let (_tmp, repo, base_id, target_id) = make_test_repo(&[("src/lib.rs", old, new)]);
        let out = TempDir::new().unwrap();
        let diff = make_diff_with_ids(
            base_id,
            target_id,
            vec![mock_file_change("src/lib.rs", FileStatus::Modified, 1, 0)],
        );

        generate_pattern_scan(out.path(), &[diff], &repo).unwrap();

        let scan = fs::read_to_string(out.path().join("PATTERN_SCAN.json")).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&scan).unwrap();
        assert_eq!(
            parsed["prod_hits"], 1,
            "Only the prod unwrap should be a prod hit"
        );
        assert_eq!(
            parsed["test_hits"], 1,
            "The test unwrap should be a test hit"
        );
    }

    #[test]
    fn pattern_scan_standalone_test_fn_classified_as_test() {
        // A standalone #[test] fn (not inside #[cfg(test)] mod) should be test code.
        let old = "fn prod() {}\n";
        let new = "\
fn prod() {}

#[test]
fn my_test() {
    danger.unwrap();
}
";
        let (_tmp, repo, base_id, target_id) = make_test_repo(&[("src/lib.rs", old, new)]);
        let out = TempDir::new().unwrap();
        let diff = make_diff_with_ids(
            base_id,
            target_id,
            vec![mock_file_change("src/lib.rs", FileStatus::Modified, 5, 0)],
        );

        generate_pattern_scan(out.path(), &[diff], &repo).unwrap();

        if out.path().join("PATTERN_SCAN.json").exists() {
            let scan = fs::read_to_string(out.path().join("PATTERN_SCAN.json")).unwrap();
            let parsed: serde_json::Value = serde_json::from_str(&scan).unwrap();
            assert_eq!(
                parsed["prod_hits"], 0,
                "unwrap() inside #[test] fn must be test_code, not prod"
            );
        }
    }

    #[test]
    fn pattern_scan_does_not_blanket_skip_array_literal_lines() {
        // A line carrying an array literal (`&["..."]`) must not be dropped from
        // the scan. The exact P0 idiom
        // `Command::new("git").args(&["rev-parse","HEAD"]).output().unwrap()` was
        // invisible to the old blanket-skip, hiding the `.unwrap()` the scan
        // exists to flag. Regression: blind != healthy.
        let old = "fn run() {}\n";
        let new = "fn run() {\n    let out = Command::new(\"git\").args(&[\"rev-parse\", \"HEAD\"]).output().unwrap();\n}\n";
        let (_tmp, repo, base_id, target_id) = make_test_repo(&[("src/lib.rs", old, new)]);
        let out = TempDir::new().unwrap();
        let diff = make_diff_with_ids(
            base_id,
            target_id,
            vec![mock_file_change("src/lib.rs", FileStatus::Modified, 2, 0)],
        );

        generate_pattern_scan(out.path(), &[diff], &repo).unwrap();

        let path = out.path().join("PATTERN_SCAN.json");
        assert!(
            path.exists(),
            "an added .unwrap() on an array-literal line must be scanned, not skipped"
        );
        let parsed: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert!(
            parsed["prod_hits"].as_u64().unwrap() >= 1,
            "unwrap on a `&[...]` line must count as a hit, got {}",
            parsed["prod_hits"]
        );
    }

    #[test]
    fn pattern_scan_skips_binary_file() {
        let old_content = "binary\x00old";
        let new_content = "binary\x00new with .unwrap()";
        let (_tmp, repo, base_id, target_id) =
            make_test_repo(&[("data.bin", old_content, new_content)]);
        let out = TempDir::new().unwrap();
        let diff = make_diff_with_ids(
            base_id,
            target_id,
            vec![mock_file_change("data.bin", FileStatus::Modified, 1, 1)],
        );

        generate_pattern_scan(out.path(), &[diff], &repo).unwrap();

        assert!(
            !out.path().join("PATTERN_SCAN.json").exists(),
            "Should not create file for binary-only diffs"
        );
    }

    #[test]
    fn pattern_scan_ignores_context_lines() {
        // unwrap() exists in both old and new, so it appears as context (no '+' prefix)
        // Only the added line (no pattern) changes
        let old = "fn main() {\n    let x = foo.unwrap();\n}\n";
        let new = "fn main() {\n    let x = foo.unwrap();\n    let y = 1;\n}\n";
        let (_tmp, repo, base_id, target_id) = make_test_repo(&[("src/lib.rs", old, new)]);
        let out = TempDir::new().unwrap();
        let diff = make_diff_with_ids(
            base_id,
            target_id,
            vec![mock_file_change("src/lib.rs", FileStatus::Modified, 1, 0)],
        );

        generate_pattern_scan(out.path(), &[diff], &repo).unwrap();

        assert!(
            !out.path().join("PATTERN_SCAN.json").exists(),
            "Should not detect patterns in context-only lines"
        );
    }

    #[test]
    fn pattern_scan_marks_added_lines_inside_existing_cfg_test_module_as_test_code() {
        let old = "pub fn answer() -> i32 {\n    42\n}\n\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn keeps_working() {\n        assert_eq!(answer(), 42);\n    }\n}\n";
        let new = "pub fn answer() -> i32 {\n    42\n}\n\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn keeps_working() {\n        let value = Some(answer()).unwrap();\n        assert_eq!(value, 42);\n    }\n}\n";
        let (_tmp, repo, base_id, target_id) = make_test_repo(&[("src/lib.rs", old, new)]);
        let out = TempDir::new().unwrap();
        let diff = make_diff_with_ids(
            base_id,
            target_id,
            vec![mock_file_change("src/lib.rs", FileStatus::Modified, 1, 0)],
        );

        generate_pattern_scan(out.path(), &[diff], &repo).unwrap();

        let scan = fs::read_to_string(out.path().join("PATTERN_SCAN.json")).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&scan).unwrap();
        assert_eq!(parsed["prod_hits"].as_u64(), Some(0));
        assert_eq!(parsed["test_hits"].as_u64(), Some(1));

        let unwrap_entry = parsed["by_pattern"]
            .as_array()
            .and_then(|items| items.iter().find(|item| item["pattern"] == "unwrap"))
            .expect("unwrap entry");
        assert_eq!(unwrap_entry["prod_count"].as_u64(), Some(0));
        assert_eq!(unwrap_entry["test_count"].as_u64(), Some(1));
    }

    // ── build_test_line_set tests ──────────────────────────────────────

    #[test]
    fn build_test_line_set_basic() {
        let content = "\
fn prod() {}
#[cfg(test)]
mod tests {
    fn t1() {}
    fn t2() {}
}
fn after() {}
";
        let set = build_test_line_set(content);
        // Lines 4, 5 are inside #[cfg(test)] mod tests { ... }
        // Line 3 is `mod tests {` — the opening brace line itself
        assert!(!set.contains(&1), "prod fn should not be test");
        assert!(
            !set.contains(&2),
            "#[cfg(test)] line itself is not inside the block yet"
        );
        assert!(set.contains(&4), "fn t1 inside test block");
        assert!(set.contains(&5), "fn t2 inside test block");
        assert!(!set.contains(&7), "fn after is outside test block");
    }

    #[test]
    fn build_test_line_set_mod_declaration() {
        let content = "\
fn prod() {}
#[cfg(test)]
mod tests;
fn still_prod() {}
";
        let set = build_test_line_set(content);
        // `mod tests;` is a declaration — no block, nothing should be marked as test
        assert!(
            set.is_empty(),
            "mod declaration should not mark any lines as test"
        );
    }

    #[test]
    fn build_test_line_set_standalone_test_fn() {
        let content = "\
fn prod() {}

#[test]
fn my_test() {
    assert!(true);
}

fn still_prod() {}
";
        let set = build_test_line_set(content);
        assert!(!set.contains(&1), "prod fn should not be test");
        assert!(!set.contains(&3), "#[test] annotation not inside body yet");
        assert!(set.contains(&5), "assert! inside #[test] fn body");
        assert!(!set.contains(&8), "fn after test fn should be prod");
    }

    #[test]
    fn build_test_line_set_tokio_test() {
        let content = "\
fn prod() {}

#[tokio::test]
async fn my_async_test() {
    let x = foo.unwrap();
}

fn also_prod() {}
";
        let set = build_test_line_set(content);
        assert!(!set.contains(&1), "prod fn");
        assert!(set.contains(&5), "unwrap inside #[tokio::test] fn");
        assert!(!set.contains(&8), "fn after async test should be prod");
    }

    #[test]
    fn build_test_line_set_test_fn_inside_cfg_test_not_double_counted() {
        // #[test] inside #[cfg(test)] should work — both flags active, no conflict
        let content = "\
#[cfg(test)]
mod tests {
    #[test]
    fn t() { x.unwrap(); }
}
";
        let set = build_test_line_set(content);
        assert!(
            set.contains(&4),
            "line inside both #[cfg(test)] and #[test] fn"
        );
        assert!(!set.contains(&6), "after closing brace");
    }

    #[test]
    fn build_test_line_set_braces_in_strings_ignored() {
        // Braces inside string literals must not affect depth tracking
        let content = "\
fn prod() {
    let s = \"closing brace here }\";
}

#[cfg(test)]
mod tests {
    fn t() { x.unwrap(); }
}
";
        let set = build_test_line_set(content);
        assert!(!set.contains(&1), "prod fn");
        assert!(!set.contains(&2), "string with brace — still prod");
        assert!(!set.contains(&3), "closing brace of prod fn");
        assert!(set.contains(&7), "unwrap inside test module");
    }
}
