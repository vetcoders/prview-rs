//! Shared test helpers for signal module tests.

use crate::checks::{CheckResult, CheckStatus};
use crate::git::{Diff, DiffStats, FileChange, FileStatus, Repository};
use std::fs;
use std::path::Path;
use std::time::Duration;
use tempfile::TempDir;

pub(super) fn mock_check(name: &str, status: CheckStatus, output: &str) -> CheckResult {
    CheckResult {
        name: name.to_string(),
        status,
        duration: Duration::from_secs(1),
        output: output.to_string(),
        cached: false,
        provenance: None,
    }
}

pub(super) fn mock_diff(files: Vec<FileChange>) -> Diff {
    Diff {
        base: "main".to_string(),
        target: "feature".to_string(),
        base_commit_id: "abc123".to_string(),
        target_commit_id: "def456".to_string(),
        files,
        stats: DiffStats {
            files_changed: 0,
            additions: 0,
            deletions: 0,
            copied: 0,
        },
        commits: vec![],
    }
}

pub(super) fn mock_file_change(
    path: &str,
    status: FileStatus,
    adds: usize,
    dels: usize,
) -> FileChange {
    FileChange {
        path: path.to_string(),
        status,
        additions: adds,
        deletions: dels,
    }
}

/// Create a temp git repo with two commits: base -> target.
/// Returns (TempDir, Repository, base_commit_id, target_commit_id).
pub(super) fn make_test_repo(
    files: &[(&str, &str, &str)], // (path, old_content, new_content)
) -> (TempDir, Repository, String, String) {
    let tmp = TempDir::new().unwrap();
    let git_repo = git2::Repository::init(tmp.path()).unwrap();
    let sig = git2::Signature::now("Test", "test@test.com").unwrap();

    // Base commit: create files with old_content
    for &(path, old_content, _) in files {
        if let Some(parent) = Path::new(path).parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(tmp.path().join(parent)).unwrap();
        }
        fs::write(tmp.path().join(path), old_content).unwrap();
    }
    let mut index = git_repo.index().unwrap();
    for &(path, _, _) in files {
        index.add_path(Path::new(path)).unwrap();
    }
    index.write().unwrap();
    let tree_id = index.write_tree().unwrap();
    let tree = git_repo.find_tree(tree_id).unwrap();
    let base_oid = git_repo
        .commit(Some("HEAD"), &sig, &sig, "base", &tree, &[])
        .unwrap();

    // Target commit: update files with new_content
    for &(path, _, new_content) in files {
        fs::write(tmp.path().join(path), new_content).unwrap();
    }
    let mut index = git_repo.index().unwrap();
    for &(path, _, _) in files {
        index.add_path(Path::new(path)).unwrap();
    }
    index.write().unwrap();
    let tree_id = index.write_tree().unwrap();
    let tree = git_repo.find_tree(tree_id).unwrap();
    let parent = git_repo.find_commit(base_oid).unwrap();
    let target_oid = git_repo
        .commit(Some("HEAD"), &sig, &sig, "target", &tree, &[&parent])
        .unwrap();

    let repo = Repository::open(tmp.path()).unwrap();
    (tmp, repo, base_oid.to_string(), target_oid.to_string())
}

pub(super) fn make_diff_with_ids(
    base_id: String,
    target_id: String,
    files: Vec<FileChange>,
) -> Diff {
    Diff {
        base: "main".to_string(),
        target: "feature".to_string(),
        base_commit_id: base_id,
        target_commit_id: target_id,
        files,
        stats: DiffStats::default(),
        commits: vec![],
    }
}
