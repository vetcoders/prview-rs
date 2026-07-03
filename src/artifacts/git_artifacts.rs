//! Git-derived pack files: file status, commit list, per-commit diffs, full patch.

use super::*;

pub(super) fn generate_file_status(dir: &Path, diffs: &[Diff]) -> Result<()> {
    let mut content = String::new();

    for diff in diffs {
        for file in &diff.files {
            let status_char = match file.status {
                crate::git::FileStatus::Added => 'A',
                crate::git::FileStatus::Modified => 'M',
                crate::git::FileStatus::Deleted => 'D',
                crate::git::FileStatus::Renamed => 'R',
                crate::git::FileStatus::Copied => 'C',
            };
            content.push_str(&format!("{}\t{}\n", status_char, file.path));
        }
    }

    fs::write(dir.join("file-status.txt"), content)?;
    Ok(())
}

pub(super) fn generate_commit_list(dir: &Path, diffs: &[Diff]) -> Result<()> {
    let mut content = String::new();

    if let Some(diff) = diffs.first() {
        for commit in &diff.commits {
            content.push_str(&format!(
                "{} {} {} {}\n",
                commit.short_id, commit.date, commit.author, commit.message
            ));
        }
    }

    if content.is_empty() {
        content = "(no commits)\n".to_string();
    }

    fs::write(dir.join("commit-list.txt"), content)?;
    Ok(())
}

/// Create a `latest` symlink in the parent of `out_dir` pointing to `out_dir`'s basename
pub(super) fn create_latest_symlink(out_dir: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        if let (Some(parent), Some(basename)) = (out_dir.parent(), out_dir.file_name()) {
            let latest_link = parent.join("latest");
            let _ = fs::remove_file(&latest_link);
            std::os::unix::fs::symlink(basename, &latest_link)?;
        }
    }
    Ok(())
}

/// Generate changed-tests.txt listing test files touched by this diff
pub(super) fn generate_changed_tests(diffs: &[Diff], dir: &Path) -> Result<()> {
    let mut test_files: Vec<String> = Vec::new();

    let ignored_extensions = ["json", "md", "txt", "yaml", "yml", "snap"];

    for diff in diffs {
        for file in &diff.files {
            let safe_path = crate::paths::validate_repo_relative_str(&file.path)?;
            let ext = safe_path.extension().and_then(|e| e.to_str()).unwrap_or("");
            if ignored_extensions.contains(&ext) {
                continue;
            }
            if is_test_file(&file.path) {
                test_files.push(file.path.clone());
            }
        }
    }

    test_files.sort();
    test_files.dedup();

    let content = if test_files.is_empty() {
        "(no test files changed)\n".to_string()
    } else {
        format!(
            "# Changed test files: {}\n\n{}\n",
            test_files.len(),
            test_files.join("\n")
        )
    };

    fs::write(dir.join("changed-tests.txt"), content)?;
    Ok(())
}

/// Parse a patch string and return per-file diff stats
pub(super) fn compute_diff_stat(patch: &str) -> Vec<(String, usize, usize)> {
    let mut stats: Vec<(String, usize, usize)> = Vec::new();
    let mut current_file: Option<String> = None;
    let mut adds = 0usize;
    let mut dels = 0usize;

    for line in patch.lines() {
        if let Some(rest) = line.strip_prefix("diff --git a/") {
            // Flush previous file
            if let Some(file) = current_file.take() {
                stats.push((file, adds, dels));
            }
            // Parse "diff --git a/FILE b/FILE" — take the b/ part
            if let Some(b_part) = rest.split(" b/").nth(1) {
                current_file = Some(b_part.to_string());
            }
            adds = 0;
            dels = 0;
        } else if line.starts_with('+') && !line.starts_with("+++") {
            adds += 1;
        } else if line.starts_with('-') && !line.starts_with("---") {
            dels += 1;
        }
    }
    if let Some(file) = current_file {
        stats.push((file, adds, dels));
    }
    stats
}

