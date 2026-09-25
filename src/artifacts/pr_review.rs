//! PR_REVIEW.md generation.

use super::*;

/// Stated verbatim when the pack reviewed a rewritten range.
///
/// This is a statement about range semantics, not a warning and not a review
/// caveat. It explains correct behaviour: with `--exact-base` the reviewed file
/// set is the literal tree difference between the pinned base and the target,
/// and a force-push makes that base a commit the target's history never passed
/// through — so a file can appear in the file list without any commit in the
/// commit list having touched it. It is deliberately kept out of the caveat
/// machinery: it must never reach the verdict, the gate disposition or any
/// quality signal.
pub(crate) const REWRITTEN_RANGE_NOTE: &str = "Force-push detected: the file set reflects the pre-push \u{2192} current tree difference, so it may contain changes not attributable to any commit in the displayed commit list.";

/// One auto-derived line of the PR template checklist.
///
/// Each line is a UNIVERSAL claim ("No lint errors", not "some linter
/// passed"), so it is ticked only when at least one check of its category
/// executed in this review and every executed check of that category passed.
/// Cached PASS replays do not count as execution; cached failures still veto
/// a universal success claim. Anything else —
/// a failed, errored or warnings-only check, or no executed check at all —
/// renders `[ ]`: the pack does not claim what it did not prove. The failing
/// check is named in the same file's Check Status table, so the template line
/// itself stays in its copy-paste shape.
///
/// This is the single derivation of the claim. `PR_REVIEW.md` renders it, and
/// the consistency checker re-derives it to prove the rendered marks still
/// match: `CONSISTENCY_CHECK.json` from the statuses serialized in
/// `report.json`, and `report.json`'s own `quality.consistency` from the
/// in-memory statuses it is about to serialize.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PrChecklistItem {
    Compiles,
    TestsPass,
    NoLintErrors,
}

impl PrChecklistItem {
    pub(crate) const ALL: [Self; 3] = [Self::Compiles, Self::TestsPass, Self::NoLintErrors];

    /// The text rendered after the checkbox.
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Compiles => "Compiles / type-checks",
            Self::TestsPass => "Tests pass",
            Self::NoLintErrors => "No lint errors",
        }
    }

    /// Stable field key used by the consistency checker.
    pub(crate) fn key(self) -> &'static str {
        match self {
            Self::Compiles => "compiles",
            Self::TestsPass => "tests_pass",
            Self::NoLintErrors => "no_lint_errors",
        }
    }

    /// Whether a check (by display name) belongs to this item's category.
    fn covers(self, check_name: &str) -> bool {
        let name = check_name.to_lowercase();
        match self {
            Self::Compiles => {
                name.contains("typescript") || name == "cargo check" || name == "mypy"
            }
            Self::TestsPass => name.contains("test") || name == "vitest" || name == "pytest",
            Self::NoLintErrors => name.contains("lint") || name == "clippy" || name == "ruff",
        }
    }
}

/// What a check contributes to a checklist claim.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ChecklistCheckOutcome {
    Passed,
    /// Ran and did not pass: failed, errored, or finished with warnings.
    NotPassed,
    /// A replay did not execute in this review and cannot earn a claim.
    CachedPassed,
    /// A replayed failure still vetoes a universal success claim.
    CachedNotPassed,
    /// Did not execute; neither supports nor vetoes the claim.
    Skipped,
}

impl ChecklistCheckOutcome {
    pub(crate) fn from_status(status: crate::checks::CheckStatus, cached: bool) -> Self {
        use crate::checks::CheckStatus;
        match (status, cached) {
            (CheckStatus::Passed, false) => Self::Passed,
            (CheckStatus::Passed, true) => Self::CachedPassed,
            (CheckStatus::Skipped, _) => Self::Skipped,
            (CheckStatus::Failed | CheckStatus::Warnings | CheckStatus::Error, false) => {
                Self::NotPassed
            }
            (CheckStatus::Failed | CheckStatus::Warnings | CheckStatus::Error, true) => {
                Self::CachedNotPassed
            }
        }
    }

