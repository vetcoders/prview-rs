//! Repository identity: name, branch, HEAD

use crate::git::git_cmd;
use anyhow::{Context, Result};
use std::path::Path;

#[derive(Debug)]
pub struct RepoInfo {
    pub name: String,
    pub branch: String,
    pub short_sha: String,
}

pub fn get_repo_info(root: &Path) -> Result<RepoInfo> {
    let name = root
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown")
        .to_string();

    let branch_out = git_cmd()
        .args(["branch", "--show-current"])
        .current_dir(root)
        .output()
        .context("git branch --show-current")?;
    if !branch_out.status.success() {
        anyhow::bail!(
            "git branch --show-current failed: {}",
            String::from_utf8_lossy(&branch_out.stderr).trim()
        );
    }
    let branch = String::from_utf8_lossy(&branch_out.stdout)
        .trim()
        .to_string();
    let branch = if branch.is_empty() {
        "HEAD (detached)".to_string()
    } else {
        branch
    };

    let sha_out = git_cmd()
        .args(["rev-parse", "HEAD"])
        .current_dir(root)
        .output()
        .context("git rev-parse HEAD")?;
    if !sha_out.status.success() {
        let stderr = String::from_utf8_lossy(&sha_out.stderr);
        if stderr.contains("ambiguous argument 'HEAD'") {
            anyhow::bail!("No commits yet — run `git commit` first, then retry `prview state`");
        }
        anyhow::bail!("git rev-parse HEAD failed: {}", stderr.trim());
    }
    let sha_full = String::from_utf8_lossy(&sha_out.stdout).trim().to_string();
    let short_sha = if sha_full.len() >= 7 {
        sha_full[..7].to_string()
    } else {
        sha_full
    };

    Ok(RepoInfo {
        name,
        branch,
        short_sha,
    })
}

pub fn print_repo_info(root: &Path) -> Result<()> {
    use colored::Colorize;

    let info = get_repo_info(root)?;

    println!("{}", "Repository".cyan().bold());
    println!("  Name:   {}", info.name);
    println!("  Branch: {}", info.branch.green());
    println!("  HEAD:   {}", info.short_sha.yellow());
    println!();

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::git_cmd;

    /// Create a temp git repo with an initial commit on a named branch.
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
    fn test_repo_info_basic() {
        let (dir, path) = make_temp_repo();
        let info = get_repo_info(&path).unwrap();

        // Name is the directory name
        let expected_name = dir
            .path()
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        assert_eq!(info.name, expected_name);

        // Branch should be "main" (from init -b main)
        assert_eq!(info.branch, "main");

        // Short SHA is 7 chars hex
        assert_eq!(info.short_sha.len(), 7);
        assert!(
            info.short_sha.chars().all(|c| c.is_ascii_hexdigit()),
            "short_sha should be hex: {}",
            info.short_sha
        );
    }

    #[test]
    fn test_repo_info_custom_branch() {
        let (_dir, path) = make_temp_repo();

        git_cmd()
            .args(["checkout", "-b", "feat/my-feature"])
            .current_dir(&path)
            .output()
            .expect("git checkout");

        let info = get_repo_info(&path).unwrap();
        assert_eq!(info.branch, "feat/my-feature");
    }

    #[test]
    fn test_repo_info_detached_head() {
        let (_dir, path) = make_temp_repo();

        // Get current SHA and detach
        let sha_out = git_cmd()
            .args(["rev-parse", "HEAD"])
            .current_dir(&path)
            .output()
            .expect("rev-parse");
        let sha = String::from_utf8_lossy(&sha_out.stdout).trim().to_string();

        git_cmd()
            .args(["checkout", &sha])
            .current_dir(&path)
            .output()
            .expect("git checkout detached");

        let info = get_repo_info(&path).unwrap();
        assert_eq!(info.branch, "HEAD (detached)");
        assert_eq!(info.short_sha, &sha[..7]);
    }

    #[test]
    fn test_repo_info_sha_matches_git() {
        let (_dir, path) = make_temp_repo();

        let sha_out = git_cmd()
            .args(["rev-parse", "HEAD"])
            .current_dir(&path)
            .output()
            .expect("rev-parse");
        let full_sha = String::from_utf8_lossy(&sha_out.stdout).trim().to_string();

        let info = get_repo_info(&path).unwrap();
        assert!(
            full_sha.starts_with(&info.short_sha),
            "full SHA {} should start with short SHA {}",
            full_sha,
            info.short_sha
        );
    }

    #[test]
    fn test_repo_info_not_a_repo() {
        let dir = tempfile::tempdir().expect("tempdir");
        let result = get_repo_info(dir.path());
        assert!(result.is_err(), "should fail on non-git directory");
    }

    #[test]
    fn test_repo_info_empty_repo_no_commits() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().to_path_buf();

        git_cmd()
            .args(["init", "-b", "main"])
            .current_dir(&path)
            .output()
            .expect("git init");

        let result = get_repo_info(&path);
        assert!(result.is_err(), "should fail on empty repo with no commits");
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("No commits yet"),
            "error should mention 'No commits yet', got: {}",
            err_msg
        );
    }
}
