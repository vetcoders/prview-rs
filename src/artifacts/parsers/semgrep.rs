//! Structured parsers for Semgrep scan output.
//!
//! Prefer Semgrep JSON when available: it is the cleanest source for SARIF
//! locations, rule ids, severity, and messages. Keep the pretty-output parser
//! as a fallback for older captured logs and tests; Semgrep's human-readable
//! output embeds source snippets, including minified vendored JS, so the parser
//! must anchor findings to the block header rather than generic `file:line`
//! scraping.

use super::LintFinding;

/// Parse Semgrep JSON output into per-location findings.
pub fn parse_semgrep_json_output(output: &str) -> Vec<LintFinding> {
    let Ok(json) = serde_json::from_str::<serde_json::Value>(output.trim()) else {
        return Vec::new();
    };

    let Some(results) = json.get("results").and_then(|value| value.as_array()) else {
        return Vec::new();
    };

    results
        .iter()
        .filter_map(|result| {
            let file = result.get("path")?.as_str()?.to_string();
            let start = result.get("start")?;
            let line = start.get("line").and_then(|value| value.as_u64())? as u32;
            let column = start
                .get("col")
                .and_then(|value| value.as_u64())
                .map(|value| value as u32);
            let extra = result.get("extra");
            let message = extra
                .and_then(|value| value.get("message"))
                .and_then(|value| value.as_str())
                .or_else(|| result.get("message").and_then(|value| value.as_str()))
                .unwrap_or("Semgrep finding")
                .to_string();
            let severity = extra
                .and_then(|value| value.get("severity"))
                .and_then(|value| value.as_str())
                .unwrap_or("WARNING");
            let level = match severity.to_ascii_uppercase().as_str() {
                "ERROR" => "error",
                _ => "warning",
            };
            let rule_id = result
                .get("check_id")
                .and_then(|value| value.as_str())
                .map(ToString::to_string);

            Some(LintFinding {
                file,
                line,
                column,
                level,
                message,
                rule_id,
                source: "semgrep",
            })
        })
        .collect()
}

/// Parse Semgrep pretty (text) output into per-location findings.
pub fn parse_semgrep_text(output: &str) -> Vec<LintFinding> {
    let mut findings = Vec::new();

    let mut current_file: Option<String> = None;
    let mut current_rule: Option<String> = None;
    let mut message = String::new();
    let mut message_done = false;

    for raw in output.lines() {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            continue;
        }

        // Gutter line: `NNN┆ <code>` emits a finding; `⋮┆----` is a separator.
        if let Some(bar_idx) = trimmed.find('┆') {
            let prefix = trimmed[..bar_idx].trim();
            if let Ok(line) = prefix.parse::<u32>()
                && let (Some(file), Some(rule)) = (&current_file, &current_rule)
            {
                findings.push(LintFinding {
                    file: file.clone(),
                    line,
                    column: None,
                    level: level_for_rule(rule),
                    message: message.trim().to_string(),
                    rule_id: Some(rule.clone()),
                    source: "semgrep",
                });
            }
            message_done = true;
            continue;
        }

        if trimmed.starts_with("Details:") {
            message_done = true;
            continue;
        }

        if trimmed.starts_with('❯') || trimmed.starts_with('❱') {
            let rule = trimmed
                .trim_start_matches(|c: char| !c.is_ascii_alphanumeric())
                .trim();
            if !rule.is_empty() {
                current_rule = Some(rule.to_string());
                message.clear();
                message_done = false;
            }
            continue;
        }

        if looks_like_file_path(trimmed) {
            current_file = Some(trimmed.to_string());
            current_rule = None;
            message.clear();
            message_done = false;
            continue;
        }

        if current_rule.is_some() && !message_done {
            if !message.is_empty() {
                message.push(' ');
            }
            message.push_str(trimmed);
        }
    }

    findings
}

fn level_for_rule(rule_id: &str) -> &'static str {
    if rule_id.contains(".security.") && !rule_id.contains(".audit.") {
        "error"
    } else {
        "warning"
    }
}

