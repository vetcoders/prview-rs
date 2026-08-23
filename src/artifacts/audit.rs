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

/// Fingerprint a cargo-audit finding for baseline comparison. The locked
/// package VERSION is part of the key (R5-22): a dependency update that swaps one
/// vulnerable version for another under the same advisory is a NEW finding, not
/// pre-existing debt. Keying by `(advisory_id, package_name)` alone treated the
/// bumped version as already-present in the base and downgraded a failed
/// security check to pre-existing — approving a PR that re-introduces the
/// vulnerability with a different locked version.
pub(crate) fn cargo_audit_finding_key(finding: &CargoAuditFinding) -> (String, String, String) {
    (
        finding.advisory_id.clone(),
        finding.package_name.clone(),
        finding.package_version.clone(),
    )
}

/// Parse a complete cargo-audit report into every advisory-like item, including
/// informational `warnings` categories. The key includes the locked version so
/// dependency changes cannot launder a finding as pre-existing.
///
/// The outer `Option` distinguishes a valid clean report from truncated output
/// or a tool error. Treating both as an empty set would manufacture resolved
/// advisories and a false clean baseline.
pub(crate) fn cargo_audit_report_advisory_keys(
    output: &str,
) -> Option<std::collections::HashSet<(String, String, String)>> {
    let parsed = extract_embedded_json(output)?;
    crate::checks::validated_cargo_audit_vulnerability_list(&parsed)?;

    let mut keys: std::collections::HashSet<_> = parse_cargo_audit_findings(output)
        .iter()
        .map(cargo_audit_finding_key)
        .collect();
    let Some(warnings) = parsed.get("warnings").and_then(|value| value.as_object()) else {
        return Some(keys);
    };
    for entries in warnings.values().filter_map(|value| value.as_array()) {
        for entry in entries {
            let Some(advisory_id) = entry
                .pointer("/advisory/id")
                .and_then(|value| value.as_str())
            else {
                continue;
            };
            let Some(package_name) = entry
                .pointer("/package/name")
                .and_then(|value| value.as_str())
            else {
                continue;
            };
            let Some(package_version) = entry
                .pointer("/package/version")
                .and_then(|value| value.as_str())
            else {
                continue;
            };
            keys.insert((
                advisory_id.to_string(),
                package_name.to_string(),
                package_version.to_string(),
            ));
        }
    }
    Some(keys)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CargoAuditComparisonContext {
    base_commit_id: String,
    base_lock_path: Option<String>,
    target_lock_path: Option<String>,
    cargo_cwd: std::path::PathBuf,
    lock_changed: bool,
}

fn effective_cargo_lock_path_at_commit(
    repo: &crate::git::Repository,
    commit_id: &str,
    relative_root: &std::path::Path,
) -> Option<Option<String>> {
    let member_lock = if relative_root == std::path::Path::new(".") {
        "Cargo.lock".to_string()
    } else {
        relative_root
            .join("Cargo.lock")
            .to_string_lossy()
            .replace('\\', "/")
    };
    match repo.regular_file_at_commit(commit_id, &member_lock) {
        Ok(true) => return Some(Some(member_lock)),
        Ok(false) => {}
        Err(_) => return None,
    }

    if member_lock == "Cargo.lock" {
        return Some(None);
    }
    match repo.regular_file_at_commit(commit_id, "Cargo.lock") {
        Ok(true) => Some(Some("Cargo.lock".to_string())),
        Ok(false) => Some(None),
        Err(_) => None,
    }
}

fn cargo_audit_comparison_context_for_diff(
    repo: &crate::git::Repository,
    diff: &crate::git::Diff,
    cargo_root: Option<&std::path::Path>,
) -> Option<CargoAuditComparisonContext> {
    let configured_root = cargo_root.unwrap_or_else(|| repo.path());
    let normalized = crate::paths::normalize_to_repo_relative(
        &configured_root.display().to_string(),
        repo.path(),
    );
    if normalized.is_external {
        return None;
    }
    let relative_root = std::path::Path::new(&normalized.display);
    let cargo_cwd = if relative_root == std::path::Path::new(".") {
        repo.path().to_path_buf()
    } else {
        repo.path().join(relative_root)
    };
    let base_lock_path =
        effective_cargo_lock_path_at_commit(repo, &diff.base_commit_id, relative_root)?;
    let target_lock_path =
        effective_cargo_lock_path_at_commit(repo, &diff.target_commit_id, relative_root)?;
    let lock_changed = base_lock_path != target_lock_path
        || diff.files.iter().any(|file| {
            base_lock_path.as_deref() == Some(file.path.as_str())
                || target_lock_path.as_deref() == Some(file.path.as_str())
        });

    Some(CargoAuditComparisonContext {
        base_commit_id: diff.base_commit_id.clone(),
        base_lock_path,
        target_lock_path,
        cargo_cwd,
        lock_changed,
    })
}

fn cargo_audit_comparison_context(
    repo: &crate::git::Repository,
    diffs: &[crate::git::Diff],
    cargo_root: Option<&std::path::Path>,
) -> Option<CargoAuditComparisonContext> {
    let mut first_resolved = None;
    let mut changed = None;
    for diff in diffs {
        let Some(context) = cargo_audit_comparison_context_for_diff(repo, diff, cargo_root) else {
            continue;
        };
        if context.lock_changed {
            if changed.is_some() {
                // One cargo-audit invocation cannot truthfully classify against
                // two different historical lockfiles. The caller will preserve
                // the changed-lock signal but report the baseline unavailable.
                return None;
            }
            changed = Some(context);
            continue;
        }
        first_resolved.get_or_insert(context);
    }
    changed.or(first_resolved)
}

pub(crate) fn cargo_audit_lock_changed(
    repo: Option<&crate::git::Repository>,
    diffs: &[crate::git::Diff],
    cargo_root: Option<&std::path::Path>,
) -> bool {
    repo.and_then(|repo| cargo_audit_comparison_context(repo, diffs, cargo_root))
        .map(|context| context.lock_changed)
        .unwrap_or_else(|| {
            diffs
                .iter()
                .flat_map(|diff| &diff.files)
                .any(|file| file.path.ends_with("Cargo.lock"))
        })
}

pub(crate) fn get_base_cargo_audit_findings(
    repo: Option<&crate::git::Repository>,
    diffs: &[crate::git::Diff],
    cargo_root: Option<&std::path::Path>,
) -> Option<std::collections::HashSet<(String, String, String)>> {
    use std::io::Write;
    use std::process::{Command, Stdio};

    let repo = repo?;
    let context = cargo_audit_comparison_context(repo, diffs, cargo_root)?;
    if !context.lock_changed {
        return None;
    }
    let cargo_lock_path = context.base_lock_path?;
    let base_content = repo
        .file_at_commit(&context.base_commit_id, &cargo_lock_path)
        .ok()?;

    // cargo-audit explicitly documents `-` as the stdin sentinel for --file.
    // Keeping the historical lock out of the target worktree avoids making the
    // baseline comparison mutate or materialise a second checkout.
    let mut child = Command::new("cargo")
        .args(["audit", "--json", "-n", "-q", "-f", "-"])
        .current_dir(context.cargo_cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(base_content.as_bytes()).ok()?;
    }

    let output = child.wait_with_output().ok()?;
    let out_str = String::from_utf8(output.stdout).ok()?;
    cargo_audit_report_advisory_keys(&out_str)
}

pub(crate) fn parse_cargo_audit_findings(output: &str) -> Vec<CargoAuditFinding> {
    let Some(parsed) = extract_embedded_json(output) else {
        return Vec::new();
    };
    let Some(entries) = crate::checks::validated_cargo_audit_vulnerability_list(&parsed) else {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn commit_fixture(root: &std::path::Path, files: &[(&str, &str)]) -> String {
        let raw = if root.join(".git").exists() {
            git2::Repository::open(root).expect("git open")
        } else {
            git2::Repository::init(root).expect("git init")
        };
        let parent = raw.head().ok().and_then(|head| head.peel_to_commit().ok());
        for (path, content) in files {
            let path = root.join(path);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).expect("fixture parent");
            }
            std::fs::write(path, content).expect("fixture write");
        }
        let mut index = raw.index().expect("index");
        for (path, _) in files {
            index
                .add_path(std::path::Path::new(path))
                .expect("index add");
        }
        index.write().expect("index write");
        let tree_id = index.write_tree().expect("tree id");
        let tree = raw.find_tree(tree_id).expect("tree");
        let signature =
            git2::Signature::now("PrView Test", "prview@example.test").expect("signature");
        let parents: Vec<&git2::Commit<'_>> = parent.iter().collect();
        raw.commit(
            Some("HEAD"),
            &signature,
            &signature,
            "fixture",
            &tree,
            &parents,
        )
        .expect("commit")
        .to_string()
    }

    fn lock_diff(base: &str, target: &str, paths: &[&str]) -> crate::git::Diff {
        crate::git::Diff {
            base: "main".to_string(),
            target: "feature".to_string(),
            base_commit_id: base.to_string(),
            target_commit_id: target.to_string(),
            files: paths
                .iter()
                .map(|path| crate::git::FileChange {
                    path: (*path).to_string(),
                    status: crate::git::FileStatus::Modified,
                    additions: 1,
                    deletions: 1,
                })
                .collect(),
            stats: Default::default(),
            commits: vec![],
        }
    }

    fn finding(advisory_id: &str, package_name: &str, package_version: &str) -> CargoAuditFinding {
        CargoAuditFinding {
            advisory_id: advisory_id.to_string(),
            package_name: package_name.to_string(),
            package_version: package_version.to_string(),
            title: "vuln".to_string(),
            severity: "high".to_string(),
            sarif_level: "error",
            patched_versions: None,
            help_url: None,
        }
    }

    #[test]
    fn version_swap_under_same_advisory_is_a_new_finding() {
        // R5-22: the base has foo@1.0.0 flagged by RUSTSEC-0001. The PR bumps foo
        // to 2.0.0, still flagged by the same advisory. Keyed WITH the version,
        // the bumped version is not in the base set, so it is a new (in-diff)
        // finding — not pre-existing debt that would be silently approved.
        let base = finding("RUSTSEC-0001", "foo", "1.0.0");
        let bumped = finding("RUSTSEC-0001", "foo", "2.0.0");
        let unchanged = finding("RUSTSEC-0001", "foo", "1.0.0");

        let base_set: std::collections::HashSet<(String, String, String)> =
            std::iter::once(cargo_audit_finding_key(&base)).collect();

        assert!(
            !base_set.contains(&cargo_audit_finding_key(&bumped)),
            "a different vulnerable version under the same advisory must be new"
        );
        assert!(
            base_set.contains(&cargo_audit_finding_key(&unchanged)),
            "an identical (advisory, package, version) stays pre-existing"
        );
    }

    #[test]
    fn all_advisory_keys_include_informational_warnings() {
        let output = r#"{
          "vulnerabilities":{"list":[]},
          "warnings":{"unmaintained":[{
            "advisory":{"id":"RUSTSEC-2024-0001"},
            "package":{"name":"demo","version":"1.2.3"}
          }]}
        }"#;
        let keys = cargo_audit_report_advisory_keys(output).expect("valid report");
        assert!(keys.contains(&(
            "RUSTSEC-2024-0001".to_string(),
            "demo".to_string(),
            "1.2.3".to_string()
        )));
    }

    #[test]
    fn malformed_informational_entries_do_not_collapse_into_sentinel_keys() {
        let output = r#"{
          "vulnerabilities":{"list":[]},
          "warnings":{"unmaintained":[
            {"advisory":{"id":"RUSTSEC-2024-0001"},"package":{"name":"demo"}},
            {"package":{"name":"demo","version":"1.2.3"}}
          ]}
        }"#;

        assert!(
            cargo_audit_report_advisory_keys(output)
                .expect("valid report")
                .is_empty()
        );
    }

    #[test]
    fn baseline_context_selects_the_configured_cargo_root_lock() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let commit = commit_fixture(
            tmp.path(),
            &[
                ("Cargo.lock", "root"),
                ("crates/member/Cargo.lock", "member"),
            ],
        );
        let repo = crate::git::Repository::open(tmp.path()).expect("repository");
        let diff = lock_diff(
            &commit,
            &commit,
            &["Cargo.lock", "crates/member/Cargo.lock"],
        );
        let member = tmp.path().join("crates/member");

        let context =
            cargo_audit_comparison_context(&repo, &[diff], Some(&member)).expect("context");
        assert_eq!(context.base_commit_id, commit);
        assert_eq!(
            context.base_lock_path.as_deref(),
            Some("crates/member/Cargo.lock")
        );
        assert_eq!(
            context.target_lock_path.as_deref(),
            Some("crates/member/Cargo.lock")
        );
        assert_eq!(context.cargo_cwd, member);
        assert!(context.lock_changed);
    }

    #[test]
    fn baseline_context_falls_back_to_workspace_lock_for_member() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let commit = commit_fixture(
            tmp.path(),
            &[
                ("Cargo.lock", "workspace"),
                ("crates/member/Cargo.toml", "[package]\nname='member'\n"),
            ],
        );
        let repo = crate::git::Repository::open(tmp.path()).expect("repository");
        let diff = lock_diff(&commit, &commit, &["Cargo.lock"]);
        let member = tmp.path().join("crates/member");

        let context =
            cargo_audit_comparison_context(&repo, &[diff], Some(&member)).expect("context");
        assert_eq!(context.base_lock_path.as_deref(), Some("Cargo.lock"));
        assert_eq!(context.target_lock_path.as_deref(), Some("Cargo.lock"));
        assert_eq!(context.cargo_cwd, member);
        assert!(context.lock_changed);
    }

    #[test]
    fn baseline_context_keeps_the_lock_change_and_base_in_the_same_diff() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let first_base = commit_fixture(tmp.path(), &[("Cargo.lock", "shared")]);
        let second_base = commit_fixture(tmp.path(), &[("Cargo.lock", "other")]);
        let target = commit_fixture(tmp.path(), &[("Cargo.lock", "shared")]);
        let repo = crate::git::Repository::open(tmp.path()).expect("repository");
        let unchanged = lock_diff(&first_base, &target, &["Cargo.toml"]);
        let changed = lock_diff(&second_base, &target, &["Cargo.lock"]);

        let context = cargo_audit_comparison_context(&repo, &[unchanged, changed], None)
            .expect("comparison context");
        assert_eq!(context.base_commit_id, second_base);
        assert_eq!(context.base_lock_path.as_deref(), Some("Cargo.lock"));
        assert!(context.lock_changed);
    }

    #[test]
    fn baseline_context_rejects_two_changed_multi_base_locks() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let first_base = commit_fixture(tmp.path(), &[("Cargo.lock", "first")]);
        let second_base = commit_fixture(tmp.path(), &[("Cargo.lock", "second")]);
        let target = commit_fixture(tmp.path(), &[("Cargo.lock", "target")]);
        let repo = crate::git::Repository::open(tmp.path()).expect("repository");
        let first = lock_diff(&first_base, &target, &["Cargo.lock"]);
        let second = lock_diff(&second_base, &target, &["Cargo.lock"]);

        assert!(
            cargo_audit_comparison_context(&repo, &[first.clone(), second.clone()], None).is_none(),
            "one baseline must not be selected from two changed lock comparisons"
        );
        assert!(cargo_audit_lock_changed(
            Some(&repo),
            &[first, second],
            None
        ));
    }

    #[test]
    fn report_keys_reject_failed_or_incomplete_tool_output() {
        assert!(cargo_audit_report_advisory_keys("").is_none());
        assert!(cargo_audit_report_advisory_keys("cargo audit failed").is_none());
        assert!(cargo_audit_report_advisory_keys(r#"{"error":"database unavailable"}"#).is_none());

        let clean = r#"{"vulnerabilities":{"list":[]},"warnings":{}}"#;
        assert_eq!(
            cargo_audit_report_advisory_keys(clean),
            Some(Default::default())
        );
    }

    #[test]
    fn report_keys_and_findings_reject_inconsistent_structural_fields() {
        let finding = r#"{
          "advisory":{"id":"RUSTSEC-2024-0001","title":"demo"},
          "package":{"name":"demo","version":"1.2.3"},
          "versions":{}
        }"#;
        for report in [
            format!(r#"{{"vulnerabilities":{{"count":99,"list":[{finding}]}}}}"#),
            format!(r#"{{"vulnerabilities":{{"found":false,"count":1,"list":[{finding}]}}}}"#),
        ] {
            assert!(cargo_audit_report_advisory_keys(&report).is_none());
            assert!(parse_cargo_audit_findings(&report).is_empty());
        }
    }
}
