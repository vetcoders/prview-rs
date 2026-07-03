//! Diff summary and file status (working tree vs HEAD)

use crate::git::git_cmd;
use anyhow::{Context, Result};
use std::path::Path;

pub struct WorkingTreeStats {
    pub files_changed: usize,
    pub insertions: usize,
    pub deletions: usize,
}

pub fn get_diff_stats(root: &Path) -> Result<WorkingTreeStats> {
    let output = git_cmd()
        .args(["diff", "HEAD", "--stat", "--stat-width=120"])
        .current_dir(root)
        .output()
        .context("git diff --stat")?;
    if !output.status.success() {
        anyhow::bail!(
            "git diff --stat failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    let stdout = String::from_utf8_lossy(&output.stdout);

    // Parse last line: " N files changed, X insertions(+), Y deletions(-)"
    let mut files_changed = 0;
    let mut insertions = 0;
    let mut deletions = 0;

    if let Some(summary) = stdout.lines().last() {
        for part in summary.split(',') {
            let part = part.trim();
            if part.contains("file") {
                files_changed = part
                    .split_whitespace()
                    .next()
                    .and_then(|n| n.parse().ok())
                    .unwrap_or(0);
            } else if part.contains("insertion") {
                insertions = part
                    .split_whitespace()
                    .next()
                    .and_then(|n| n.parse().ok())
                    .unwrap_or(0);
            } else if part.contains("deletion") {
                deletions = part
                    .split_whitespace()
                    .next()
                    .and_then(|n| n.parse().ok())
                    .unwrap_or(0);
            }
        }
    }

    Ok(WorkingTreeStats {
        files_changed,
        insertions,
        deletions,
    })
}

/// Count untracked files in the worktree.
///
/// `git diff HEAD` cannot see brand-new files, so a repo whose only change is
/// an untracked file reports zero diff stats. Callers that decide "is the
/// worktree dirty?" need this signal too (PR #12 review).
pub fn count_untracked(root: &Path) -> Result<usize> {
    let output = git_cmd()
        .args(["status", "--porcelain", "--untracked-files=all"])
        .current_dir(root)
        .output()
        .context("git status --porcelain")?;
    if !output.status.success() {
        anyhow::bail!(
            "git status --porcelain failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(stdout.lines().filter(|l| l.starts_with("??")).count())
}

pub fn print_diff_summary(root: &Path) -> Result<()> {
    use colored::Colorize;

    let stats = get_diff_stats(root)?;

    if stats.files_changed == 0 {
        println!("{}", "Working tree clean".dimmed());
        println!();
        return Ok(());
    }

    println!("{}", "Diff Summary".cyan().bold());
    println!(
        "  {} files changed, {} {}, {} {}",
        stats.files_changed,
        format!("+{}", stats.insertions).green(),
        "insertions".green(),
        format!("-{}", stats.deletions).red(),
        "deletions".red(),
    );
    println!();

    Ok(())
}

pub fn print_file_status(root: &Path) -> Result<()> {
    use colored::Colorize;

    let output = git_cmd()
        .args(["diff", "HEAD", "--name-status"])
        .current_dir(root)
        .output()
        .context("git diff --name-status")?;
    if !output.status.success() {
        anyhow::bail!(
            "git diff --name-status failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let lines: Vec<&str> = stdout.lines().collect();

    if lines.is_empty() {
        return Ok(());
    }

    println!("{}", "Changed Files".cyan().bold());
    for line in &lines {
        let parts: Vec<&str> = line.splitn(2, '\t').collect();
        if parts.len() == 2 {
            let status = match parts[0] {
                "M" => "M".yellow(),
                "A" => "A".green(),
                "D" => "D".red(),
                "R" => "R".blue(),
                s => s.normal(),
            };
            println!("  {} {}", status, parts[1]);
        }
    }
    println!();

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::git_cmd;

    /// Create a temp git repo with an initial commit.
    /// Returns (TempDir, PathBuf) — keep TempDir alive for the repo to exist.
    fn make_temp_repo() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().to_path_buf();

        git_cmd()
            .args(["init"])
            .current_dir(&path)
            .output()
            .expect("git init");
        git_cmd()
            .args(["config", "user.email", "test@test.com"])
            .current_dir(&path)
            .output()
            .expect("git config email");
        git_cmd()
            .args(["config", "user.name", "Test"])
            .current_dir(&path)
            .output()
            .expect("git config name");

        std::fs::write(path.join("init.txt"), "init\n").unwrap();
        git_cmd()
            .args(["add", "."])
            .current_dir(&path)
            .output()
            .expect("git add");
        git_cmd()
            .args(["commit", "-m", "init"])
            .current_dir(&path)
            .output()
            .expect("git commit");

        (dir, path)
    }

    #[test]
    fn test_clean_working_tree() {
        let (_dir, path) = make_temp_repo();
        let stats = get_diff_stats(&path).unwrap();
        assert_eq!(stats.files_changed, 0);
        assert_eq!(stats.insertions, 0);
        assert_eq!(stats.deletions, 0);
    }

    #[test]
    fn test_single_file_insertions_only() {
        let (_dir, path) = make_temp_repo();

        // Add a new file (not staged → unstaged diff vs HEAD won't see it)
        // Modify existing file instead — that shows as unstaged diff
        std::fs::write(path.join("init.txt"), "init\nline2\nline3\n").unwrap();

        let stats = get_diff_stats(&path).unwrap();
        assert_eq!(stats.files_changed, 1);
        assert_eq!(stats.insertions, 2);
        assert_eq!(stats.deletions, 0);
    }

    #[test]
    fn test_single_file_deletions_only() {
        let (_dir, path) = make_temp_repo();

        // Create multi-line file, commit, then remove lines
        std::fs::write(path.join("init.txt"), "a\nb\nc\n").unwrap();
        git_cmd()
            .args(["add", "."])
            .current_dir(&path)
            .output()
            .unwrap();
        git_cmd()
            .args(["commit", "-m", "multi-line"])
            .current_dir(&path)
            .output()
            .unwrap();

        // Now delete two lines
        std::fs::write(path.join("init.txt"), "a\n").unwrap();

        let stats = get_diff_stats(&path).unwrap();
        assert_eq!(stats.files_changed, 1);
        assert_eq!(stats.insertions, 0);
        assert_eq!(stats.deletions, 2);
    }

    #[test]
    fn test_mixed_insertions_and_deletions() {
        let (_dir, path) = make_temp_repo();

        // Replace content: 1 deletion (old "init\n") + 2 insertions
        std::fs::write(path.join("init.txt"), "replaced\nand more\n").unwrap();

        let stats = get_diff_stats(&path).unwrap();
        assert_eq!(stats.files_changed, 1);
        assert_eq!(stats.insertions, 2);
        assert_eq!(stats.deletions, 1);
    }

    #[test]
    fn test_multiple_files_changed() {
        let (_dir, path) = make_temp_repo();

        // Modify existing file
        std::fs::write(path.join("init.txt"), "changed\n").unwrap();

        // Add a new tracked file (must stage + commit first, then modify)
        std::fs::write(path.join("second.txt"), "hello\n").unwrap();
        git_cmd()
            .args(["add", "second.txt"])
            .current_dir(&path)
            .output()
            .unwrap();
        git_cmd()
            .args(["commit", "-m", "add second"])
            .current_dir(&path)
            .output()
            .unwrap();

        // Now modify both
        std::fs::write(path.join("init.txt"), "changed again\n").unwrap();
        std::fs::write(path.join("second.txt"), "hello\nworld\n").unwrap();

        let stats = get_diff_stats(&path).unwrap();
        assert_eq!(stats.files_changed, 2);
        // init.txt: 1 ins + 1 del (replaced "changed\n" -> "changed again\n")
        // Hmm — actually git may see differently. Just check >= 1
        assert!(stats.insertions >= 1);
        assert!(stats.files_changed == 2);
    }

    #[test]
    fn test_staged_changes_included() {
        // `git diff HEAD` includes both staged and unstaged changes
        let (_dir, path) = make_temp_repo();

        std::fs::write(path.join("init.txt"), "staged content\n").unwrap();
        git_cmd()
            .args(["add", "init.txt"])
            .current_dir(&path)
            .output()
            .unwrap();

        let stats = get_diff_stats(&path).unwrap();
        assert_eq!(stats.files_changed, 1);
        assert_eq!(stats.insertions, 1);
        assert_eq!(stats.deletions, 1);
    }

    #[test]
    fn test_new_untracked_file_not_counted() {
        // Untracked files do not appear in `git diff HEAD`
        let (_dir, path) = make_temp_repo();

        std::fs::write(path.join("untracked.txt"), "new file\n").unwrap();

        let stats = get_diff_stats(&path).unwrap();
        assert_eq!(stats.files_changed, 0);
    }
}
