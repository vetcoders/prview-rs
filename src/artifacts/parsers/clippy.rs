use super::LintFinding;
use regex::Regex;
use std::sync::LazyLock;

static FINDING_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(.+?):(\d+):(\d+):\s+(warning|error)(?:\[(\w+)\])?:\s+(.+)$").unwrap()
});

fn is_noise(line: &str) -> bool {
    let trimmed = line.trim();
    trimmed.starts_with("Compiling ")
        || trimmed.starts_with("Finished ")
        || trimmed.starts_with("Checking ")
        || trimmed.starts_with("Downloading ")
        || trimmed.starts_with("Downloaded ")
        || trimmed.starts_with("Blocking waiting for file lock")
        || trimmed.starts_with("warning: ")
        || trimmed.starts_with("error: could not compile ")
        || trimmed.starts_with("error: aborting due to ")
}

pub fn parse_clippy_short_output(output: &str) -> Vec<LintFinding> {
    let mut findings = Vec::new();

    for line in output.lines() {
        if is_noise(line) {
            continue;
        }

        let Some(caps) = FINDING_RE.captures(line) else {
            continue;
        };

        findings.push(LintFinding {
            file: caps[1].to_string(),
            line: caps[2].parse().unwrap_or_default(),
            column: caps[3].parse().ok(),
            level: if caps.get(4).map(|m| m.as_str()) == Some("error") {
                "error"
            } else {
                "warning"
            },
            message: caps[6].trim().to_string(),
            rule_id: caps
                .get(5)
                .map(|m: regex::Match<'_>| m.as_str().to_string()),
            source: "clippy",
        });
    }

    findings
}

#[cfg(test)]
mod tests {
    use super::parse_clippy_short_output;

    #[test]
    fn parses_warning_without_error_code() {
        let output = "src/main.rs:10:5: warning: unused variable `x`\n";
        let findings = parse_clippy_short_output(output);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].level, "warning");
        assert_eq!(findings[0].rule_id, None);
    }

    #[test]
    fn parses_error_with_code() {
        let output = "src/lib.rs:42:1: error[E0308]: mismatched types\n";
        let findings = parse_clippy_short_output(output);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].level, "error");
        assert_eq!(findings[0].rule_id.as_deref(), Some("E0308"));
    }

    #[test]
    fn parses_mixed_findings() {
        let output = "src/a.rs:1:1: warning: first\nsrc/b.rs:2:3: error[E0123]: second\n";
        let findings = parse_clippy_short_output(output);
        assert_eq!(findings.len(), 2);
        assert_eq!(findings[0].level, "warning");
        assert_eq!(findings[1].level, "error");
    }

    #[test]
    fn filters_build_noise() {
        let output = "Compiling foo v0.1.0\nBlocking waiting for file lock on build directory\nwarning: 3 warnings emitted\n";
        assert!(parse_clippy_short_output(output).is_empty());
    }

    #[test]
    fn returns_empty_for_clean_output() {
        let output = "Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.20s\n";
        assert!(parse_clippy_short_output(output).is_empty());
    }
}
