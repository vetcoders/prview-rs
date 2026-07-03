use super::LintFinding;
use regex::Regex;
use std::sync::LazyLock;

static FINDING_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\s+(\d+):(\d+)\s+([✖⚠ℹ×‼i])\s+(.+?)(?:\s{2,}(\S[\S-]*))?\s*$").unwrap()
});

fn symbol_to_level(symbol: &str) -> &'static str {
    match symbol {
        "✖" | "×" => "error",
        "⚠" | "‼" => "warning",
        "ℹ" | "i" => "note",
        _ => "warning",
    }
}

pub fn parse_stylelint_output(output: &str) -> Vec<LintFinding> {
    let mut findings = Vec::new();
    let mut current_file: Option<String> = None;

    for line in output.lines() {
        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            continue;
        }

        if let Some(caps) = FINDING_RE.captures(trimmed) {
            let Some(file) = current_file.as_ref() else {
                continue;
            };

            findings.push(LintFinding {
                file: file.clone(),
                line: caps[1].parse().unwrap_or_default(),
                column: caps[2].parse().ok(),
                level: symbol_to_level(&caps[3]),
                message: caps[4].trim().to_string(),
                rule_id: caps
                    .get(5)
                    .map(|m: regex::Match<'_>| m.as_str().to_string()),
                source: "stylelint",
            });
            continue;
        }

        if !line.starts_with(' ') && !line.starts_with('\t') {
            current_file = Some(trimmed.to_string());
        }
    }

    findings
}

#[cfg(test)]
mod tests {
    use super::parse_stylelint_output;

    #[test]
    fn parses_error_symbol() {
        let output = "src/style.css\n  10:5  ✖  Unexpected unit  unit-disallowed-list\n";
        let findings = parse_stylelint_output(output);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].level, "error");
        assert_eq!(findings[0].rule_id.as_deref(), Some("unit-disallowed-list"));
    }

    #[test]
    fn parses_warning_symbol() {
        let output = "src/a.css\n  2:1  ⚠  Expected indentation  indentation\n";
        let findings = parse_stylelint_output(output);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].level, "warning");
    }

    #[test]
    fn parses_css_syntax_error_without_rule_id() {
        let output = "src/bad.css\n  1:1  ✖  Unknown word (CssSyntaxError)\n";
        let findings = parse_stylelint_output(output);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].level, "error");
        assert_eq!(findings[0].rule_id, None);
        assert!(findings[0].message.contains("CssSyntaxError"));
    }

    #[test]
    fn parses_multiple_files() {
        let output = "a.css\n  1:1  ✖  msg1  rule1\n\nb.css\n  3:2  ⚠  msg2  rule2\n";
        let findings = parse_stylelint_output(output);
        assert_eq!(findings.len(), 2);
        assert_eq!(findings[0].file, "a.css");
        assert_eq!(findings[1].file, "b.css");
    }

    #[test]
    fn returns_empty_for_empty_output() {
        assert!(parse_stylelint_output("").is_empty());
    }
}