/// Format a diff-stat block as header comment lines
pub(super) fn format_diff_stat_header(stats: &[(String, usize, usize)]) -> String {
    if stats.is_empty() {
        return String::new();
    }
    let mut out = String::new();
    out.push_str(&format!("# {} files changed\n", stats.len()));
    let max_path = stats.iter().map(|(f, _, _)| f.len()).max().unwrap_or(0);
    for (file, adds, dels) in stats {
        out.push_str(&format!(
            "# {:<width$}  +{:<4} -{}\n",
            file,
            adds,
            dels,
            width = max_path
        ));
    }
    out
}

pub(super) fn sanitize_commit_msg(msg: &str) -> String {
    msg.chars()
        .take(40)
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

pub(super) fn generate_per_commit_diffs(
    repo: &Repository,
    dir: &Path,
    diffs: &[Diff],
    emit_human_stdout: bool,
) -> Result<()> {
    use colored::Colorize;

    let diff = match diffs.first() {
        Some(d) => d,
        None => return Ok(()),
    };

    let commit_count = diff.commits.len();

    if commit_count > MAX_COMMITS_FOR_PER_COMMIT_DIFFS {
        if emit_human_stdout {
            println!(
                "  {} Skipping per-commit diffs (>{} commits), generating top-10 summary",
                "i".blue(),
                MAX_COMMITS_FOR_PER_COMMIT_DIFFS
            );
        }

        // Generate top-10 commits by churn even when full diffs are skipped
        let mut commit_churns: Vec<(&crate::git::CommitInfo, usize)> = diff
            .commits
            .iter()
            .filter(|c| !c.message.starts_with("Merge "))
            .filter_map(|c| {
                let patch = repo.commit_patch(&c.id).ok()?;
                let stats = compute_diff_stat(&patch);
                let churn: usize = stats.iter().map(|s| s.1 + s.2).sum();
                Some((c, churn))
            })
            .collect();
        commit_churns.sort_by_key(|entry| std::cmp::Reverse(entry.1));
        commit_churns.truncate(10);

        let mut summary = format!(
            "# Per-commit diffs skipped: too many commits ({} > {})\n\n\
             ## Top 10 commits by churn\n\n\
             | # | Churn | Commit | Message |\n\
             |---|-------|--------|---------|{}\n",
            commit_count,
            MAX_COMMITS_FOR_PER_COMMIT_DIFFS,
            commit_churns
                .iter()
                .enumerate()
                .map(|(i, (c, churn))| {
                    let msg: String = c.message.chars().take(60).collect();
                    format!("\n| {} | {} | `{}` | {} |", i + 1, churn, c.short_id, msg)
                })
                .collect::<String>()
        );
        summary.push('\n');

        fs::write(dir.join("00-SUMMARY.md"), summary)?;
        return Ok(());
    }

    if emit_human_stdout {
        println!(
            "  {} Generating per-commit diffs ({} commits)",
            "i".blue(),
            commit_count
        );
    }

    let use_batching = commit_count > COMMIT_BATCH_THRESHOLD;

    if use_batching {
        generate_batched_commits(repo, dir, diff, commit_count)?;
    } else {
        generate_individual_commits(repo, dir, diff)?;
    }

    Ok(())
}

/// Generate individual patch files (one per commit, for <= COMMIT_BATCH_THRESHOLD commits)
pub(super) fn generate_individual_commits(
    repo: &Repository,
    dir: &Path,
    diff: &Diff,
) -> Result<()> {
    let commit_count = diff.commits.len();

    for (idx, commit) in diff.commits.iter().enumerate() {
        let patch = repo.commit_patch(&commit.id)?;
        let stats = compute_diff_stat(&patch);
        let safe_msg = sanitize_commit_msg(&commit.message);
        let filename = format!("{:02}-{}-{}.patch", idx + 1, commit.short_id, safe_msg);

        let mut content = String::new();
        content.push_str(&format!("# Commit: {}\n", commit.id));
        content.push_str(&format!("# Author: {} <{}>\n", commit.author, commit.email));
        content.push_str(&format!("# Date:   {}\n", commit.date));
        content.push_str(&format!("# Message: {}\n", commit.message));
        content.push_str("#\n# --- Diff stat ---\n");
        content.push_str(&format_diff_stat_header(&stats));
        content.push_str("# ---\n");
        content.push_str(&patch);
        content.push('\n');

        fs::write(dir.join(&filename), content)?;
    }

    // Summary
    let mut summary = String::new();
    summary.push_str("# Per-Commit Diffs Summary\n");
    summary.push_str(&format!(
        "# Generated: {}\n",
        chrono::Local::now().format("%Y-%m-%d %H:%M:%S")
    ));
    summary.push_str(&format!("# Total commits: {}\n\n", commit_count));
    summary.push_str("## Commits (oldest first):\n\n");
    for (idx, commit) in diff.commits.iter().enumerate() {
        let safe_msg = sanitize_commit_msg(&commit.message);
        summary.push_str(&format!(
            "- `{:02}-{}-{}.patch` | {} | {} | {}\n",
            idx + 1,
            commit.short_id,
            safe_msg,
            commit.date,
            commit.author,
            commit.message
        ));
    }

    fs::write(dir.join("00-SUMMARY.md"), summary)?;
    Ok(())
}

/// Generate batched patch files (groups of COMMIT_BATCH_SIZE, for > COMMIT_BATCH_THRESHOLD commits)
pub(super) fn generate_batched_commits(
    repo: &Repository,
    dir: &Path,
    diff: &Diff,
    commit_count: usize,
) -> Result<()> {
    let batches: Vec<_> = diff.commits.chunks(COMMIT_BATCH_SIZE).collect();
    let mut summary = String::new();
    summary.push_str("# Per-Commit Diffs Summary (Batched)\n");
    summary.push_str(&format!(
        "# Generated: {}\n",
        chrono::Local::now().format("%Y-%m-%d %H:%M:%S")
    ));
    summary.push_str(&format!(
        "# Total commits: {} in {} batches\n\n",
        commit_count,
        batches.len()
    ));

    let mut global_idx = 0usize;

    for (batch_idx, batch) in batches.iter().enumerate() {
        let batch_start = global_idx + 1;
        let batch_end = global_idx + batch.len();
        let batch_filename = format!("batch-{:02}.patch", batch_idx + 1);
        let batch_theme = infer_batch_theme(batch);

        let mut batch_content = String::new();
        let mut all_stats: Vec<(String, usize, usize)> = Vec::new();
        let mut commit_patches: Vec<(String, String)> = Vec::new();

        for commit in *batch {
            let patch = repo.commit_patch(&commit.id)?;
            let stats = compute_diff_stat(&patch);
            for (file, a, d) in &stats {
                if let Some(existing) = all_stats.iter_mut().find(|(f, _, _)| f == file) {
                    existing.1 += a;
                    existing.2 += d;
                } else {
                    all_stats.push((file.clone(), *a, *d));
                }
            }
            commit_patches.push((commit.id.clone(), patch));
        }

        let total_adds: usize = all_stats.iter().map(|(_, a, _)| *a).sum();
        let total_dels: usize = all_stats.iter().map(|(_, _, d)| *d).sum();

        batch_content.push_str(&format!(
            "# Batch {:02}: {} — commits {}-{} of {}\n",
            batch_idx + 1,
            batch_theme,
            batch_start,
            batch_end,
            commit_count
        ));
        batch_content.push_str("# --- Batch diff stat ---\n");
        batch_content.push_str(&format!(
            "# {} files changed, +{} -{}\n",
            all_stats.len(),
            total_adds,
            total_dels
        ));
        batch_content.push_str(&format_diff_stat_header(&all_stats));
        batch_content.push_str("# ---\n\n");

        for (commit, (_, patch)) in batch.iter().zip(commit_patches.iter()) {
            batch_content.push_str(&format!(
                "## Commit: {} -- {}\n",
                commit.short_id, commit.message
            ));
            batch_content.push_str(patch);
            batch_content.push_str("\n\n");
        }

        fs::write(dir.join(&batch_filename), batch_content)?;

        // Add to summary
        summary.push_str(&format!(
            "### Batch {:02} (`{}`): {} — commits {}-{}\n\n",
            batch_idx + 1,
            batch_filename,
            batch_theme,
            batch_start,
            batch_end
        ));
        for commit in *batch {
            summary.push_str(&format!(
                "- {} | {} | {} | {}\n",
                commit.short_id, commit.date, commit.author, commit.message
            ));
        }
        summary.push('\n');

        global_idx = batch_end;
    }

    fs::write(dir.join("00-SUMMARY.md"), summary)?;
    Ok(())
}

pub(super) fn infer_batch_theme(batch: &[crate::git::CommitInfo]) -> String {
    const THEMES: &[(&str, &[&str])] = &[
        (
            "search infrastructure",
            &[
                "search", "query", "bm25", "hybrid", "retriev", "index", "ranking", "rank",
                "vector",
            ],
        ),
        (
            "storage and persistence",
            &[
                "storage", "db", "database", "sqlite", "cache", "persist", "store", "lance",
            ],
        ),
        (
            "api and runtime",
            &[
                "api", "server", "http", "grpc", "rpc", "runtime", "service", "resolver",
            ],
        ),
        (
            "artifacts and review flow",
            &[
                "artifact",
                "report",
                "review",
                "sarif",
                "dashboard",
                "merge",
                "gate",
                "signal",
            ],
        ),
        (
            "tests and validation",
            &[
                "test",
                "e2e",
                "integration",
                "fixture",
                "contract",
                "validate",
                "qa",
            ],
        ),
        (
            "dependencies and security",
            &[
                "dependency",
                "dependencies",
                "dep ",
                "upgrade",
                "bump",
                "rustsec",
                "audit",
                "security",
            ],
        ),
        (
            "docs and tooling",
            &[
                "docs", "readme", "ci", "clippy", "fmt", "tool", "hook", "build",
            ],
        ),
        (
            "ui and tui polish",
            &["ui", "tui", "panel", "layout", "view"],
        ),
    ];

    let messages: Vec<String> = batch
        .iter()
        .map(|commit| commit.message.to_ascii_lowercase())
        .collect();

    let mut best_label = "mixed changes";
    let mut best_score = 0usize;
    for (label, keywords) in THEMES {
        let score = messages
            .iter()
            .map(|message| {
                keywords
                    .iter()
                    .filter(|keyword| message.contains(**keyword))
                    .count()
            })
            .sum::<usize>();
        if score > best_score {
            best_score = score;
            best_label = label;
        }
    }

    if best_score == 0
        && batch
            .iter()
            .all(|commit| commit.message.to_ascii_lowercase().contains("test"))
    {
        return "tests and validation".to_string();
    }

    best_label.to_string()
}

/// Generate full.patch and return the raw patch texts per diff (for reuse by breaking_changes).
pub(super) fn generate_full_patch(
    dir: &Path,
    repo: &Repository,
    diffs: &[Diff],
) -> Result<Vec<String>> {
    let mut content = String::new();
    let mut patch_texts = Vec::with_capacity(diffs.len());

    for diff in diffs {
        content.push_str(&format!(
            "# Diff: {} vs {}\n# Files: {} | +{} -{}\n\n",
            diff.base,
            diff.target,
            diff.stats.files_changed,
            diff.stats.additions,
            diff.stats.deletions
        ));

        match repo.full_diff(&diff.base_commit_id, &diff.target_commit_id) {
            Ok(patch) => {
                content.push_str(&patch);
                content.push('\n');
                patch_texts.push(patch);
            }
            Err(e) => {
                content.push_str(&format!("# Error generating diff: {}\n\n", e));
                patch_texts.push(String::new());
            }
        }
    }

    fs::write(dir.join("full.patch"), content)?;
    Ok(patch_texts)
}
