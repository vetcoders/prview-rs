use super::LintFinding;
use regex::Regex;
use std::sync::LazyLock;

static FILE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(/[^\s].*|[A-Z]:\\[^\s].*)$").unwrap());

static FINDING_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\s+(\d+):(\d+)\s+(error|warning)\s+(.+?)(?:\s{2,}(\S+))?\s*$").unwrap()
});

pub fn parse_eslint_output(output: &str) -> Vec<LintFinding> {
    let mut findings = Vec::new();
    let mut current_file: Option<String> = None;

    for line in output.lines() {
        let trimmed = line.trim_end();

        if FILE_RE.is_match(trimmed) {
            current_file = Some(trimmed.to_string());
            continue;
        }

        if trimmed.starts_with('✖') || trimmed.starts_with("✔") {
            continue;
        }

        let Some(file) = current_file.as_ref() else {
            continue;
        };

        let Some(caps) = FINDING_RE.captures(trimmed) else {
            continue;
        };

        findings.push(LintFinding {
            file: file.clone(),
            line: caps[1].parse().unwrap_or_default(),
            column: caps[2].parse().ok(),
            level: if caps.get(3).map(|m| m.as_str()) == Some("error") {
                "error"
            } else {
                "warning"
            },
            message: caps[4].trim().to_string(),
            rule_id: caps
                .get(5)
                .map(|m: regex::Match<'_>| m.as_str().to_string()),
            source: "eslint",
        });
    }

    findings
}

#[cfg(test)]
mod tests {
    use super::parse_eslint_output;

    #[test]
    fn parses_single_error_with_rule_id() {
        let output = "/src/app.ts\n  10:5  error  Unexpected var  no-var\n";
        let findings = parse_eslint_output(output);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].file, "/src/app.ts");
        assert_eq!(findings[0].line, 10);
        assert_eq!(findings[0].column, Some(5));
        assert_eq!(findings[0].level, "error");
        assert_eq!(findings[0].rule_id.as_deref(), Some("no-var"));
    }

    #[test]
    fn parses_multiple_findings_in_one_file() {
        let output = "/src/foo.ts\n  1:1  error  msg1  rule-a\n  2:3  warning  msg2  rule-b\n";
        let findings = parse_eslint_output(output);
        assert_eq!(findings.len(), 2);
        assert_eq!(findings[0].level, "error");
        assert_eq!(findings[1].level, "warning");
    }

    #[test]
    fn parses_multiple_files() {
        let output = "/a.ts\n  1:1  error  msg  r1\n\n/b.ts\n  5:2  warning  msg2  r2\n";
        let findings = parse_eslint_output(output);
        assert_eq!(findings.len(), 2);
        assert_eq!(findings[0].file, "/a.ts");
        assert_eq!(findings[1].file, "/b.ts");
    }

    #[test]
    fn parses_finding_without_rule_id() {
        let output = "/src/bad.ts\n  3:1  error  Parsing error: Unexpected token\n";
        let findings = parse_eslint_output(output);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].rule_id, None);
    }

    #[test]
    fn returns_empty_for_empty_output() {
        assert!(parse_eslint_output("").is_empty());
    }

    #[test]
    fn skips_summary_only_output() {
        let output = "\n✖ 0 problems (0 errors, 0 warnings)\n";
        assert!(parse_eslint_output(output).is_empty());
    }

    #[test]
    fn supports_absolute_unix_and_windows_paths() {
        let output = "/tmp/src/app.ts\n  4:2  error  unix issue  no-console\n\nC:\\repo\\src\\main.ts\n  8:7  warning  win issue  no-alert\n";
        let findings = parse_eslint_output(output);
        assert_eq!(findings.len(), 2);
        assert_eq!(findings[0].file, "/tmp/src/app.ts");
        assert_eq!(findings[1].file, "C:\\repo\\src\\main.ts");
        assert_eq!(findings[1].level, "warning");
    }
}
