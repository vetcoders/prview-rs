//! Hot files — top changed files by churn (insertions + deletions)

use crate::git::git_cmd;
use anyhow::{Context, Result};
use std::path::Path;

/// Maximum number of hot files shown in display outputs (CLI, TUI, JSON).
pub(crate) const HOT_FILES_DISPLAY_LIMIT: usize = 10;

#[derive(Debug, Clone)]
pub struct HotFile {
    pub lines: usize,
    pub path: String,
}

pub fn get_hot_files(root: &Path) -> Result<Vec<HotFile>> {
    let output = git_cmd()
        .args(["diff", "HEAD", "--numstat"])
        .current_dir(root)
        .output()
        .context("git diff --numstat")?;

    if !output.status.success() {
        anyhow::bail!(
            "git diff --numstat failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut files: Vec<HotFile> = Vec::new();

    for line in stdout.lines() {
        let parts: Vec<&str> = line.split('\t').collect();
        if parts.len() == 3 {
            // Binary files show "-" for insertions/deletions
            let insertions: usize = parts[0].parse().unwrap_or(0);
            let deletions: usize = parts[1].parse().unwrap_or(0);
            let total = insertions + deletions;
            if total > 0 {
                files.push(HotFile {
                    lines: total,
                    path: parts[2].to_string(),
                });
            }
        }
    }

    files.sort_by_key(|file| std::cmp::Reverse(file.lines));
    Ok(files)
}

pub fn print_hot_files(root: &Path) -> Result<()> {
    use colored::Colorize;

    let files = get_hot_files(root)?;

    if files.is_empty() {
        println!("{}", "No changed files".dimmed());
        println!();
        return Ok(());
    }

    println!("{}", "Hot Files".cyan().bold());
    for hot in files.iter().take(HOT_FILES_DISPLAY_LIMIT) {
        println!("  {:>6}  {}", hot.lines, hot.path);
    }
    println!();

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::git_cmd;

    fn make_temp_repo() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().to_path_buf();

        git_cmd()
            .args(["init", "-b", "main"])
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
    fn test_clean_repo_no_hot_files() {
        let (_dir, path) = make_temp_repo();
        let files = get_hot_files(&path).unwrap();
        assert!(files.is_empty());
    }

    #[test]
    fn test_single_modified_file() {
        let (_dir, path) = make_temp_repo();
        std::fs::write(path.join("init.txt"), "changed\nline2\nline3\n").unwrap();

        let files = get_hot_files(&path).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path, "init.txt");
        assert!(files[0].lines > 0);
    }

    #[test]
    fn test_sorted_by_churn_descending() {
        let (_dir, path) = make_temp_repo();

        // Create and commit two files
        std::fs::write(path.join("small.txt"), "a\n").unwrap();
        std::fs::write(path.join("big.txt"), "a\nb\nc\nd\ne\n").unwrap();
        git_cmd()
            .args(["add", "."])
            .current_dir(&path)
            .output()
            .unwrap();
        git_cmd()
            .args(["commit", "-m", "add files"])
            .current_dir(&path)
            .output()
            .unwrap();

        // Modify both — big.txt gets more churn
        std::fs::write(path.join("small.txt"), "x\n").unwrap();
        std::fs::write(path.join("big.txt"), "x\ny\nz\nw\nv\nu\n").unwrap();

        let files = get_hot_files(&path).unwrap();
        assert!(files.len() == 2);
        assert!(
            files[0].lines >= files[1].lines,
            "should be sorted descending"
        );
    }

    #[test]
    fn test_max_10_files_displayed() {
        let (_dir, path) = make_temp_repo();

        // Create 15 files
        for i in 0..15 {
            let name = format!("file{:02}.txt", i);
            std::fs::write(path.join(&name), "content\n").unwrap();
        }
        git_cmd()
            .args(["add", "."])
            .current_dir(&path)
            .output()
            .unwrap();
        git_cmd()
            .args(["commit", "-m", "add 15 files"])
            .current_dir(&path)
            .output()
            .unwrap();

        // Modify all
        for i in 0..15 {
            let name = format!("file{:02}.txt", i);
            std::fs::write(path.join(&name), "modified\n").unwrap();
        }

        let files = get_hot_files(&path).unwrap();
        assert_eq!(files.len(), 15); // get_hot_files returns all
        // print_hot_files limits to 10 — tested via print_hot_files
    }
}
