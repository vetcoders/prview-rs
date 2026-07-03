//! Filtered checks log — errors/warnings only, no compilation noise.

use crate::checks::{CheckResult, CheckStatus};
use anyhow::Result;
use std::collections::HashSet;
use std::fmt::Write as FmtWrite;
use std::fs;
use std::path::Path;

/// Generate `checks-errors.log` — only errors/warnings, no compilation noise.
pub fn generate_checks_errors_log(dir: &Path, checks: &[CheckResult]) -> Result<()> {
    let mut output = String::new();

    for check in checks {
        if matches!(check.status, CheckStatus::Passed | CheckStatus::Skipped) {
            continue;
        }

        let lines: Vec<&str> = check.output.lines().collect();
        let matched_indices = find_error_lines(&lines);

        writeln!(output, "=== {} ({}) ===", check.name, check.status.as_str())?;

        if matched_indices.is_empty() {
            writeln!(output, "(no errors/warnings extracted)")?;
        } else {
            let expanded = expand_with_context(&matched_indices, lines.len(), 2);
            for &idx in &expanded {
                writeln!(output, "{}", lines[idx])?;
            }
        }

        writeln!(output)?;
    }

    if !output.is_empty() {
        fs::write(dir.join("checks-errors.log"), &output)?;
    }

    Ok(())
}

/// Find indices of lines that match error/warning patterns.
fn find_error_lines(lines: &[&str]) -> Vec<usize> {
    lines
        .iter()
        .enumerate()
        .filter(|(_, line)| is_error_line(line))
        .map(|(i, _)| i)
        .collect()
}

/// Check if a line matches error/warning patterns.
fn is_error_line(line: &str) -> bool {
    let lower = line.to_lowercase();

    if lower.contains("error") || lower.contains("warn") {
        // Exclude compilation noise
        if lower.trim_start().starts_with("compiling")
            || lower.trim_start().starts_with("downloading")
            || lower.trim_start().starts_with("updating")
        {
            return false;
        }
        return true;
    }

    if lower.contains("fail") || lower.contains("panicked") || lower.contains("panic") {
        return true;
    }

    if line.starts_with("test ") && lower.contains("failed") {
        return true;
    }

    // Rust error context lines
    if line.starts_with('^') || line.starts_with("  --> ") {
        return true;
    }

    false
}

/// Expand matched indices with context lines, deduplicating overlaps.
fn expand_with_context(matched: &[usize], total_lines: usize, context: usize) -> Vec<usize> {
    let mut expanded = HashSet::new();

    for &idx in matched {
        let start = idx.saturating_sub(context);
        let end = (idx + context + 1).min(total_lines);
        for i in start..end {
            expanded.insert(i);
        }
    }

    let mut sorted: Vec<usize> = expanded.into_iter().collect();
    sorted.sort_unstable();
    sorted
}

#[cfg(test)]
mod tests {
    use super::super::test_helpers::mock_check;
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn checks_errors_log_filters_noise() {
        let tmp = TempDir::new().unwrap();
        let checks = vec![mock_check(
            "cargo build",
            CheckStatus::Failed,
            concat!(
                "   Compiling foo v1.0.0\n",
                "   Compiling bar v2.0.0\n",
                "   Compiling dep v3.0.0\n",
                "   Compiling qux v4.0.0\n",
                "error[E0308]: mismatched types\n",
                "  --> src/lib.rs:42:5\n",
                "   |   expected u32, found bool\n",
                "warning: unused variable `x`\n",
                "   Compiling baz v1.0.0\n",
                "   Compiling fin v5.0.0\n",
                "   Compiling end v6.0.0\n",
                "   Compiling last v7.0.0\n",
            ),
        )];

        generate_checks_errors_log(tmp.path(), &checks).unwrap();

        let content = fs::read_to_string(tmp.path().join("checks-errors.log")).unwrap();

        assert!(content.contains("error[E0308]"));
        assert!(content.contains("warning: unused variable"));
        assert!(content.contains("  --> src/lib.rs:42:5"));
        // Compilation lines far from errors should be filtered out
        assert!(!content.contains("Compiling foo"));
        assert!(!content.contains("Compiling bar"));
        assert!(!content.contains("Compiling last"));
        // Note: lines within 2-line context of errors may appear — that's expected
    }

    #[test]
    fn checks_errors_log_skips_passed() {
        let tmp = TempDir::new().unwrap();
        let checks = vec![
            mock_check(
                "cargo test",
                CheckStatus::Passed,
                "test result: ok. 42 passed",
            ),
            mock_check("cargo clippy", CheckStatus::Passed, "no warnings"),
        ];

        generate_checks_errors_log(tmp.path(), &checks).unwrap();

        let log_path = tmp.path().join("checks-errors.log");
        assert!(
            !log_path.exists(),
            "Should not create file for all-passed checks"
        );
    }

    #[test]
    fn is_error_line_patterns() {
        assert!(is_error_line("error[E0308]: mismatched types"));
        assert!(is_error_line("warning: unused variable `x`"));
        assert!(is_error_line("FAILED tests::my_test"));
        assert!(is_error_line(
            "thread 'main' panicked at 'index out of bounds'"
        ));
        assert!(is_error_line("  --> src/lib.rs:42:5"));
        assert!(is_error_line("test result: FAILED. 1 passed; 1 failed"));

        assert!(!is_error_line("   Compiling foo v1.0.0"));
        assert!(!is_error_line("  Downloading crates.io index"));
        assert!(!is_error_line("    Updating crates.io index"));
        assert!(!is_error_line("running 42 tests"));
        assert!(!is_error_line("test result: ok. 42 passed"));
    }
}