    /// The status token `report.json` serializes (`PASS`/`FAIL`/`ERROR`/`SKIP`/
    /// `WARN`, a closed vocabulary). Any other token is evidence of neither
    /// outcome: `None`, which the consistency checker reports as an unreadable
    /// entry instead of reading it as a failure that might happen to agree.
    pub(crate) fn from_report_status(status: &str, cached: bool) -> Option<Self> {
        match (status, cached) {
            ("PASS", false) => Some(Self::Passed),
            ("PASS", true) => Some(Self::CachedPassed),
            ("SKIP", _) => Some(Self::Skipped),
            ("FAIL" | "ERROR" | "WARN", false) => Some(Self::NotPassed),
            ("FAIL" | "ERROR" | "WARN", true) => Some(Self::CachedNotPassed),
            _ => None,
        }
    }
}

/// A derived checklist claim and the checks behind it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PrChecklistClaim {
    pub(crate) item: PrChecklistItem,
    /// Executed checks of this category.
    pub(crate) executed: Vec<String>,
    /// Executed checks of this category that did not pass.
    pub(crate) not_passed: Vec<String>,
}

impl PrChecklistClaim {
    pub(crate) fn ticked(&self) -> bool {
        !self.executed.is_empty() && self.not_passed.is_empty()
    }
}

/// Derive every checklist claim from `(check name, outcome)` pairs.
pub(crate) fn derive_pr_checklist(
    checks: &[(&str, ChecklistCheckOutcome)],
) -> Vec<PrChecklistClaim> {
    PrChecklistItem::ALL
        .into_iter()
        .map(|item| {
            let mut claim = PrChecklistClaim {
                item,
                executed: Vec::new(),
                not_passed: Vec::new(),
            };
            for (name, outcome) in checks.iter().filter(|(name, _)| item.covers(name)) {
                match outcome {
                    ChecklistCheckOutcome::Skipped => {}
                    ChecklistCheckOutcome::Passed => claim.executed.push((*name).to_string()),
                    ChecklistCheckOutcome::CachedPassed => {}
                    ChecklistCheckOutcome::NotPassed => {
                        claim.executed.push((*name).to_string());
                        claim.not_passed.push((*name).to_string());
                    }
                    ChecklistCheckOutcome::CachedNotPassed => {
                        claim.not_passed.push((*name).to_string());
                    }
                }
            }
            claim
        })
        .collect()
}

/// Read the rendered checklist marks back from a `PR_REVIEW.md`.
///
/// Only lines inside the PR Template's `## Checklist` section count: the
/// `## Checklist` heading inside the ```` ```markdown ```` block that follows
/// the final generated `## PR Template` heading and its preceding separator, up to
/// the next level-2 heading or the block's closing fence. A `## Checklist`
/// anywhere else — check-derived text above
/// the template, or anything appended after its closing fence — is not the
/// template's checklist and cannot stand in for it. A second complete PR
/// Template, or a template with more than one `## Checklist`,
/// is ambiguous and yields no section.
///
/// Within the section, every line that names an item's label is a claim about
/// that item, whatever its shape, and exactly one may exist: `- [x] <label>`
/// or `- [ ] <label>`. A second line naming the same item — a duplicate, a
/// contradicting copy, or a variant such as `* [x] <label>` — is ambiguous in
/// the same way a second template is, so no copy wins; reading the first one
/// let an honest `- [ ]` hide a false `- [x]` beneath it. `None` per item means
/// no single readable line for it — missing, ambiguous, or a mark that is
/// neither `x` nor a space — never a guessed mark; with no readable section
/// every item is `None`.
pub(crate) fn parse_pr_checklist(pr_review: &str) -> Vec<(PrChecklistItem, Option<bool>)> {
    let section = pr_template_checklist(pr_review).unwrap_or_default();
    PrChecklistItem::ALL
        .into_iter()
        .map(|item| {
            let mut claims = section.iter().filter(|line| line.contains(item.label()));
            let mark = match (claims.next(), claims.next()) {
                (Some(line), None) => pr_checklist_mark(line, item),
                _ => None,
            };
            (item, mark)
        })
        .collect()
}

/// The mark on the one line that claims `item`, when that line has exactly the
/// rendered shape.
fn pr_checklist_mark(line: &str, item: PrChecklistItem) -> Option<bool> {
    let (mark, label) = line.strip_prefix("- [")?.split_once("] ")?;
    if label != item.label() {
        return None;
    }
    match mark {
        "x" | "X" => Some(true),
        " " => Some(false),
        _ => None,
    }
}

