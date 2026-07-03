//! Analysis snapshots via `git archive | tar`
//!
//! Creates clean, deterministic file trees from git objects
//! for heuristics analysis in remote/remote-only mode.

use super::cmd::git_cmd;
use anyhow::{Context, Result};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::SystemTime;

/// A temporary snapshot of a git tree at a specific commit.
/// Auto-cleaned on drop via `tempfile::TempDir`.
pub struct AnalysisSnapshot {
    pub path: PathBuf,
    pub sha: String,
    pub created_at: SystemTime,
    _tempdir: tempfile::TempDir,
}

impl super::Repository {
    /// Create a snapshot of the repository at the given SHA.
    ///
    /// Uses `git archive <sha> | tar -x` for a clean extraction
    /// without working tree state. Returns an `AnalysisSnapshot`
    /// whose path can be used as analysis root.
    pub fn create_snapshot(&self, sha: &str) -> Result<AnalysisSnapshot> {
        let short_sha = if sha.len() >= 7 { &sha[..7] } else { sha };
        let repo_name = self
            .path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("unknown");

        let base_dir = std::env::temp_dir().join("prview").join(repo_name);
        std::fs::create_dir_all(&base_dir).with_context(|| {
            format!("Failed to create snapshot base dir: {}", base_dir.display())
        })?;

        let tempdir = tempfile::Builder::new()
            .prefix(&format!("{}-{}-", short_sha, std::process::id()))
            .tempdir_in(&base_dir)
            .context("Failed to create snapshot tempdir")?;

        let dest = tempdir.path();

        // Pipeline: git archive <sha> | tar -x -C <dest>
        let mut archive = git_cmd()
            .args(["archive", sha])
            .current_dir(&self.path)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .context("Failed to spawn git archive")?;

        let archive_stdout = archive
            .stdout
            .take()
            .context("Failed to capture git archive stdout")?;

        let tar_status = Command::new("tar")
            .args(["-x", "-C"])
            .arg(dest)
            .stdin(archive_stdout)
            .stderr(Stdio::null())
            .status()
            .context("Failed to run tar")?;

        let archive_status = archive.wait().context("Failed to wait for git archive")?;

        if !archive_status.success() {
            anyhow::bail!(
                "git archive failed for SHA {} (exit code: {:?})",
                sha,
                archive_status.code()
            );
        }

        if !tar_status.success() {
            anyhow::bail!(
                "tar extraction failed for SHA {} (exit code: {:?})",
                sha,
                tar_status.code()
            );
        }

        // Optionally symlink node_modules from the working tree
        #[cfg(unix)]
        {
            let nm = self.path.join("node_modules");
            if nm.exists()
                && let Err(e) = std::os::unix::fs::symlink(&nm, dest.join("node_modules"))
            {
                eprintln!("[prview] warning: failed to symlink node_modules into snapshot: {e}");
            }
        }

        Ok(AnalysisSnapshot {
            path: dest.to_path_buf(),
            sha: sha.to_string(),
            created_at: SystemTime::now(),
            _tempdir: tempdir,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_analysis_snapshot_fields() {
        // Verify struct layout compiles and fields are accessible
        let dir = tempfile::tempdir().unwrap();
        let snap = AnalysisSnapshot {
            path: dir.path().to_path_buf(),
            sha: "abc1234".to_string(),
            created_at: SystemTime::now(),
            _tempdir: dir,
        };
        assert_eq!(snap.sha, "abc1234");
        assert!(snap.path.exists());
    }

    #[test]
    fn test_snapshot_auto_cleanup() {
        let path;
        {
            let dir = tempfile::tempdir().unwrap();
            path = dir.path().to_path_buf();
            let _snap = AnalysisSnapshot {
                path: path.clone(),
                sha: "deadbeef".to_string(),
                created_at: SystemTime::now(),
                _tempdir: dir,
            };
            assert!(path.exists());
        }
        // After drop, tempdir should be cleaned up
        assert!(!path.exists());
    }

    #[test]
    fn test_create_snapshot_with_real_repo() {
        // Create a temp dir for the git repo
        let repo_dir = tempfile::tempdir().unwrap();
        let repo_path = repo_dir.path().to_path_buf();

        // git init
        let init = git_cmd()
            .args(["init"])
            .current_dir(&repo_path)
            .output()
            .expect("git init failed");
        assert!(init.status.success(), "git init should succeed");

        // Configure user for the commit
        git_cmd()
            .args(["config", "user.email", "test@test.com"])
            .current_dir(&repo_path)
            .output()
            .expect("git config email failed");
        git_cmd()
            .args(["config", "user.name", "Test User"])
            .current_dir(&repo_path)
            .output()
            .expect("git config name failed");

        // Create a file and commit it
        std::fs::write(repo_path.join("hello.txt"), "hello world\n").unwrap();
        git_cmd()
            .args(["add", "hello.txt"])
            .current_dir(&repo_path)
            .output()
            .expect("git add failed");
        let commit = git_cmd()
            .args(["commit", "-m", "initial commit"])
            .current_dir(&repo_path)
            .output()
            .expect("git commit failed");
        assert!(commit.status.success(), "git commit should succeed");

        // Get the commit SHA
        let sha_output = git_cmd()
            .args(["rev-parse", "HEAD"])
            .current_dir(&repo_path)
            .output()
            .expect("git rev-parse failed");
        let sha = String::from_utf8_lossy(&sha_output.stdout)
            .trim()
            .to_string();
        assert_eq!(sha.len(), 40, "SHA should be 40 hex chars");

        // Open with our Repository wrapper and create a snapshot
        let repo = super::super::Repository::open(&repo_path).expect("open repo");
        let snap = repo.create_snapshot(&sha).expect("create_snapshot");

        // The extracted file should exist in the snapshot path
        assert!(snap.path.exists(), "snapshot path should exist");
        assert!(
            snap.path.join("hello.txt").exists(),
            "hello.txt should be in snapshot"
        );
        let content = std::fs::read_to_string(snap.path.join("hello.txt")).unwrap();
        assert_eq!(content, "hello world\n");
        assert_eq!(snap.sha, sha);

        // Capture path before drop
        let snap_path = snap.path.clone();
        drop(snap);
        // After drop the tempdir is cleaned up
        assert!(
            !snap_path.exists(),
            "snapshot dir should be gone after drop"
        );
    }

    #[test]
    fn test_create_snapshot_invalid_sha() {
        // Create a minimal git repo
        let repo_dir = tempfile::tempdir().unwrap();
        let repo_path = repo_dir.path().to_path_buf();

        git_cmd()
            .args(["init"])
            .current_dir(&repo_path)
            .output()
            .expect("git init failed");
        git_cmd()
            .args(["config", "user.email", "test@test.com"])
            .current_dir(&repo_path)
            .output()
            .unwrap();
        git_cmd()
            .args(["config", "user.name", "Test User"])
            .current_dir(&repo_path)
            .output()
            .unwrap();
        std::fs::write(repo_path.join("f.txt"), "x").unwrap();
        git_cmd()
            .args(["add", "f.txt"])
            .current_dir(&repo_path)
            .output()
            .unwrap();
        git_cmd()
            .args(["commit", "-m", "init"])
            .current_dir(&repo_path)
            .output()
            .unwrap();

        let repo = super::super::Repository::open(&repo_path).expect("open repo");
        let result = repo.create_snapshot("0000000000000000000000000000000000000000");
        assert!(result.is_err(), "bogus SHA should return Err, not panic");
    }
}