fn looks_like_file_path(s: &str) -> bool {
    if s.is_empty() || s.chars().any(char::is_whitespace) {
        return false;
    }
    if !(s.contains('/') || s.contains('\\')) {
        return false;
    }
    const CODE_CHARS: &[char] = &[
        '{', '}', '"', '=', '(', ')', ';', ',', '\'', '`', '*', '│', '┆', '❯', '❱', '⋮', '<', '>',
        '[', ']',
    ];
    if s.contains(CODE_CHARS) {
        return false;
    }
    let base = s.rsplit(['/', '\\']).next().unwrap_or(s);
    base.contains('.')
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
┌──────────────────┐
│ 45 Code Findings │
└──────────────────┘

    editors/vscode/src/client.ts
    ❯❱ javascript.lang.security.audit.path-traversal.path-join-resolve-traversal.path-join-resolve-traversal
          Detected possible user input going into a `path.join` or `path.resolve` function. This could
          possibly lead to a path traversal vulnerability.
          Details: https://sg.run/OPqk

          135┆ const binDir = path.join(storageDir, 'bin');
            ⋮┆----------------------------------------
          139┆ const binaryPath = path.join(binDir, binaryName);

    loctree-rs/src/analyzer/assets/dagre.min.js
    ❯❱ javascript.lang.security.audit.prototype-pollution.prototype-pollution-loop.prototype-pollution-loop
          Possibility of prototype polluting function detected.
          Details: https://sg.run/w1DB

          1325┆ */function baseSet(object,path,value,customizer){if(!isObject(object)){return
               sted=nested[key]}return object}module.exports=baseSet},{"./_assignValue":75,"./_castPath":1
            ⋮┆----------------------------------------
"#;

    #[test]
    fn parses_semgrep_json_findings() {
        let output = r#"{
          "results": [{
            "check_id": "rust.lang.security.test",
            "path": "src/lib.rs",
            "start": {"line": 12, "col": 5},
            "extra": {"message": "problem", "severity": "ERROR"}
          }]
        }"#;

        let findings = parse_semgrep_json_output(output);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].file, "src/lib.rs");
        assert_eq!(findings[0].line, 12);
        assert_eq!(findings[0].column, Some(5));
        assert_eq!(findings[0].level, "error");
        assert_eq!(
            findings[0].rule_id.as_deref(),
            Some("rust.lang.security.test")
        );
    }

    #[test]
    fn extracts_text_findings_with_file_rule_and_line() {
        let findings = parse_semgrep_text(SAMPLE);
        assert_eq!(findings.len(), 3, "one finding per numbered gutter line");

        assert_eq!(findings[0].file, "editors/vscode/src/client.ts");
        assert_eq!(findings[0].line, 135);
        assert_eq!(
            findings[0].rule_id.as_deref(),
            Some(
                "javascript.lang.security.audit.path-traversal.path-join-resolve-traversal.path-join-resolve-traversal"
            )
        );
        assert!(findings[0].message.contains("path traversal"));
        assert_eq!(findings[0].source, "semgrep");

        assert_eq!(findings[1].file, "editors/vscode/src/client.ts");
        assert_eq!(findings[1].line, 139);

        assert_eq!(
            findings[2].file,
            "loctree-rs/src/analyzer/assets/dagre.min.js"
        );
        assert_eq!(findings[2].line, 1325);
    }

    #[test]
    fn never_treats_minified_code_as_a_file_path() {
        let leaked = "sted=nested[key]}return object}module.exports=baseSet},{\"./_assignValue\":75,\"./_castPath\":1";
        assert!(!looks_like_file_path(leaked));
    }

    #[test]
    fn audit_rules_are_warnings_non_audit_security_is_error() {
        assert_eq!(
            level_for_rule("html.security.audit.missing-integrity.missing-integrity"),
            "warning"
        );
        assert_eq!(
            level_for_rule("python.lang.security.dangerous-eval.dangerous-eval"),
            "error"
        );
        assert_eq!(level_for_rule("generic.style.no-tabs"), "warning");
    }

    #[test]
    fn empty_output_yields_no_findings() {
        assert!(parse_semgrep_text("").is_empty());
        assert!(parse_semgrep_text("┌────┐\n│ 0 Findings │\n└────┘\n").is_empty());
        assert!(parse_semgrep_json_output("").is_empty());
    }

    #[test]
    fn real_relative_paths_are_recognized() {
        assert!(looks_like_file_path("editors/vscode/src/client.ts"));
        assert!(looks_like_file_path("public_dist/404.html"));
        assert!(looks_like_file_path(
            "loctree-rs/src/analyzer/assets/dagre.min.js"
        ));
        assert!(!looks_like_file_path("Details: https://sg.run/OPqk"));
        assert!(!looks_like_file_path("bare-word"));
        assert!(!looks_like_file_path("nested[key]/module"));
    }
}
