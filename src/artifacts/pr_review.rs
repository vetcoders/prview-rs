//! PR_REVIEW.md generation.

use super::*;

pub(crate) fn generate_pr_review(
    dir: &Path,
    config: &Config,
    diffs: &[Diff],
    checks: &[CheckResult],
    coverage: &CoverageDelta,
    heuristics: Option<&HeuristicsResult>,
) -> Result<()> {
    use std::collections::HashMap;
    use std::fmt::Write;

    let timestamp = chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
    let profile = config.profile.kind.as_str();
    let cargo_tree = load_cargo_tree_index(dir);

    let (target, base) = if let Some(diff) = diffs.first() {
        (diff.target.as_str(), diff.base.as_str())
    } else {
        (
            config.target.as_deref().unwrap_or("HEAD"),
            config.bases.first().map(|s| s.as_str()).unwrap_or("main"),
        )
    };

    // Count files
    let all_files: Vec<&crate::git::FileChange> = diffs.iter().flat_map(|d| &d.files).collect();
    let files_changed = all_files.len();
    let mut code_files = 0usize;
    let mut test_files = 0usize;
    let mut non_code_files = 0usize;
    for f in &all_files {
        match classify_review_file(&f.path) {
            ReviewFileCategory::Code => code_files += 1,
            ReviewFileCategory::Test => test_files += 1,
            _ => non_code_files += 1,
        }
    }

    // Commits
    let commit_count = diffs.first().map(|d| d.commits.len()).unwrap_or(0);

    // Diff stat line
    let total_adds: usize = diffs.iter().map(|d| d.stats.additions).sum();
    let total_dels: usize = diffs.iter().map(|d| d.stats.deletions).sum();
    let diff_stat_line = format!(
        "{} files changed, {} insertions(+), {} deletions(-)",
        files_changed, total_adds, total_dels
    );

    let mut md = String::new();

    // Header
    writeln!(md, "# PR Review")?;
    writeln!(md)?;
    writeln!(
        md,
        "> **Branch:** {} | **Base:** {} | **Profile:** {}",
        target, base, profile
    )?;
    writeln!(
        md,
        "> **Commits:** {} \u{2022} **Files:** {} \u{2022} **Code:** {} \u{2022} **Tests:** {} \u{2022} **Non-code:** {}",
        commit_count, files_changed, code_files, test_files, non_code_files
    )?;
    writeln!(md, "> **Generated:** {}", timestamp)?;
    writeln!(md)?;

    // Summary table
    writeln!(md, "| | |")?;
    writeln!(md, "|---|---|")?;
    writeln!(md, "| **Branch** | `{}` |", target)?;
    writeln!(md, "| **Base** | `{}` |", base)?;
    writeln!(md, "| **Generated** | {} |", timestamp)?;
    writeln!(md)?;

    writeln!(md, "## Summary")?;
    writeln!(md)?;
    writeln!(md, "| Metric | Value |")?;
    writeln!(md, "|--------|-------|")?;
    writeln!(md, "| Commits | {} |", commit_count)?;
    writeln!(md, "| Files changed | {} |", files_changed)?;
    writeln!(md, "| Diff | {} |", diff_stat_line)?;
    writeln!(md, "| Code files | {} |", code_files)?;
    writeln!(md, "| Test files | {} |", test_files)?;
    writeln!(md, "| Non-code files | {} |", non_code_files)?;
    writeln!(md)?;

    let hotspot_count = all_files
        .iter()
        .filter(|file| file.additions + file.deletions >= 80)
        .count();
    let exact_twins = heuristics
        .and_then(|h| h.loctree.as_ref())
        .map(|l| l.twins.exact_twins.len())
        .unwrap_or(0);
    let dead_parrots = heuristics
        .and_then(|h| h.loctree.as_ref())
        .map(|l| l.twins.dead_parrots.len())
        .unwrap_or(0);

    writeln!(md, "## Structural Signals")?;
    writeln!(md)?;
    writeln!(
        md,
        "- Hotspots: {} file(s) crossed the hotspot threshold (`>=80` changed lines).",
        hotspot_count
    )?;
    if let Some(diff) = diffs.first() {
        let top_hotspots: Vec<String> = diff
            .files
            .iter()
            .filter(|file| file.additions + file.deletions >= 80)
            .take(3)
            .map(|file| format!("`{}` ({})", file.path, file.additions + file.deletions))
            .collect();
        if !top_hotspots.is_empty() {
            writeln!(md, "- Top hotspots: {}", top_hotspots.join(", "))?;
        }
    }
    if let Some(h) = heuristics {
        writeln!(
            md,
            "- Loctree twins: {} exact twin pair(s) and {} unused symbol(s).",
            exact_twins, dead_parrots
        )?;

        // Show twin pair details if available
        if let Some(loctree) = &h.loctree
            && !loctree.twins.exact_twins.is_empty()
        {
            writeln!(md)?;
            writeln!(md, "  **Twin pairs (potential duplication):**")?;
            for twin in loctree.twins.exact_twins.iter().take(5) {
                writeln!(md, "  - `{}` and `{}`", twin.file_a, twin.file_b)?;
            }
            if loctree.twins.exact_twins.len() > 5 {
                writeln!(
                    md,
                    "  - ... and {} more (see `20_quality/heuristics_loctree.log`)",
                    loctree.twins.exact_twins.len() - 5
                )?;
            }
        }
    }
    writeln!(md)?;

    // Files changed
    writeln!(md, "## Files Changed")?;
    writeln!(md)?;
    writeln!(md, "<details>")?;
    writeln!(
        md,
        "<summary>Show {} changed files</summary>",
        files_changed
    )?;
    writeln!(md)?;
    writeln!(md, "```")?;
    for f in &all_files {
        let status_char = match f.status {
            crate::git::FileStatus::Added => 'A',
            crate::git::FileStatus::Modified => 'M',
            crate::git::FileStatus::Deleted => 'D',
            crate::git::FileStatus::Renamed => 'R',
            crate::git::FileStatus::Copied => 'C',
        };
        writeln!(md, "{}\t{}", status_char, f.path)?;
    }
    writeln!(md, "```")?;
    writeln!(md, "</details>")?;
    writeln!(md)?;

    // Commits — narrative summary + categorized table
    writeln!(md, "## Commits")?;
    writeln!(md)?;

    if let Some(diff) = diffs.first() {
        // Categorize commits by Conventional Commits type
        let mut by_type: HashMap<&str, Vec<&crate::git::CommitInfo>> = HashMap::new();
        for c in &diff.commits {
            let cc_type = classify_commit_type(&c.message);
            by_type.entry(cc_type).or_default().push(c);
        }

        // Narrative summary
        let mut narrative_parts: Vec<String> = Vec::new();
        for &(label, key) in &[
            ("new features", "feat"),
            ("bug fixes", "fix"),
            ("refactoring", "refactor"),
            ("documentation updates", "docs"),
            ("test additions", "test"),
            ("chore/maintenance", "chore"),
        ] {
            if let Some(commits) = by_type.get(key) {
                narrative_parts.push(format!("{} {}", commits.len(), label));
            }
        }
        let other_count: usize = by_type
            .iter()
            .filter(|(k, _)| {
                !matches!(**k, "feat" | "fix" | "refactor" | "docs" | "test" | "chore")
            })
            .map(|(_, v)| v.len())
            .sum();
        if other_count > 0 {
            narrative_parts.push(format!("{} other changes", other_count));
        }

        if !narrative_parts.is_empty() {
            writeln!(
                md,
                "This PR contains {} commits: {}.",
                commit_count,
                narrative_parts.join(", ")
            )?;
            writeln!(md)?;
        }

        // Categorized table
        writeln!(md, "| Type | Commit | Message |")?;
        writeln!(md, "|------|--------|---------|")?;
        for c in &diff.commits {
            let cc_type = classify_commit_type(&c.message);
            let msg: String = c
                .message
                .lines()
                .next()
                .unwrap_or("")
                .chars()
                .take(72)
                .collect();
            writeln!(md, "| `{}` | `{}` | {} |", cc_type, c.short_id, msg)?;
        }
        writeln!(md)?;

        // Author shortlog
        writeln!(md, "**Authors:**")?;
        writeln!(md)?;
        let mut author_counts: HashMap<&str, usize> = HashMap::new();
        for c in &diff.commits {
            *author_counts.entry(c.author.as_str()).or_default() += 1;
        }
        let mut authors: Vec<_> = author_counts.into_iter().collect();
        authors.sort_by_key(|entry| std::cmp::Reverse(entry.1));
        for (author, count) in &authors {
            writeln!(md, "- {} ({})", author, count)?;
        }
    }
    writeln!(md)?;

    // Check status
    writeln!(md, "## Check Status")?;
    writeln!(md)?;

    // In update mode, note that some checks were skipped
    if config.update_mode {
        writeln!(
            md,
            "> **Update mode:** Only essential checks were re-run. Lint, tests, and security \
             checks were skipped. See previous run for full check results.\n"
        )?;
    }

    writeln!(md, "| Check | Status |")?;
    writeln!(md, "|-------|--------|")?;

    let ran: HashMap<String, &CheckResult> =
        checks.iter().map(|c| (c.name.to_lowercase(), c)).collect();
    let all_profile_checks = crate::checks::get_checks_for_profile(config);

    for check in &all_profile_checks {
        let name = check.name();
        if let Some(result) = ran.get(&name.to_lowercase()) {
            let (icon, text) = match result.status {
                crate::checks::CheckStatus::Passed => ("\u{2705}", "Passed".to_string()),
                crate::checks::CheckStatus::Failed => ("\u{274c}", "Failed".to_string()),
                crate::checks::CheckStatus::Warnings => {
                    ("\u{26a0}\u{fe0f}", "Warnings".to_string())
                }
                crate::checks::CheckStatus::Error => ("\u{274c}", "Error".to_string()),
                crate::checks::CheckStatus::Skipped => ("\u{23ed}\u{fe0f}", "Skipped".to_string()),
            };
            writeln!(md, "| {} | {} {} |", name, icon, text)?;
        } else {
            let reason = match check.check_eligibility(config) {
                crate::checks::CheckEligibility::Skip(r) => r,
                crate::checks::CheckEligibility::Run => "unknown skip reason".to_string(),
            };
            writeln!(md, "| {} | \u{23ed}\u{fe0f} Skipped ({}) |", name, reason)?;
        }
    }
    writeln!(md)?;

    // Failed details
    let failures: Vec<&CheckResult> = checks.iter().filter(|c| c.is_failure()).collect();
    if !failures.is_empty() {
        writeln!(md, "## Review Findings")?;
        writeln!(md)?;
        writeln!(
            md,
            "_Compact reviewer summary. Full logs remain in `20_quality/full-checks.log`._"
        )?;
        writeln!(md)?;

        for check in &failures {
            writeln!(md, "### {}", check.name)?;
            writeln!(md)?;
            writeln!(md, "- Status: {}", check.status.as_str())?;
            writeln!(
                md,
                "- Log: `20_quality/{}.log`",
                check_id_from_name(&check.name)
            )?;

            if let Some(rc) = extract_root_cause(check) {
                writeln!(md, "- Summary: {}", rc.cause)?;
                if !rc.evidence.is_empty() {
                    writeln!(md, "- Evidence: {}", rc.evidence)?;
                }
                writeln!(md, "- Action: {}", rc.hint)?;
            }

            if check.name.eq_ignore_ascii_case("cargo audit") {
                let findings = parse_cargo_audit_findings(&check.output);
                if !findings.is_empty() {
                    writeln!(
                        md,
                        "- Review call: dependency security issue in `{}`.",
                        cargo_audit_best_location()
                    )?;
                    writeln!(
                        md,
                        "- Advisory summary: {} ({})",
                        cargo_audit_summary_cause(&findings),
                        cargo_audit_advisory_ids(&findings, 3)
                    )?;
                    let dependency_paths: Vec<String> = findings
                        .iter()
                        .flat_map(|finding| {
                            cargo_tree
                                .as_ref()
                                .into_iter()
                                .flat_map(|tree| tree.paths_for(finding, 1))
                        })
                        .take(3)
                        .collect();
                    if !dependency_paths.is_empty() {
                        writeln!(
                            md,
                            "- Dependency paths: {}",
                            dependency_paths
                                .iter()
                                .map(|path| format!("`{path}`"))
                                .collect::<Vec<_>>()
                                .join("; ")
                        )?;
                    }
                    writeln!(md, "- Full advisory list: `00_summary/FAILURES_SUMMARY.md`")?;
                    writeln!(
                        md,
                        "- Per-advisory SARIF: `30_context/INLINE_FINDINGS.sarif`"
                    )?;
                    writeln!(
                        md,
                        "- Detailed advisory breakdown is intentionally deduplicated here; use the artifacts above for full per-advisory context."
                    )?;
                }
            }

            writeln!(md)?;
        }
    }

    let quick_wins = collect_quick_wins(config, checks, exact_twins);
    if !quick_wins.is_empty() {
        writeln!(md, "## Quick Wins")?;
        writeln!(md)?;
        for win in &quick_wins {
            writeln!(md, "- {}", win)?;
        }
        writeln!(md)?;
    }

    // Warnings
    let mut warnings: Vec<String> = Vec::new();
    // Low test ratio: many code changes, very few test changes
    if code_files > 10 && test_files < 2 {
        warnings.push(format!(
            "Low test ratio: {} code files changed but only {} test file(s). Consider adding tests for new/modified code.",
            code_files, test_files
        ));
    }
    if coverage.total_source > 0 && coverage.pct < 80 {
        warnings.push(format!(
            "Coverage review signal: {}% heuristic coverage ({}/{})",
            coverage.pct, coverage.covered_count, coverage.total_source
        ));
    } else if code_files > 0 && test_files == 0 {
        warnings.push(format!(
            "Coverage alert: {} code files changed without test changes",
            code_files
        ));
    }
    let warning_checks: Vec<&CheckResult> = checks
        .iter()
        .filter(|c| matches!(c.status, crate::checks::CheckStatus::Warnings))
        .collect();
    for wc in &warning_checks {
        if wc.name.eq_ignore_ascii_case("cargo audit") {
            // Extract advisory details instead of generic "produced warnings"
            let findings = parse_cargo_audit_findings(&wc.output);
            if !findings.is_empty() {
                let ids = cargo_audit_advisory_ids(&findings, 5);
                warnings.push(format!(
                    "Cargo audit: {} advisory/ies ({}) — see `20_quality/cargo_audit.log`",
                    findings.len(),
                    ids
                ));
            } else {
                warnings.push(format!("{} produced warnings", wc.name));
            }
        } else if wc.name.eq_ignore_ascii_case("cargo geiger") {
            // Extract unsafe usage stats from geiger output
            let unsafe_summary = extract_geiger_summary(&wc.output);
            warnings.push(format!(
                "Cargo geiger: {} — see `20_quality/cargo_geiger.log`",
                unsafe_summary
            ));
        } else {
            warnings.push(format!("{} produced warnings", wc.name));
        }
    }

    // Surface cargo audit informational advisories (unmaintained, unsound, notice)
    // even when the check status is Passed (no actionable vulnerabilities found).
    if let Some(audit_check) = checks
        .iter()
        .find(|c| c.name.eq_ignore_ascii_case("cargo audit"))
        && let Some(info_summary) = cargo_audit_informational_summary(&audit_check.output)
    {
        warnings.push(format!(
            "Cargo audit note: {} (check `20_quality/cargo_audit.log` for details)",
            info_summary
        ));
    }

    if coverage_has_rust_inline_test_blind_spot(coverage) {
        warnings.push(
            "Rust caveat: coverage heuristic may miss inline `#[cfg(test)]` modules inside changed `.rs` files.".to_string(),
        );
    }

    if !warnings.is_empty() {
        writeln!(md, "## Warnings")?;
        writeln!(md)?;
        for w in &warnings {
            writeln!(md, "- {}", w)?;
        }
        writeln!(md)?;
    }

    writeln!(md, "---")?;
    writeln!(md)?;

    // PR template
    writeln!(md, "## PR Template")?;
    writeln!(md)?;
    writeln!(md, "_Copy below for GitHub PR description:_")?;
    writeln!(md)?;
    writeln!(md, "```markdown")?;
    writeln!(md, "## Description")?;
    writeln!(md, "<!-- Describe your changes -->")?;
    writeln!(md, "## Type of Change")?;
    writeln!(md, "- [ ] Bug fix")?;
    writeln!(md, "- [ ] New feature")?;
    writeln!(md, "- [ ] Breaking change")?;
    writeln!(md, "- [ ] Refactoring")?;
    writeln!(md, "## Checklist")?;

    // Auto-check based on check results
    let has_tsc = checks.iter().any(|c| {
        c.name.to_lowercase().contains("typescript") || c.name.to_lowercase() == "cargo check"
    });
    let tsc_pass = checks.iter().any(|c| {
        (c.name.to_lowercase().contains("typescript") || c.name.to_lowercase() == "cargo check")
            && matches!(c.status, crate::checks::CheckStatus::Passed)
    });
    let tests_pass = checks.iter().any(|c| {
        (c.name.to_lowercase().contains("test")
            || c.name.to_lowercase() == "vitest"
            || c.name.to_lowercase() == "pytest")
            && matches!(c.status, crate::checks::CheckStatus::Passed)
    });
    let lint_pass = checks.iter().any(|c| {
        (c.name.to_lowercase().contains("lint")
            || c.name.to_lowercase() == "clippy"
            || c.name.to_lowercase() == "ruff")
            && matches!(c.status, crate::checks::CheckStatus::Passed)
    });

    let check_mark = |pass: bool| if pass { "x" } else { " " };

    writeln!(
        md,
        "- [{}] Compiles / type-checks",
        check_mark(tsc_pass || (!has_tsc && checks.is_empty()))
    )?;
    writeln!(md, "- [{}] Tests pass", check_mark(tests_pass))?;
    writeln!(md, "- [{}] No lint errors", check_mark(lint_pass))?;
    writeln!(md, "- [ ] Manually tested")?;
    writeln!(md, "```")?;

    fs::write(dir.join("PR_REVIEW.md"), md)?;
    Ok(())
}
