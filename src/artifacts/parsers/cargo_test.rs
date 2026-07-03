use super::LintFinding;
use regex::Regex;
use serde::Serialize;
use std::collections::HashMap;
use std::sync::LazyLock;

static PANIC_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"thread '(.+?)' panicked at (.+?):(\d+):(\d+)").unwrap());

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct FailedTest {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub column: Option<u32>,
}

pub fn parse_cargo_test_output(output: &str) -> Vec<LintFinding> {
    let mut panic_locations: HashMap<String, (String, u32, u32)> = HashMap::new();
    for line in output.lines() {
        let Some(caps) = PANIC_RE.captures(line) else {
            continue;
        };

        panic_locations.insert(
            caps[1].to_string(),
            (
                caps[2].to_string(),
                caps[3].parse().unwrap_or_default(),
                caps[4].parse().unwrap_or_default(),
            ),
        );
    }

    let mut findings = Vec::new();
    let mut failures_blocks = 0usize;
    let mut in_clean_list = false;

    for line in output.lines() {
        let trimmed = line.trim();

        if trimmed == "failures:" {
            failures_blocks += 1;
            in_clean_list = failures_blocks == 2;
            continue;
        }

        if !in_clean_list {
            continue;
        }

        if trimmed.is_empty() || trimmed.starts_with("test result:") {
            break;
        }

        if !line.starts_with("    ") {
            continue;
        }

        let test_name = trimmed.to_string();
        let (file, line_num, col) =
            if let Some((file, line_num, col)) = panic_locations.get(&test_name) {
                (file.clone(), *line_num, Some(*col))
            } else {
                ("(test)".to_string(), 0, None)
            };

        findings.push(LintFinding {
            file,
            line: line_num,
            column: col,
            level: "error",
            message: format!("test {} failed", test_name),
            rule_id: None,
            source: "cargo_test",
        });
    }

    findings
}

pub fn extract_failed_test_names(output: &str) -> Vec<String> {
    extract_failed_tests_with_locations(output)
        .into_iter()
        .map(|test| test.name)
        .collect()
}

pub fn extract_failed_tests_with_locations(output: &str) -> Vec<FailedTest> {
    parse_cargo_test_output(output)
        .into_iter()
        .map(|finding| FailedTest {
            name: finding
                .message
                .strip_prefix("test ")
                .and_then(|message| message.strip_suffix(" failed"))
                .unwrap_or(&finding.message)
                .to_string(),
            file: (finding.file != "(test)").then_some(finding.file),
            line: (finding.line > 0).then_some(finding.line),
            column: finding.column,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{
        FailedTest, extract_failed_test_names, extract_failed_tests_with_locations,
        parse_cargo_test_output,
    };

    #[test]
    fn parses_single_failure_with_panic_location() {
        let output = "\
running 2 tests
test tests::good ... ok
thread 'tests::bad' panicked at src/lib.rs:42:5:
assertion failed
test tests::bad ... FAILED

failures:

---- tests::bad stdout ----

failures:
    tests::bad

test result: FAILED. 1 passed; 1 failed
";
        let findings = parse_cargo_test_output(output);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].file, "src/lib.rs");
        assert_eq!(findings[0].line, 42);
        assert_eq!(findings[0].column, Some(5));
    }

    #[test]
    fn parses_multiple_failures() {
        let output = "\
failures:

---- a stdout ----
---- b stdout ----

failures:
    a
    b

test result: FAILED.
";
        let findings = parse_cargo_test_output(output);
        assert_eq!(findings.len(), 2);
        assert!(findings.iter().all(|finding| finding.level == "error"));
    }

    #[test]
    fn returns_empty_for_all_pass() {
        let output = "running 5 tests\ntest a ... ok\ntest b ... ok\n\ntest result: ok. 5 passed\n";
        assert!(parse_cargo_test_output(output).is_empty());
    }

    #[test]
    fn parses_doctest_failure_without_panic_location() {
        let output = "\
running 1 test
test src/lib.rs - demo (line 12) ... FAILED

failures:

---- src/lib.rs - demo (line 12) stdout ----

failures:
    src/lib.rs - demo (line 12)

test result: FAILED. 0 passed; 1 failed
";
        let findings = parse_cargo_test_output(output);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].file, "(test)");
        assert!(findings[0].message.contains("src/lib.rs - demo (line 12)"));
    }

    #[test]
    fn extracts_failed_test_names() {
        let output = "\
failures:

---- a stdout ----
---- b stdout ----

failures:
    a
    b

test result: FAILED.
";

        assert_eq!(
            extract_failed_test_names(output),
            vec!["a".to_string(), "b".to_string()]
        );
    }

    #[test]
    fn extracts_failed_tests_with_locations() {
        let output = "\
thread 'tests::bad' panicked at src/lib.rs:42:5:
assertion failed
test tests::bad ... FAILED

failures:

---- tests::bad stdout ----

failures:
    tests::bad

test result: FAILED. 0 passed; 1 failed
";

        assert_eq!(
            extract_failed_tests_with_locations(output),
            vec![FailedTest {
                name: "tests::bad".to_string(),
                file: Some("src/lib.rs".to_string()),
                line: Some(42),
                column: Some(5),
            }]
        );
    }

    #[test]
    fn returns_empty_failed_test_names_for_pass_run() {
        let output = "running 5 tests\ntest a ... ok\ntest b ... ok\n\ntest result: ok. 5 passed\n";
        assert!(extract_failed_test_names(output).is_empty());
    }
}
