//! cargo-audit / cargo-tree finding parsers and summaries.

use super::*;

#[derive(Debug, Clone)]
pub(crate) struct CargoAuditFinding {
    pub(crate) advisory_id: String,
    pub(crate) package_name: String,
    pub(crate) package_version: String,
    pub(crate) title: String,
    pub(crate) severity: String,
    pub(crate) sarif_level: &'static str,
    pub(crate) patched_versions: Option<String>,
    pub(crate) help_url: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct CargoTreeIndex {
    pub(crate) paths_by_package: HashMap<(String, String), Vec<String>>,
}

impl CargoTreeIndex {
    pub(crate) fn from_text(tree: &str) -> Self {
        let mut stack: Vec<String> = Vec::new();
        let mut paths_by_package: HashMap<(String, String), Vec<String>> = HashMap::new();

        for raw_line in tree.lines() {
            if raw_line.trim().is_empty() {
                continue;
            }
            let Some(label_start) = raw_line
                .char_indices()
                .find_map(|(idx, ch)| ch.is_ascii_alphanumeric().then_some(idx))
            else {
                continue;
            };

            let label = raw_line[label_start..]
                .trim()
                .trim_end_matches(" (*)")
                .to_string();
            let depth = raw_line[..label_start].chars().count() / 4;
            if stack.len() <= depth {
                stack.resize(depth + 1, String::new());
            }
            stack[depth] = label.clone();
            stack.truncate(depth + 1);

            if let Some((name, version)) = parse_cargo_tree_package(&label) {
                let path = stack.join(" -> ");
                let entry = paths_by_package.entry((name, version)).or_default();
                if !entry.iter().any(|existing| existing == &path) {
                    entry.push(path);
                }
            }
        }

        Self { paths_by_package }
    }

    pub(crate) fn paths_for(&self, finding: &CargoAuditFinding, limit: usize) -> Vec<String> {
        self.paths_by_package
            .get(&(
                finding.package_name.clone(),
                finding.package_version.clone(),
            ))
            .map(|paths| paths.iter().take(limit).cloned().collect())
            .unwrap_or_default()
    }
}

pub(crate) fn parse_cargo_tree_package(label: &str) -> Option<(String, String)> {
    let package = label.split_once(" v")?;
    let version = package
        .1
        .split_whitespace()
        .next()?
        .trim_matches(|ch: char| ch == '(' || ch == ')')
        .to_string();
    Some((package.0.to_string(), version))
}

pub(crate) fn load_cargo_tree_index(root_dir: &Path) -> Option<CargoTreeIndex> {
    let cargo_tree_path = root_dir.join("30_context/cargo-tree.txt");
    let cargo_tree = fs::read_to_string(cargo_tree_path).ok()?;
    Some(CargoTreeIndex::from_text(&cargo_tree))
}

impl CargoAuditFinding {
    pub(crate) fn package_display(&self) -> String {
        format!("{}@{}", self.package_name, self.package_version)
    }

    pub(crate) fn summary_line(&self) -> String {
        let mut line = format!(
            "`{}` {} in `{}`",
            self.advisory_id,
            self.severity,
            self.package_display()
        );
        if !self.title.is_empty() && self.title != "Security advisory" {
            line.push_str(&format!(": {}", self.title));
        }
        if let Some(patched) = &self.patched_versions {
            line.push_str(&format!(" Fix: `{}`.", patched));
        } else {
            line.push('.');
        }
        line
    }

