//! Diff-aware lint metrics (PRV-205).

use super::*;

// ---------------------------------------------------------------------------
// PRV-205: Diff-aware Lint Metrics
// ---------------------------------------------------------------------------

/// Check if a check result is lint-related based on its name.
pub(crate) fn is_lint_check(name: &str) -> bool {
    let lower = name.to_lowercase();
    lower.contains("clippy")
        || lower.contains("eslint")
        || lower.contains("ruff")
        || lower.contains("mypy")
        || lower.contains("lint")
        || lower.contains("pylint")
        || lower.contains("biome")
        || lower.contains("stylelint")
}

/// Normalize a path extracted from lint output for comparison with diff file paths.
///
/// Handles:
/// - Leading `./` and `/`
/// - Absolute Unix paths (`/home/user/project/src/foo.rs` → `src/foo.rs`)
/// - Windows paths (`C:\Users\...\src\foo.rs` → `src/foo.rs`)
/// - Backslash → forward slash conversion
pub(crate) fn normalize_lint_path(path: &str) -> String {
    let mut p = path.trim().replace('\\', "/");

    // Strip Windows drive letter prefix (e.g. `C:/`)
    if p.len() >= 3
        && p.as_bytes()[0].is_ascii_alphabetic()
        && p.as_bytes()[1] == b':'
        && p.as_bytes()[2] == b'/'
    {
        p = p[3..].to_string();
    }

    // Strip leading `./`
    if let Some(rest) = p.strip_prefix("./") {
        p = rest.to_string();
    }

    // Strip leading `/`
    if let Some(rest) = p.strip_prefix('/') {
        p = rest.to_string();
    }

    // Try to relativize deep absolute paths by stripping a long prefix up to
    // a known project-root boundary.
    // e.g. `/Users/dev/Git/myapp/src/app.ts` → `src/app.ts`
    //
    // We require the prefix before the marker to contain at least 2 path segments
    // (i.e. at least 1 `/` in prefix) to avoid false positives on shallow relative paths
    // like `src-tauri/src/lib.rs` (prefix "src-tauri" has 0 slashes → don't cut).
    // Paths like `Users/dev/project/src/...` (prefix has 2+ slashes) get cut.
    if p.contains('/') {
        let markers = ["src/", "lib/", "app/", "pkg/", "crates/", "tests/", "test/"];
        for marker in markers {
            let needle = format!("/{marker}");
            if let Some(idx) = p.find(&needle) {
                let prefix = &p[..idx];
                let prefix_depth = prefix.chars().filter(|&c| c == '/').count();
                if prefix_depth >= 2 {
                    return p[idx + 1..].to_string();
                }
            }
        }
    }

    p
}

/// Parse lint output for file references (file:line patterns).
/// Returns a list of normalized file paths found in the output.
/// Each occurrence is counted separately (i.e. duplicates = multiple issues in same file).
///
/// Handles two output formats:
/// 1. **Inline** (`file:line`): clippy, ruff, mypy, eslint --format=compact
/// 2. **Block** (path on its own line, indented `line:col` below): eslint/stylelint default
pub(crate) fn parse_lint_issues(output: &str) -> Vec<String> {
    let mut results = Vec::new();

    // Strategy 1: inline file:line pattern
    // Handles clippy (`--> src/foo.rs:42:9`), ruff, mypy, eslint compact, Windows paths
    let re_inline =
        regex::Regex::new(r"(?m)(?:-->\s*)?([a-zA-Z0-9_.][a-zA-Z0-9_./\\\-]*\.[a-zA-Z0-9]+):(\d+)")
            .expect("lint inline regex must compile");

    for cap in re_inline.captures_iter(output) {
        if let Some(file) = cap.get(1) {
            let f = file.as_str();
            // Skip version strings like `1.2.3:4`
            if f.chars().all(|c| c.is_ascii_digit() || c == '.') {
                continue;
            }
            results.push(normalize_lint_path(f));
        }
    }

    // Strategy 2: block format (eslint/stylelint default formatter)
    // Path alone on a line, followed by indented `line:col  severity  message` lines
    //
    //   /Users/dev/project/src/app.tsx
    //     10:5  warning  Unexpected console statement  no-console
    //     25:1  error    Missing semicolon             semi
    //
    let re_block_path =
        regex::Regex::new(r"(?m)^([a-zA-Z0-9_./\\\-][a-zA-Z0-9_./\\\- ]*\.[a-zA-Z0-9]+)\s*$")
            .expect("lint block path regex must compile");
    let re_block_issue =
        regex::Regex::new(r"(?m)^\s+\d+:\d+\s+").expect("lint block issue regex must compile");

    let lines: Vec<&str> = output.lines().collect();
    for (i, line) in lines.iter().enumerate() {
        if let Some(cap) = re_block_path.captures(line) {
            let Some(path) = cap.get(1).map(|m| m.as_str()) else {
                debug_assert!(false, "re_block_path must capture group 1");
                continue;
            };
            // Verify next line(s) have indented line:col pattern (not a false positive)
            let has_issues = lines
                .get(i + 1)
                .is_some_and(|next| re_block_issue.is_match(next));
            if has_issues {
                // Count each indented issue line under this path
                for subsequent in &lines[i + 1..] {
                    if re_block_issue.is_match(subsequent) {
                        results.push(normalize_lint_path(path));
                    } else if subsequent.trim().is_empty() {
                        // Blank line ends this file's block
                        break;
                    } else {
                        break;
                    }
                }
            }
        }
    }

    results
}

/// Compute diff-aware lint metrics for all lint checks.
///
/// For each lint check, parses its output for file references and classifies
/// issues as "new" (file is in the diff) or "legacy" (file is not in the diff).
pub(crate) fn compute_lint_metrics(checks: &[CheckResult], diffs: &[Diff]) -> Vec<LintMetrics> {
    use crate::checks::CheckStatus;
    use std::collections::{BTreeSet, HashSet};

    // Collect all changed file paths from the diff
    let changed_files: HashSet<String> = diffs
        .iter()
        .flat_map(|d| d.files.iter().map(|f| normalize_lint_path(&f.path)))
        .collect();

    let mut metrics = Vec::new();

    for check in checks {
        if !is_lint_check(&check.name) {
            continue;
        }
        // Skip checks in Error state — output may be garbage
        if check.status == CheckStatus::Error {
            continue;
        }

        let issue_files = parse_lint_issues(&check.output);
        let total_issues = issue_files.len();

        if total_issues == 0 {
            // Lint check ran but found no issues — include as clean
            metrics.push(LintMetrics {
                check_name: check.name.clone(),
                new_issues: 0,
                legacy_issues: 0,
                total_issues: 0,
                changed_files_with_issues: Vec::new(),
            });
            continue;
        }

        let mut new_issues = 0usize;
        let mut legacy_issues = 0usize;
        let mut changed_with_issues: BTreeSet<String> = BTreeSet::new();

        for file in &issue_files {
            if changed_files.contains(file.as_str()) {
                new_issues += 1;
                changed_with_issues.insert(file.clone());
            } else {
                legacy_issues += 1;
            }
        }

        metrics.push(LintMetrics {
            check_name: check.name.clone(),
            new_issues,
            legacy_issues,
            total_issues,
            changed_files_with_issues: changed_with_issues.into_iter().collect(),
        });
    }

    metrics
}
