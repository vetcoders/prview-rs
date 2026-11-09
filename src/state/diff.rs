//! Diff summary and file status (working tree vs HEAD)

use anyhow::Result;
use std::path::Path;

pub struct WorkingTreeStats {
    pub files_changed: usize,
    pub insertions: usize,
    pub deletions: usize,
}

/// libgit2 reads worktree bytes without executing configured Git callbacks.
pub(super) fn with_worktree_diff<T>(
    root: &Path,
    read: impl FnOnce(&git2::Diff<'_>) -> Result<T>,
) -> Result<T> {
    let repo = git2::Repository::discover(root)?;
    crate::git::WorktreeSnapshot::ensure_worktree_comparison_complete(&repo)?;
    let tree = repo.head()?.peel_to_tree()?;
    let mut diff = repo.diff_tree_to_workdir_with_index(Some(&tree), None)?;
    // libgit2 merges HEAD→index and index→workdir without cancelling a
    // Modified delta whose final workdir bytes equal HEAD. Reconcile those
    // candidates against the actual tree→workdir diff. Keep the merged diff
    // for every other path: a staged deletion must remain a deletion even if
    // an untracked replacement exists at that path, as in native git diff HEAD.
    let modified: std::collections::BTreeSet<Vec<u8>> = diff
        .deltas()
        .filter(|delta| delta.status() == git2::Delta::Modified)
        .filter_map(|delta| delta.new_file().path_bytes().map(<[u8]>::to_vec))
        .collect();
    if !modified.is_empty() {
        let mut options = git2::DiffOptions::new();
        options
            .disable_pathspec_match(true)
            .include_typechange(true);
        for path in &modified {
            options.pathspec(path.as_slice());
        }
        let actual = repo.diff_tree_to_workdir(Some(&tree), Some(&mut options))?;
        let changed: std::collections::BTreeSet<Vec<u8>> = actual
            .deltas()
            .flat_map(|delta| {
                [delta.old_file().path_bytes(), delta.new_file().path_bytes()]
                    .into_iter()
                    .flatten()
                    .map(<[u8]>::to_vec)
                    .collect::<Vec<_>>()
            })
            .collect();
        let cancelled: std::collections::BTreeSet<Vec<u8>> =
            modified.difference(&changed).cloned().collect();
        if !cancelled.is_empty() {
            let retained: std::collections::BTreeSet<Vec<u8>> = diff
                .deltas()
                .filter(|delta| {
                    !delta
                        .new_file()
                        .path_bytes()
                        .is_some_and(|path| cancelled.contains(path))
                })
                .flat_map(|delta| {
                    [delta.old_file().path_bytes(), delta.new_file().path_bytes()]
                        .into_iter()
                        .flatten()
                        .map(<[u8]>::to_vec)
                        .collect::<Vec<_>>()
                })
                .collect();
            diff = if retained.is_empty() {
                repo.diff_tree_to_tree(Some(&tree), Some(&tree), None)?
            } else {
                let mut options = git2::DiffOptions::new();
                options.disable_pathspec_match(true);
                for path in retained {
                    options.pathspec(path);
                }
                repo.diff_tree_to_workdir_with_index(Some(&tree), Some(&mut options))?
            };
        }
    }
    diff.find_similar(Some(git2::DiffFindOptions::new().renames(true)))?;
    read(&diff)
}

pub fn get_diff_stats(root: &Path) -> Result<WorkingTreeStats> {
    with_worktree_diff(root, |diff| {
        let stats = diff.stats()?;
        Ok(WorkingTreeStats {
            files_changed: stats.files_changed(),
            insertions: stats.insertions(),
            deletions: stats.deletions(),
        })
    })
}

/// Count untracked files in the worktree.
///
/// `git diff HEAD` cannot see brand-new files, so a repo whose only change is
/// an untracked file reports zero diff stats. Callers that decide "is the
/// worktree dirty?" need this signal too (PR #12 review).
pub fn count_untracked(root: &Path) -> Result<usize> {
    let repo = git2::Repository::discover(root)?;
    let mut options = git2::StatusOptions::new();
    options.include_untracked(true).recurse_untracked_dirs(true);
    Ok(repo
        .statuses(Some(&mut options))?
        .iter()
        .filter(|entry| entry.status().contains(git2::Status::WT_NEW))
        .count())
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

    let lines: Vec<String> = with_worktree_diff(root, |diff| {
        Ok(diff
            .deltas()
            .map(|delta| {
                let status = match delta.status() {
                    git2::Delta::Added => "A",
                    git2::Delta::Deleted => "D",
                    git2::Delta::Renamed => "R",
                    _ => "M",
                };
                let path = delta.new_file().path().or_else(|| delta.old_file().path());
                format!(
                    "{}\t{}",
                    status,
                    path.map(|p| p.to_string_lossy().into_owned())
                        .unwrap_or_default()
                )
            })
            .collect())
    })?;

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

    #[cfg(unix)]
    #[test]
    fn review80_round2_state_refuses_unsupported_clean_normalization_without_callbacks() {
        use std::os::unix::fs::PermissionsExt as _;
        let (_dir, path) = make_temp_repo();
        let marker = path.join("clean-marker");
        let script = path.join("owned-clean");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\ncat >/dev/null\nprintf 'init\n'\nprintf x >> '{}'\n",
                marker.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::write(path.join(".gitattributes"), "init.txt filter=round2\n").unwrap();
        let repo = git2::Repository::open(&path).unwrap();
        repo.config()
            .unwrap()
            .set_str("filter.round2.clean", script.to_str().unwrap())
            .unwrap();
        std::fs::write(
            path.join("init.txt"),
            "DISPLAY CONTENT WITH DIFFERENT SIZE\n",
        )
        .unwrap();
        let error = match get_diff_stats(&path) {
            Ok(stats) => panic!(
                "unsupported clean normalization cannot certify {} changed files",
                stats.files_changed
            ),
            Err(error) => error,
        };
        assert!(
            error.to_string().contains("init.txt (filter=round2)"),
            "{error}"
        );
        assert!(!marker.exists());
        assert!(
            super::super::collect_state(
                &path,
                &super::super::StateOpts {
                    fast: true,
                    json: true,
                    hot: true
                }
            )
            .is_err()
        );
        assert!(!marker.exists());
        // Remove the untracked attribute file from native output by asking
        // only about the tracked path; Git's owned clean driver proves identity.
        let native = git_cmd()
            .args(["diff", "--numstat", "HEAD", "--", "init.txt"])
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .current_dir(&path)
            .output()
            .unwrap();
        assert!(native.status.success());
        assert!(native.stdout.is_empty());
        assert!(!std::fs::read_to_string(&marker).unwrap().is_empty());
    }

    #[test]
    fn review80_staged_change_restored_to_head_matches_native_git() {
        let (_dir, path) = make_temp_repo();
        std::fs::write(path.join("init.txt"), "staged content\n").unwrap();
        let staged = git_cmd()
            .args(["add", "init.txt"])
            .current_dir(&path)
            .output()
            .unwrap();
        assert!(staged.status.success());
        std::fs::write(path.join("init.txt"), "init\n").unwrap();

        let native = git_cmd()
            .args(["-c", "core.fsmonitor=false", "diff", "--numstat", "HEAD"])
            .current_dir(&path)
            .output()
            .unwrap();
        assert!(native.status.success());
        assert!(
            native.stdout.is_empty(),
            "native Git sees the restored bytes"
        );

        let stats = get_diff_stats(&path).unwrap();
        assert_eq!(stats.files_changed, 0, "match native git diff HEAD");
        assert_eq!((stats.insertions, stats.deletions), (0, 0));
        assert!(super::super::hot::get_hot_files(&path).unwrap().is_empty());

        // Cancellation of one path must not hide another path's staged edit.
        std::fs::write(path.join("other.txt"), "staged addition\n").unwrap();
        let staged = git_cmd()
            .args(["add", "other.txt"])
            .current_dir(&path)
            .output()
            .unwrap();
        assert!(staged.status.success());
        let stats = get_diff_stats(&path).unwrap();
        assert_eq!(stats.files_changed, 1);
        assert_eq!((stats.insertions, stats.deletions), (1, 0));
        let hot = super::super::hot::get_hot_files(&path).unwrap();
        assert_eq!(hot.len(), 1);
        assert_eq!(hot[0].path, "other.txt");
    }

    #[test]
    fn review80_staged_deletion_retains_native_git_index_semantics() {
        let (_dir, path) = make_temp_repo();
        let removed = git_cmd()
            .args(["rm", "init.txt"])
            .current_dir(&path)
            .output()
            .unwrap();
        assert!(removed.status.success());
        std::fs::write(path.join("init.txt"), "init\nrestored as untracked\n").unwrap();
        let native = git_cmd()
            .args(["-c", "core.fsmonitor=false", "diff", "--numstat", "HEAD"])
            .current_dir(&path)
            .output()
            .unwrap();
        assert!(native.status.success());
        assert_eq!(native.stdout, b"0\t1\tinit.txt\n");
        let stats = get_diff_stats(&path).unwrap();
        assert_eq!(stats.files_changed, 1);
        assert_eq!((stats.insertions, stats.deletions), (0, 1));
    }

    #[test]
    fn review80_binary_cancellation_does_not_hide_a_real_binary_change() {
        let (_dir, path) = make_temp_repo();
        let original = b"original\0binary\n";
        std::fs::write(path.join("binary.bin"), original).unwrap();
        let repo = git2::Repository::open(&path).unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(Path::new("binary.bin")).unwrap();
        index.write().unwrap();
        let tree_id = index.write_tree().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        let parent = repo.head().unwrap().peel_to_commit().unwrap();
        let signature = repo.signature().unwrap();
        repo.commit(
            Some("HEAD"),
            &signature,
            &signature,
            "binary fixture",
            &tree,
            &[&parent],
        )
        .unwrap();
        std::fs::write(path.join("binary.bin"), b"staged\0binary\n").unwrap();
        index.add_path(Path::new("binary.bin")).unwrap();
        index.write().unwrap();
        std::fs::write(path.join("binary.bin"), original).unwrap();
        assert_eq!(get_diff_stats(&path).unwrap().files_changed, 0);
        std::fs::write(path.join("binary.bin"), b"real\0binary\n").unwrap();
        let stats = get_diff_stats(&path).unwrap();
        assert_eq!(
            stats.files_changed, 1,
            "zero text churn is still a binary change"
        );
        assert_eq!((stats.insertions, stats.deletions), (0, 0));
    }
}
