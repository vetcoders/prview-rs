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
///
/// The key set is TOTAL over the report: every vulnerability entry is keyed,
/// and exactly as many `warnings` items are keyed as the check status counts
/// ([`crate::checks::count_cargo_audit_warning_items`]), or the report is
/// unreadable (`None`). Skipping an item the counter counts is how a change
/// introduced a warning the pre-existing comparison could not see — the
/// remaining vulnerability row then downgraded the failed audit on its own. A
/// counted shape that carries no list of entries (a `count` field, a nested
/// object, a bare `true`) has nothing to key, so the counts disagree and the
/// report is unreadable.
///
/// * A `yanked` item carries no advisory at all (`"advisory": null`): a
///   yanked release is a fact about the package, not an advisory. Its identity
///   is the category plus the locked package, so it is keyed as `yanked` —
///   stable across the base and target reports, and one key per yanked
///   package version, which is exactly what the counter counts.
/// * Any other item — a vulnerability or a warning — without an advisory id,
///   or without its locked package name and version, cannot be keyed without
///   inventing a sentinel. A sentinel collapses distinct items into one key, so
///   a malformed advisory in the target would match an unrelated malformed one
///   in the base and read as pre-existing. It makes the whole report
///   unreadable, which the gate treats as causation unknown.
/// * Two items that share a key would leave the set smaller than the report,
///   with nothing to say which of them the base's copy of that key accounts
///   for. The key names no package source, and rustsec needs none to tell
///   genuine items apart: it matches vulnerabilities and warnings against
///   default-registry packages only, so a git or alternate-registry package of
///   the same name and version is never reported. Its yanked check does accept
///   both spellings of the crates.io index, though, so a lockfile listing one
///   version under each would repeat a `yanked` key. A repeated key of any kind
///   makes the report unreadable rather than silently shorter.
pub(crate) fn cargo_audit_report_advisory_keys(
    output: &str,
) -> Option<std::collections::HashSet<(String, String, String)>> {
    let parsed = extract_embedded_json(output)?;

    let mut keys = std::collections::HashSet::new();
    for entry in crate::checks::validated_cargo_audit_vulnerability_list(&parsed)? {
        if !keys.insert(cargo_audit_item_key(entry, None)?) {
            return None;
        }
    }
    let Some(warnings) = parsed.get("warnings") else {
        return Some(keys);
    };
    let mut keyed = 0;
    if let Some(categories) = warnings.as_object() {
        for (category, value) in categories {
            // Only a list has entries to key; whether the counter saw items
            // anywhere else is settled by the count comparison below.
            let Some(entries) = value.as_array() else {
                continue;
            };
            for entry in entries {
                if !keys.insert(cargo_audit_item_key(entry, Some(category.as_str()))?) {
                    return None;
                }
                keyed += 1;
            }
        }
    }
    (keyed == crate::checks::count_cargo_audit_warning_items(warnings)).then_some(keys)
}