/// The lines of the generated tail's `## Checklist` section (see
/// [`parse_pr_checklist`]), or `None` when there is no such section or it is
/// ambiguous.
fn pr_template_checklist(pr_review: &str) -> Option<Vec<&str>> {
    let lines: Vec<&str> = pr_review.lines().collect();
    // The generator appends a separator and then its template. An earlier
    // diagnostic can quote the signature, but without a complete checklist it
    // is not a template. File paths are escaped before rendering, and two
    // complete templates remain ambiguous.
    let signature = [
        "## PR Template",
        "",
        "_Copy below for GitHub PR description:_",
        "",
        "```markdown",
    ];
    let candidates: Vec<_> = (2..lines.len())
        .filter(|&i| {
            lines[i - 2] == "---"
                && lines[i - 1].is_empty()
                && lines.get(i..i + signature.len()) == Some(signature.as_slice())
        })
        .filter_map(|heading| {
            let after = &lines[heading + 1..];
            let open = after
                .iter()
                .take_while(|line| !line.starts_with("## "))
                .position(|line| *line == "```markdown")?;
            let body = &after[open + 1..];
            let close = body.iter().position(|line| line.starts_with("```"))?;
            let block = &body[..close];
            block
                .contains(&"## Checklist")
                .then_some((block, heading + open + close + 2))
        })
        .collect();
    let [(block, close_line)] = candidates.as_slice() else {
        return None;
    };
    // An unanchored complete template appended after the real tail is also
    // ambiguous; ordinary trailing narrative cannot provide checklist lines.
    if lines[*close_line + 1..]
        .windows(signature.len())
        .any(|window| window == signature.as_slice())
    {
        return None;
    }
    let mut checklists = (0..block.len()).filter(|&i| block[i] == "## Checklist");
    let start = checklists.next()? + 1;
    if checklists.next().is_some() {
        return None;
    }
    Some(
        block[start..]
            .iter()
            .take_while(|line| !line.starts_with("## "))
            .copied()
            .collect(),
    )
}

/// Whether this run actually reviewed a rewritten range: `--exact-base` is in
/// force for this very base, and the pinned base is not an ancestor of the
/// target.
///
/// The ancestry question is asked of the repository rather than inferred from
/// the CI event or the workflow that invoked the gate, so the note is true for
/// any caller of `--exact-base`. An unreadable repository or an unanswerable
/// ancestry query says "ancestor", which prints nothing: a claim that cannot be
/// proven is not made.
fn reviewed_a_rewritten_range(config: &Config, base_commit: &str, target_commit: &str) -> bool {
    if !config.required_base_exact {
        return false;
    }
    let Some(required) = config.required_base.as_ref() else {
        return false;
    };
    if required.commit_id != base_commit {
        return false;
    }
    let Ok(repo) = crate::git::Repository::open(&config.repo_root) else {
        return false;
    };
    !repo.is_ancestor(base_commit, target_commit).unwrap_or(true)
}

