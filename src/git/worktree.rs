//! Ephemeral git worktree support for remote check verification
//!
//! Creates a detached git worktree at a specific commit, with
//! local dependencies (node_modules, .venv) symlinked to preserve local caches.

use super::cmd::git_cmd;
use anyhow::Result;
use std::path::{Path, PathBuf};

/// An ephemeral detached `git worktree` checked out at a specific commit. Kept
/// alive for the duration of a scan; the worktree is deregistered and its files
/// removed on drop, on every path (scan success or error).
pub struct WorktreeSnapshot {
    pub repo_root: PathBuf,
    pub worktree_path: PathBuf,
    // Owns the enclosing temp dir; dropped after the worktree is deregistered so
    // the directory removal is the backstop for the `git worktree remove` call.
    _tmp: tempfile::TempDir,
}

impl Drop for WorktreeSnapshot {
    fn drop(&mut self) {
        // Deregister the worktree from the main repo, then prune bookkeeping.
        // `--force` is required because the checkout is detached. Errors are
        // swallowed: cleanup must be best-effort and never panic in a
        // destructor (the temp-dir removal is the backstop).
        let _ = git_cmd()
            .args(["worktree", "remove", "--force"])
            .arg(&self.worktree_path)
            .current_dir(&self.repo_root)
            .output();
        let _ = git_cmd()
            .args(["worktree", "prune"])
            .current_dir(&self.repo_root)
            .output();
    }
}

/// Create an ephemeral detached worktree of `commit` under a fresh temp dir.
pub fn create_worktree_snapshot(repo_root: &Path, commit: &str) -> Result<WorktreeSnapshot> {
    let tmp = tempfile::tempdir()?;
    // `git worktree add` wants a path it can create, so point it at a fresh
    // subdirectory of the temp dir rather than the (already-created) temp root.
    let worktree_path = tmp.path().join("snapshot");

    let output = git_cmd()
        .args(["worktree", "add", "--detach", "--force"])
        .arg(&worktree_path)
        .arg(commit)
        .current_dir(repo_root)
        .output()?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("git worktree add failed: {}", stderr.trim());
    }

    // Symlink untracked dependencies (node_modules and .venv) to bypass reinstall overhead
    #[cfg(unix)]
    {
        let nm = repo_root.join("node_modules");
        if nm.exists() {
            let _ = std::os::unix::fs::symlink(&nm, worktree_path.join("node_modules"));
        }
        let venv = repo_root.join(".venv");
        if venv.exists() {
            let _ = std::os::unix::fs::symlink(&venv, worktree_path.join(".venv"));
        }
    }

    Ok(WorktreeSnapshot {
        repo_root: repo_root.to_path_buf(),
        worktree_path,
        _tmp: tmp,
    })
}