    pub(crate) fn sarif_message(&self) -> String {
        let mut message = format!("{} in {}", self.advisory_id, self.package_display());
        if !self.title.is_empty() && self.title != "Security advisory" {
            message.push_str(&format!(": {}", self.title));
        }
        message
    }
}

pub(crate) fn extract_embedded_json(output: &str) -> Option<serde_json::Value> {
    use serde::Deserialize;

    let start = output.find(['{', '['])?;
    let mut deserializer = serde_json::Deserializer::from_str(&output[start..]);
    serde_json::Value::deserialize(&mut deserializer).ok()
}

pub(crate) fn cargo_audit_text_list(value: Option<&serde_json::Value>) -> Option<String> {
    match value {
        Some(serde_json::Value::String(text)) if !text.trim().is_empty() => {
            Some(text.trim().to_string())
        }
        Some(serde_json::Value::Array(items)) => {
            let values: Vec<&str> = items.iter().filter_map(|item| item.as_str()).collect();
            if values.is_empty() {
                None
            } else {
                Some(values.join(", "))
            }
        }
        _ => None,
    }
}

pub(crate) fn cargo_audit_cvss_score(advisory: &serde_json::Value) -> Option<f64> {
    advisory
        .pointer("/cvss/score")
        .and_then(|value| value.as_f64())
        .or_else(|| advisory.get("cvss").and_then(|value| value.as_f64()))
        .or_else(|| {
            advisory
                .pointer("/cvss/score")
                .and_then(|value| value.as_str())
                .and_then(|value| value.parse::<f64>().ok())
        })
        .or_else(|| {
            advisory
                .get("cvss")
                .and_then(|value| value.as_str())
                .and_then(|value| value.parse::<f64>().ok())
        })
}

pub(crate) fn cargo_audit_severity(advisory: &serde_json::Value) -> (String, &'static str) {
    if let Some(score) = cargo_audit_cvss_score(advisory) {
        if score >= 9.0 {
            return ("critical".to_string(), "error");
        }
        if score >= 7.0 {
            return ("high".to_string(), "error");
        }
        if score >= 4.0 {
            return ("medium".to_string(), "warning");
        }
        return ("low".to_string(), "warning");
    }

    if let Some(level) = advisory
        .get("severity")
        .and_then(|value| value.as_str())
        .map(|value| value.to_ascii_lowercase())
    {
        let sarif_level = match level.as_str() {
            "critical" | "high" => "error",
            "medium" | "low" => "warning",
            _ => "error",
        };
        return (level, sarif_level);
    }

    ("unknown".to_string(), "error")
}

pub(crate) fn get_base_cargo_audit_findings(
    repo: Option<&crate::git::Repository>,
    diffs: &[crate::git::Diff],
) -> Option<std::collections::HashSet<(String, String)>> {
    use std::io::Write;
    use std::process::{Command, Stdio};

    let repo = repo?;

    let cargo_lock_diff = diffs
        .iter()
        .flat_map(|d| &d.files)
        .find(|f| f.path.ends_with("Cargo.lock"))?;

    let base_commit_id = diffs.first()?.base_commit_id.clone();
    let base_content = repo
        .file_at_commit(&base_commit_id, &cargo_lock_diff.path)
        .ok()?;

    let mut child = Command::new("cargo")
        .args(["audit", "--json", "-n", "-q", "-f", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(base_content.as_bytes()).ok()?;
    }

    let output = child.wait_with_output().ok()?;
    if let Ok(out_str) = String::from_utf8(output.stdout) {
        let findings = parse_cargo_audit_findings(&out_str);
        Some(
            findings
                .into_iter()
                .map(|f| (f.advisory_id, f.package_name))
                .collect(),
        )
    } else {
        None
    }
}

pub(crate) fn parse_cargo_audit_findings(output: &str) -> Vec<CargoAuditFinding> {
    let Some(parsed) = extract_embedded_json(output) else {
        return Vec::new();
    };
    let Some(entries) = parsed
        .pointer("/vulnerabilities/list")
        .and_then(|value| value.as_array())
    else {
        return Vec::new();
    };

    entries
        .iter()
        .map(|entry| {
            let advisory = entry.get("advisory").unwrap_or(&serde_json::Value::Null);
            let package = entry.get("package").unwrap_or(&serde_json::Value::Null);
            let versions = entry.get("versions").unwrap_or(&serde_json::Value::Null);
            let (severity, sarif_level) = cargo_audit_severity(advisory);

            CargoAuditFinding {
                advisory_id: advisory
                    .get("id")
                    .and_then(|value| value.as_str())
                    .unwrap_or("cargo-audit")
                    .to_string(),
                package_name: package
                    .get("name")
                    .and_then(|value| value.as_str())
                    .unwrap_or("unknown-package")
                    .to_string(),
                package_version: package
                    .get("version")
                    .and_then(|value| value.as_str())
                    .unwrap_or("unknown")
                    .to_string(),
                title: advisory
                    .get("title")
                    .and_then(|value| value.as_str())
                    .or_else(|| advisory.get("description").and_then(|value| value.as_str()))
                    .unwrap_or("Security advisory")
                    .trim()
                    .to_string(),
                severity,
                sarif_level,
                patched_versions: cargo_audit_text_list(versions.get("patched"))
                    .or_else(|| cargo_audit_text_list(versions.get("unaffected"))),
                help_url: advisory
                    .get("url")
                    .and_then(|value| value.as_str())
                    .or_else(|| advisory.get("reference").and_then(|value| value.as_str()))
                    .map(str::to_string),
            }
        })
        .collect()
}

pub(crate) fn cargo_audit_summary_cause(findings: &[CargoAuditFinding]) -> String {
    use std::collections::BTreeSet;

    let affected_packages: BTreeSet<String> = findings
        .iter()
        .map(CargoAuditFinding::package_display)
        .collect();
    format!(
        "{} security {} affecting {} locked {}",
        findings.len(),
        if findings.len() == 1 {
            "advisory"
        } else {
            "advisories"
        },
        affected_packages.len(),
        if affected_packages.len() == 1 {
            "dependency"
        } else {
            "dependencies"
        }
    )
}

pub(crate) fn cargo_audit_advisory_ids(findings: &[CargoAuditFinding], limit: usize) -> String {
    let display_count = limit.min(findings.len());
    let mut ids: Vec<String> = findings
        .iter()
        .take(display_count)
        .map(|finding| finding.advisory_id.clone())
        .collect();
    if display_count < findings.len() {
        ids.push(format!("+{} more", findings.len() - display_count));
    }
    ids.join(", ")
}

pub(crate) fn cargo_audit_cli_summary(output: &str) -> Option<String> {
    let findings = parse_cargo_audit_findings(output);
    if !findings.is_empty() {
        return Some(format!(
            "{} ({})",
            cargo_audit_summary_cause(&findings),
            cargo_audit_advisory_ids(&findings, 3)
        ));
    }

    extract_cargo_audit_root_cause(output).map(|root_cause| {
        if root_cause.evidence.is_empty() {
            root_cause.cause
        } else {
            format!("{} ({})", root_cause.cause, root_cause.evidence)
        }
    })
}

/// Parse informational warnings (unmaintained, unsound, notice) from cargo audit JSON output.
/// Returns a summary string like "2 informational advisory(ies): paste (unmaintained), ..."
/// Returns None if no informational warnings are present.
/// Extract a one-line summary from cargo geiger output.
///
/// Looks for the metric summary line like "3/10 unsafe usage(s) in 2 crate(s)".
pub(crate) fn extract_geiger_summary(output: &str) -> String {
    // Geiger outputs lines like "N/M unsafe usage(s) in K crate(s)"
    for line in output.lines() {
        let trimmed = line.trim();
        if trimmed.contains("unsafe") && trimmed.contains("usage") {
            return trimmed.to_string();
        }
    }
    // Fallback: count lines that mention "unsafe"
    let unsafe_lines = output.lines().filter(|l| l.contains("unsafe")).count();
    if unsafe_lines > 0 {
        format!("{} lines mentioning unsafe", unsafe_lines)
    } else {
        "warnings detected (see log for details)".to_string()
    }
}

pub(crate) fn cargo_audit_informational_summary(output: &str) -> Option<String> {
    let parsed = extract_embedded_json(output)?;
    let warnings_map = parsed.get("warnings")?.as_object()?;

    let mut items: Vec<String> = Vec::new();
    for (kind, entries) in warnings_map {
        if let Some(arr) = entries.as_array() {
            for entry in arr {
                let pkg_name = entry
                    .pointer("/package/name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown");
                items.push(format!("{} ({})", pkg_name, kind));
            }
        }
    }

    if items.is_empty() {
        return None;
    }

    Some(format!(
        "{} informational {}: {}",
        items.len(),
        if items.len() == 1 {
            "advisory"
        } else {
            "advisories"
        },
        items.join(", ")
    ))
}

pub(crate) fn cargo_audit_best_location() -> &'static str {
    "Cargo.lock"
}

pub(crate) fn cargo_audit_location_for_check(check: &CheckResult) -> String {
    check
        .provenance
        .as_ref()
        .map(|prov| Path::new(&prov.cwd).join(cargo_audit_best_location()))
        .map(|path| path.to_string_lossy().to_string())
        .unwrap_or_else(|| cargo_audit_best_location().to_string())
}

pub(crate) fn append_cargo_audit_findings(
    md: &mut String,
    findings: &[CargoAuditFinding],
    limit: Option<usize>,
    cargo_tree: Option<&CargoTreeIndex>,
) {
    let display_count = limit.unwrap_or(findings.len()).min(findings.len());
    for finding in findings.iter().take(display_count) {
        md.push_str("- ");
        md.push_str(&finding.summary_line());
        if let Some(cargo_tree) = cargo_tree {
            let paths = cargo_tree.paths_for(finding, 2);
            if !paths.is_empty() {
                md.push_str(" Dependency path: ");
                md.push_str(
                    &paths
                        .iter()
                        .map(|path| format!("`{path}`"))
                        .collect::<Vec<_>>()
                        .join("; "),
                );
                md.push('.');
            }
        }
        if let Some(url) = &finding.help_url {
            md.push_str(&format!(" Ref: {}.", url));
        }
        md.push('\n');
    }
    if display_count < findings.len() {
        md.push_str(&format!(
            "- ... plus {} more advisory findings in `30_context/INLINE_FINDINGS.sarif`\n",
            findings.len() - display_count
        ));
    }
}