pub(crate) fn generate_pr_review(
    dir: &Path,
    config: &Config,
    diffs: &[Diff],
    checks: &[CheckResult],
    skipped_checks: &[crate::checks::SkippedCheck],
    coverage: &CoverageDelta,
    heuristics: Option<&HeuristicsResult>,
) -> Result<()> {
    use std::collections::HashMap;
    use std::fmt::Write;

    let timestamp = chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
    let profile = config.profile.kind.as_str();
    let cargo_tree = load_cargo_tree_index(dir);

    let (target, base, base_commit) = if let Some(diff) = diffs.first() {
        (
            diff.target.as_str(),
            diff.base.as_str(),
            diff.base_commit_id.as_str(),
        )
    } else {
        (
            config.target.as_deref().unwrap_or("HEAD"),
            display_base_name(config),
            config
                .required_base
                .as_ref()
                .map_or("", |required| required.commit_id.as_str()),
        )
    };
    // The header names the ref; the commit beside it says which one that was.
    let base_display = base_ref_display(base, base_commit);
    // Emitted only when the range really was rewritten, so a reader who sees a
    // file the commit list does not account for knows why.
    let rewritten_range_note = diffs
        .first()
        .filter(|diff| {
            reviewed_a_rewritten_range(config, &diff.base_commit_id, &diff.target_commit_id)
        })
        .map(|_| REWRITTEN_RANGE_NOTE);

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
        target, base_display, profile
    )?;
    writeln!(
        md,
        "> **Commits:** {} \u{2022} **Files:** {} \u{2022} **Code (excluding tests):** {} \u{2022} **Tests:** {} \u{2022} **Non-code:** {}",
        commit_count, files_changed, code_files, test_files, non_code_files
    )?;
    writeln!(md, "> **Generated:** {}", timestamp)?;
    if let Some(note) = rewritten_range_note {
        writeln!(md, ">")?;
        writeln!(md, "> {}", note)?;
    }
    writeln!(md)?;

    // Summary table
    writeln!(md, "| | |")?;
    writeln!(md, "|---|---|")?;
    writeln!(md, "| **Branch** | `{}` |", target)?;
    writeln!(md, "| **Base** | {} |", base_display)?;
    writeln!(md, "| **Generated** | {} |", timestamp)?;
    writeln!(md)?;

    writeln!(md, "## Summary")?;
    writeln!(md)?;
    writeln!(md, "| Metric | Value |")?;
    writeln!(md, "|--------|-------|")?;
    writeln!(md, "| Commits | {} |", commit_count)?;
    writeln!(md, "| Files changed | {} |", files_changed)?;
    writeln!(md, "| Diff | {} |", diff_stat_line)?;
    writeln!(md, "| Code files (excluding tests) | {} |", code_files)?;
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
            .map(|file| {
                format!(
                    "`{}` ({})",
                    file.path.escape_debug(),
                    file.additions + file.deletions
                )
            })
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
        // Git paths may contain newlines and even a whole Markdown template.
        // Keep each path on one visible line so it cannot create artifact
        // headings, fences or checklist claims of its own.
        writeln!(md, "{}\t{}", status_char, f.path.escape_debug())?;
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
            let msg = c.message.lines().next().unwrap_or("").replace('|', "\\|");
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
    let skipped: HashMap<String, &crate::checks::SkippedCheck> = skipped_checks
        .iter()
        .map(|check| (check.name.to_lowercase(), check))
        .collect();
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
                crate::checks::CheckStatus::Skipped => (
                    "\u{23ed}\u{fe0f}",
                    format!(
                        "Not executed by this PrView run. Reason: {}. External CI status not included.",
                        result
                            .output
                            .lines()
                            .find(|line| !line.trim().is_empty())
                            .unwrap_or("reason unavailable")
                            .replace('|', "\\|")
                    ),
                ),
            };
            writeln!(md, "| {} | {} {} |", name, icon, text)?;
        } else if let Some(skipped) = skipped.get(&name.to_lowercase()) {
            writeln!(
                md,
                "| {} | \u{23ed}\u{fe0f} Not executed by this PrView run. Reason: {}. External CI status not included. |",
                name,
                skipped.reason.replace('|', "\\|")
            )?;
        }
    }
    writeln!(md)?;

    // Contract §7: how much of each test suite ran, in prose, for the reader who
    // never opens MERGE_GATE.json. Rendered from the same `ScopeReport` those
    // rows carry, so the sentence and the JSON cannot disagree. REVIEW_SUMMARY.md
    // embeds this file, which is how it inherits the same statement.
    if let Some(scope) = config.test_scope.as_ref() {
        let sentences = scope.review_sentences(checks);
        if !sentences.is_empty() {
            writeln!(md, "## Test Scope")?;
            writeln!(md)?;
            for sentence in sentences {
                writeln!(md, "- {}", sentence)?;
            }
            writeln!(md)?;
        }
    }

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

    let quick_wins = collect_quick_wins(config, checks);
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
    if let Some(pct) = coverage.pct
        && pct < 80
    {
        warnings.push(format!(
            "Coverage review signal: {}% heuristic coverage ({}/{})",
            pct, coverage.covered_count, coverage.total_source
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

    // Auto-check from check results. Each universal claim is earned only when
    // EVERY check of its category that ran passed (see `derive_pr_checklist`);
    // the consistency checker re-derives the same claims from report.json.
    let outcomes: Vec<_> = checks
        .iter()
        .map(|c| {
            (
                c.name.as_str(),
                ChecklistCheckOutcome::from_status(c.status, c.cached),
            )
        })
        .collect();
    for claim in derive_pr_checklist(&outcomes) {
        writeln!(
            md,
            "- [{}] {}",
            if claim.ticked() { "x" } else { " " },
            claim.item.label()
        )?;
    }
    writeln!(md, "- [ ] Manually tested")?;
    writeln!(md, "```")?;

    fs::write(dir.join("PR_REVIEW.md"), md)?;
    Ok(())
}