/// One report item's identity, `(advisory id, package, locked version)`, read
/// from the fields cargo-audit emits — the same fields
/// [`parse_cargo_audit_findings`] reads, so a well-formed row's
/// [`cargo_audit_finding_key`] is this key. `category` is the `warnings` family
/// the item is listed under, `None` for a vulnerability. `None` when a field is
/// missing: see [`cargo_audit_report_advisory_keys`] for why no sentinel stands
/// in for it.
fn cargo_audit_item_key(
    entry: &serde_json::Value,
    category: Option<&str>,
) -> Option<(String, String, String)> {
    let package_name = entry.pointer("/package/name")?.as_str()?;
    let package_version = entry.pointer("/package/version")?.as_str()?;
    let advisory_id = match entry.get("advisory") {
        Some(advisory) if !advisory.is_null() => advisory.get("id")?.as_str()?,
        _ if category == Some("yanked") => "yanked",
        _ => return None,
    };
    Some((
        advisory_id.to_string(),
        package_name.to_string(),
        package_version.to_string(),
    ))
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CargoAuditComparisonContext {
    base_commit_id: String,
    /// The lockfile governing `cargo_root` in the base, with the workspace-root
    /// fallback. It answers "did the relevant lock change?", nothing more.
    base_lock_path: Option<String>,
    target_lock_path: Option<String>,
    /// The base's copy of the one lockfile the target audit READ
    /// ([`cargo_audit_lock_path`]) — the only base file whose advisories are
    /// comparable with that audit. `None` when the base has no such file.
    ///
    /// Kept apart from `base_lock_path` because the two questions differ. A
    /// member that gains its own `Cargo.lock` is audited against that file,
    /// while the base resolves to the repository-root lock by fallback; that
    /// root lock is a superset of every member's resolution (or an unrelated
    /// file outright), so a vulnerable tuple found there proves nothing about
    /// the member. Comparing against it classified an advisory the member lock
    /// introduced as out-of-diff, and the failed audit downgraded to PASS.
    comparable_base_lock_path: Option<String>,
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

/// The repository-relative `Cargo.lock` recorded IN `commit_id`'s tree at
/// `cargo_root` itself — the one file a `cargo audit` run there reads.
///
/// `Some(Some(path))` is "the target tree carries this lockfile", `Some(None)`
/// is "the target tree carries none", and `None` is "the question could not be
/// answered". Only the first licenses cargo audit's provenance proof: `cargo
/// audit` GENERATES a lockfile when none is present (measured on cargo-audit
/// 0.22.2), so a run in a lock-less tree reports real advisories against a
/// lockfile that exists in no commit at all. Naming that "pre-existing:
/// Cargo.lock unchanged by this PR" would assert a fact about a file the
/// target does not have.
///
/// There is deliberately no workspace-root fallback here, unlike
/// [`effective_cargo_lock_path_at_commit`]. `cargo audit` opens `Cargo.lock`
/// relative to the directory it runs in and never walks up to a workspace root;
/// when that file is absent beside a `Cargo.toml` it runs `cargo update
/// --workspace` instead (cargo-audit 0.22, `lockfile::locate_or_generate`). A
/// root lock beside a lock-less member is therefore a file the audit did not
/// read, and it proves nothing about the one it did. The member lock is also
/// held to [`crate::git::Repository::regular_file_at_commit`]: a symlink or a
/// gitlink committed in its place is not a committed lockfile.
///
/// A `cargo_root` outside the repository is treated as "no lockfile in the
/// target tree": nothing inside the reviewed tree can vouch for what was
/// scanned, which is the same conclusion by a different route.
pub(crate) fn cargo_audit_lock_path_in_commit(
    repo: &crate::git::Repository,
    commit_id: &str,
    repo_root: &std::path::Path,
    cargo_root: Option<&std::path::Path>,
) -> Option<Option<String>> {
    let Some(lock) = cargo_audit_lock_path(repo_root, cargo_root) else {
        return Some(None);
    };
    match repo.regular_file_at_commit(commit_id, &lock) {
        Ok(true) => Some(Some(lock)),
        Ok(false) => Some(None),
        Err(_) => None,
    }
}

/// The repository-relative path of the `Cargo.lock` a `cargo audit` run in
/// `cargo_root` reads — `Cargo.lock` in that directory, and only there (see
/// [`cargo_audit_lock_path_in_commit`]).
///
/// `None` for a `cargo_root` outside the repository: no path inside the
/// reviewed tree names the lockfile that was scanned.
pub(crate) fn cargo_audit_lock_path(
    repo_root: &std::path::Path,
    cargo_root: Option<&std::path::Path>,
) -> Option<String> {
    cargo_root_file_path(repo_root, cargo_root, "Cargo.lock")
}

/// The repository-relative path of the configuration a `cargo audit` run in
/// `cargo_root` discovers: `.cargo/audit.toml` in that directory, and only
/// there. cargo-audit reads `./.cargo/audit.toml` relative to its working
/// directory without walking up, and otherwise falls back to
/// `audit.toml` in the Cargo home, `CARGO_HOME` or else `$HOME/.cargo`
/// (`CargoAuditCommand::config_path` upstream). That fallback lies outside the
/// reviewed tree unless the home is relative or points into the checkout or
/// the scanned tree, which the lock proof refuses on its own
/// ([`crate::artifacts::verdict::LockProofGap::RelativeCargoHome`],
/// [`crate::artifacts::verdict::LockProofGap::InTreeCargoHome`]).
///
/// `None` for a `cargo_root` outside the repository, as for the lockfile.
pub(crate) fn cargo_audit_config_path(
    repo_root: &std::path::Path,
    cargo_root: Option<&std::path::Path>,
) -> Option<String> {
    cargo_root_file_path(repo_root, cargo_root, ".cargo/audit.toml")
}

fn cargo_root_file_path(
    repo_root: &std::path::Path,
    cargo_root: Option<&std::path::Path>,
    file: &str,
) -> Option<String> {
    let configured_root = cargo_root.unwrap_or(repo_root);
    let normalized =
        crate::paths::normalize_to_repo_relative(&configured_root.display().to_string(), repo_root);
    if normalized.is_external {
        return None;
    }
    let relative_root = std::path::Path::new(&normalized.display);
    if relative_root == std::path::Path::new(".") {
        return Some(file.to_string());
    }
    Some(
        relative_root
            .join(file)
            .to_string_lossy()
            .replace('\\', "/"),
    )
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
    let audited_lock_path = cargo_audit_lock_path(repo.path(), cargo_root);
    let comparable_base_lock_path = base_lock_path
        .clone()
        .filter(|path| audited_lock_path.as_deref() == Some(path.as_str()));

    Some(CargoAuditComparisonContext {
        base_commit_id: diff.base_commit_id.clone(),
        base_lock_path,
        target_lock_path,
        comparable_base_lock_path,
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

/// The bytes a baseline `cargo audit` would read — the base revision's copy of
/// the audited lockfile — and the directory it runs in. `None` when the
/// relevant lock did not change (no baseline is needed) or the base has no
/// comparable copy of it (the baseline is unavailable).
fn base_cargo_audit_input(
    repo: &crate::git::Repository,
    diffs: &[crate::git::Diff],
    cargo_root: Option<&std::path::Path>,
) -> Option<(std::path::PathBuf, String)> {
    let context = cargo_audit_comparison_context(repo, diffs, cargo_root)?;
    if !context.lock_changed {
        return None;
    }
    // Only the base's copy of the lockfile the audit read is comparable with
    // it; a base that has none leaves the baseline unavailable.
    let cargo_lock_path = context.comparable_base_lock_path?;
    let base_content = repo
        .file_at_commit(&context.base_commit_id, &cargo_lock_path)
        .ok()?;
    Some((context.cargo_cwd, base_content))
}

pub(crate) fn get_base_cargo_audit_findings(
    repo: Option<&crate::git::Repository>,
    diffs: &[crate::git::Diff],
    cargo_root: Option<&std::path::Path>,
) -> anyhow::Result<Option<std::collections::HashSet<(String, String, String)>>> {
    use std::process::Command;

    let Some(repo) = repo else {
        return Ok(None);
    };
    let Some((cargo_cwd, base_content)) = base_cargo_audit_input(repo, diffs, cargo_root) else {
        return Ok(None);
    };

    // cargo-audit explicitly documents `-` as the stdin sentinel for --file.
    // Keeping the historical lock out of the target worktree avoids making the
    // baseline comparison mutate or materialise a second checkout.
    let mut command = Command::new("cargo");
    command
        .args(["audit", "--json", "-n", "-q", "-f", "-"])
        .current_dir(cargo_cwd);
    let output = match crate::proc::output_governed_with_input_timeout(
        command,
        "cargo audit baseline",
        base_content.as_bytes(),
        std::time::Duration::from_secs(120),
    ) {
        Ok(output) => output,
        Err(error) if crate::governor::is_cancellation(&error) => return Err(error),
        Err(_) => return Ok(None),
    };
    let Ok(out_str) = String::from_utf8(output.stdout) else {
        return Ok(None);
    };
    Ok(cargo_audit_report_advisory_keys(&out_str))
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

    /// cargo-audit reports a yanked release with `"advisory": null` — there is
    /// no advisory, only the package. It used to be skipped for lacking
    /// `/advisory/id`, while the check status still counted it, so a change
    /// that added a yanked crate beside a pre-existing vulnerability downgraded
    /// the failed audit as if nothing had been introduced.
    #[test]
    fn a_yanked_package_is_keyed_by_its_category() {
        for entry in [
            r#"{"kind":"yanked","package":{"name":"shiny","version":"2.0.0"},"advisory":null}"#,
            r#"{"kind":"yanked","package":{"name":"shiny","version":"2.0.0"}}"#,
        ] {
            let output =
                format!(r#"{{"vulnerabilities":{{"list":[]}},"warnings":{{"yanked":[{entry}]}}}}"#);
            let keys = cargo_audit_report_advisory_keys(&output).expect("valid report");
            assert_eq!(
                keys,
                std::iter::once((
                    "yanked".to_string(),
                    "shiny".to_string(),
                    "2.0.0".to_string()
                ))
                .collect::<std::collections::HashSet<_>>(),
                "{entry}",
            );
        }
    }

    /// An item the check status counts but the key set cannot hold makes the
    /// report unreadable. Skipping it silently hid it from the pre-existing
    /// comparison; keying it by a sentinel (`cargo-audit`, `unknown`) would
    /// collapse distinct items into one key. Neither may happen.
    #[test]
    fn unkeyable_informational_entries_make_the_report_unreadable() {
        for warnings in [
            // A locked package without its version.
            r#"{"unmaintained":[{"advisory":{"id":"RUSTSEC-2024-0001"},"package":{"name":"demo"}}]}"#,
            // An advisory-bearing category without an advisory id.
            r#"{"unmaintained":[{"package":{"name":"demo","version":"1.2.3"}}]}"#,
            r#"{"unsound":[{"advisory":null,"package":{"name":"demo","version":"1.2.3"}}]}"#,
            // Counted shapes that carry no keyable entries.
            r#"{"notice":{"count":1}}"#,
            r#"{"unmaintained":{"list":[{"advisory":{"id":"RUSTSEC-2024-0001"},"package":{"name":"demo","version":"1.2.3"}}]}}"#,
            r#"[{"package":{"name":"demo","version":"1.2.3"}}]"#,
            // A `count` directly on `warnings` is what the counter reads there.
            r#"{"count":2}"#,
            r#"{"count":0,"yanked":[{"package":{"name":"shiny","version":"2.0.0"},"advisory":null}]}"#,
        ] {
            let output = format!(r#"{{"vulnerabilities":{{"list":[]}},"warnings":{warnings}}}"#);
            assert!(
                cargo_audit_report_advisory_keys(&output).is_none(),
                "{warnings} is counted by the check status but cannot be keyed",
            );
        }

        // Control: shapes the counter counts as zero stay a readable, clean
        // report.
        for warnings in [
            r#"{}"#,
            r#"{"yanked":[],"notice":{}}"#,
            r#"[]"#,
            r#"{"count":0}"#,
        ] {
            let output = format!(r#"{{"vulnerabilities":{{"list":[]}},"warnings":{warnings}}}"#);
            assert_eq!(
                cargo_audit_report_advisory_keys(&output),
                Some(std::collections::HashSet::new()),
                "{warnings}",
            );
        }
    }

    /// The vulnerability half of the key set follows the same rule. Its keys
    /// used to come from the rendered findings, whose missing fields read as
    /// `cargo-audit` / `unknown-package` / `unknown`: two different malformed
    /// advisories then shared one key, so a malformed one the change
    /// introduced matched an unrelated malformed one in the base and read as
    /// pre-existing.
    #[test]
    fn unkeyable_vulnerability_entries_make_the_report_unreadable() {
        fn report(entries: &[&str]) -> String {
            format!(
                r#"{{"vulnerabilities":{{"found":true,"count":{},"list":[{}]}},"warnings":{{}}}}"#,
                entries.len(),
                entries.join(",")
            )
        }
        // Both used to key as ("cargo-audit", "unknown-package", "unknown").
        let base = report(&[r#"{"advisory":{"title":"one"}}"#]);
        let target = report(&[r#"{"advisory":{"title":"two"}}"#]);
        assert!(cargo_audit_report_advisory_keys(&base).is_none());
        assert!(
            cargo_audit_report_advisory_keys(&target).is_none(),
            "distinct malformed advisories must not collapse into one pre-existing key"
        );
        for entry in [
            r#"{"advisory":{"title":"no id"},"package":{"name":"demo","version":"1.2.3"}}"#,
            r#"{"advisory":null,"package":{"name":"demo","version":"1.2.3"}}"#,
            r#"{"advisory":{"id":"RUSTSEC-2024-0001"},"package":{"name":"demo"}}"#,
            r#"{"advisory":{"id":"RUSTSEC-2024-0001"}}"#,
        ] {
            assert!(
                cargo_audit_report_advisory_keys(&report(&[entry])).is_none(),
                "{entry}"
            );
        }

        // Control: a well-formed vulnerability is keyed exactly as its row is.
        let valid = report(&[
            r#"{"advisory":{"id":"RUSTSEC-2024-0001"},"package":{"name":"demo","version":"1.2.3"}}"#,
        ]);
        let rows = parse_cargo_audit_findings(&valid);
        assert_eq!(rows.len(), 1);
        assert_eq!(
            cargo_audit_report_advisory_keys(&valid),
            Some(
                std::iter::once(cargo_audit_finding_key(&rows[0]))
                    .collect::<std::collections::HashSet<_>>()
            ),
        );
    }

    /// The key names no package source, so two items for one name and version
    /// from different sources share it. Collapsed into one key, the report
    /// counts one item fewer than it lists, and a base report holding either
    /// of them accounts for both. rustsec reports vulnerabilities and warnings
    /// for default-registry packages only, but its yanked check accepts both
    /// spellings of the crates.io index, and a report is not to be trusted to
    /// be one rustsec wrote. A repeated key makes the report unreadable.
    #[test]
    fn items_that_share_a_key_make_the_report_unreadable() {
        let vulnerability = |source: &str| {
            format!(
                r#"{{"advisory":{{"id":"RUSTSEC-2024-0001"}},"package":{{"name":"demo","version":"1.2.3","source":"{source}"}}}}"#
            )
        };
        let registry = vulnerability("registry+https://github.com/rust-lang/crates.io-index");
        let git = vulnerability("git+https://example.com/demo#0123456789abcdef");
        let repeated = format!(
            r#"{{"vulnerabilities":{{"found":true,"count":2,"list":[{registry},{git}]}},"warnings":{{}}}}"#
        );
        assert!(
            cargo_audit_report_advisory_keys(&repeated).is_none(),
            "two sources of one vulnerable version"
        );

        let yanked = |source: &str| {
            format!(
                r#"{{"kind":"yanked","advisory":null,"package":{{"name":"shiny","version":"2.0.0","source":"{source}"}}}}"#
            )
        };
        let both_spellings = format!(
            r#"{{"vulnerabilities":{{"list":[]}},"warnings":{{"yanked":[{},{}]}}}}"#,
            yanked("registry+https://github.com/rust-lang/crates.io-index"),
            yanked("sparse+https://index.crates.io/"),
        );
        assert!(
            cargo_audit_report_advisory_keys(&both_spellings).is_none(),
            "one yanked version under both crates.io index spellings"
        );

        let across_categories = format!(
            r#"{{"vulnerabilities":{{"list":[]}},"warnings":{{"unmaintained":[{registry}],"unsound":[{registry}]}}}}"#
        );
        assert!(
            cargo_audit_report_advisory_keys(&across_categories).is_none(),
            "one advisory repeated across warning categories"
        );

        // Control: the same advisory for two versions keys twice.
        let other_version = registry.replace("1.2.3", "1.2.4");
        let distinct = format!(
            r#"{{"vulnerabilities":{{"found":true,"count":2,"list":[{registry},{other_version}]}},"warnings":{{}}}}"#
        );
        assert_eq!(
            cargo_audit_report_advisory_keys(&distinct).map(|keys| keys.len()),
            Some(2)
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
            cargo_audit_comparison_context(&repo, std::slice::from_ref(&diff), Some(&member))
                .expect("context");
        assert_eq!(context.base_commit_id, commit);
        assert_eq!(
            context.base_lock_path.as_deref(),
            Some("crates/member/Cargo.lock")
        );
        assert_eq!(
            context.target_lock_path.as_deref(),
            Some("crates/member/Cargo.lock")
        );
        assert_eq!(
            context.comparable_base_lock_path.as_deref(),
            Some("crates/member/Cargo.lock"),
            "the base carries the lockfile the audit reads, so it is comparable"
        );
        assert_eq!(context.cargo_cwd, member);
        assert!(context.lock_changed);
        assert_eq!(
            base_cargo_audit_input(&repo, &[diff], Some(&member)),
            Some((member.clone(), "member".to_string())),
            "the baseline audit reads the member lock's base bytes, in the member"
        );
    }

    /// The workspace-root fallback still decides whether the relevant lock
    /// changed, but the root lock is not the file an audit run in the member
    /// reads, so it is never the comparison baseline.
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
        assert_eq!(context.comparable_base_lock_path, None);
        assert_eq!(context.cargo_cwd, member);
        assert!(context.lock_changed);
    }

    /// A member that gains its own `Cargo.lock` is audited against that file,
    /// while its base resolves to the repository-root lock by fallback. The
    /// root lock holds every member's resolution, so a vulnerable tuple in it
    /// proves nothing about this member: comparing against it classified an
    /// advisory the new member lock introduced as out-of-diff, and the failed
    /// audit downgraded to PASS. The base has no copy of the audited file, so
    /// the baseline is unavailable.
    #[test]
    fn a_member_lock_new_in_the_target_is_not_compared_with_the_root_lock() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let base = commit_fixture(
            tmp.path(),
            &[
                ("Cargo.lock", "workspace"),
                ("crates/member/Cargo.toml", "[package]\nname='member'\n"),
            ],
        );
        let target = commit_fixture(tmp.path(), &[("crates/member/Cargo.lock", "member")]);
        let repo = crate::git::Repository::open(tmp.path()).expect("repository");
        let diff = lock_diff(&base, &target, &["crates/member/Cargo.lock"]);
        let member = tmp.path().join("crates/member");

        let context =
            cargo_audit_comparison_context(&repo, std::slice::from_ref(&diff), Some(&member))
                .expect("context");
        assert_eq!(context.base_lock_path.as_deref(), Some("Cargo.lock"));
        assert_eq!(
            context.target_lock_path.as_deref(),
            Some("crates/member/Cargo.lock")
        );
        assert!(
            context.lock_changed,
            "the audited lock is new, so it changed"
        );
        assert_eq!(
            context.comparable_base_lock_path, None,
            "the base has no copy of the lockfile the audit read"
        );

        // No base bytes are selected for a baseline audit: the root lock is
        // never fed to it. (A spawned audit of the fixture lock would also
        // come back empty, so this checks the selection, not the tool.)
        assert_eq!(
            base_cargo_audit_input(&repo, &[diff], Some(&member)),
            None,
            "the root lock must not stand in for the member's audited lock"
        );
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
