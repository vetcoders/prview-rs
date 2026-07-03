//! Fast repo state probe (`prview state`)
//!
//! Lightweight repo inspection: branch, HEAD, diff stats, file status, tree.
//! No checks, no heuristics, no snapshots, no artifact generation.

mod diff;
pub mod hot;
mod repo;
mod tree;

use anyhow::Result;
use std::path::Path;

/// Options for `prview state`
pub struct StateOpts {
    pub fast: bool,
    pub json: bool,
    pub hot: bool,
}

/// Structured repo state — usable by both CLI and TUI
#[derive(Debug, Clone)]
pub struct RepoState {
    pub repo: String,
    pub branch: String,
    pub head: String,
    pub files_changed: usize,
    pub insertions: usize,
    pub deletions: usize,
    /// Untracked files in the worktree (invisible to `git diff HEAD`).
    pub untracked_files: usize,
    pub hot_files: Vec<hot::HotFile>,
}

impl RepoState {
    /// The worktree differs from HEAD, including brand-new untracked files that
    /// diff stats alone cannot see (PR #12 review).
    pub fn is_dirty(&self) -> bool {
        self.files_changed > 0
            || self.insertions > 0
            || self.deletions > 0
            || self.untracked_files > 0
    }
}

/// Collect structured repo state
pub fn collect_state(root: &Path, opts: &StateOpts) -> Result<RepoState> {
    let info = repo::get_repo_info(root)?;
    let stats = diff::get_diff_stats(root)?;
    let untracked_files = diff::count_untracked(root)?;

    let hot_files = if opts.hot {
        hot::get_hot_files(root)?
    } else {
        Vec::new()
    };

    Ok(RepoState {
        repo: info.name,
        branch: info.branch,
        head: info.short_sha,
        files_changed: stats.files_changed,
        insertions: stats.insertions,
        deletions: stats.deletions,
        untracked_files,
        hot_files,
    })
}

/// Run the state probe
pub fn run(root: &Path, opts: &StateOpts) -> Result<()> {
    if opts.json {
        return run_json(root, opts);
    }

    repo::print_repo_info(root)?;
    diff::print_diff_summary(root)?;
    diff::print_file_status(root)?;

    if opts.hot {
        hot::print_hot_files(root)?;
    }

    if !opts.fast {
        tree::print_tree(root)?;
    }

    Ok(())
}

/// JSON output mode
fn run_json(root: &Path, opts: &StateOpts) -> Result<()> {
    let state = collect_state(root, opts)?;

    let mut json = serde_json::json!({
        "repo": state.repo,
        "branch": state.branch,
        "commit": state.head,
        "files_changed": state.files_changed,
        "insertions": state.insertions,
        "deletions": state.deletions,
    });

    if opts.hot {
        let hot_arr: Vec<serde_json::Value> = state
            .hot_files
            .iter()
            .take(hot::HOT_FILES_DISPLAY_LIMIT)
            .map(|h| {
                serde_json::json!({
                    "path": h.path,
                    "lines": h.lines,
                })
            })
            .collect();
        json["hot_files"] = serde_json::Value::Array(hot_arr);
    }

    println!("{}", serde_json::to_string_pretty(&json)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::git_cmd;

    /// Build JSON value (testable without stdout capture)
    fn build_json(root: &Path) -> Result<serde_json::Value> {
        let info = repo::get_repo_info(root)?;
        let stats = diff::get_diff_stats(root)?;

        Ok(serde_json::json!({
            "repo": info.name,
            "branch": info.branch,
            "commit": info.short_sha,
            "files_changed": stats.files_changed,
            "insertions": stats.insertions,
            "deletions": stats.deletions,
        }))
    }

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

    /// PR #12 review: a worktree whose only change is an untracked file has zero
    /// diff stats but must still count as dirty, so `state` does not report a
    /// fresh file as invisible.
    #[test]
    fn untracked_only_worktree_is_dirty() {
        let (_dir, path) = make_temp_repo();
        std::fs::write(path.join("brand-new.txt"), "hello\n").unwrap();

        let state = collect_state(
            &path,
            &StateOpts {
                fast: true,
                json: true,
                hot: false,
            },
        )
        .unwrap();

        assert_eq!(state.files_changed, 0, "diff stats cannot see untracked");
        assert!(state.untracked_files >= 1, "untracked file must be counted");
        assert!(state.is_dirty(), "untracked-only worktree must be dirty");
    }

    #[test]
    fn test_run_default_opts_clean_repo() {
        let (_dir, path) = make_temp_repo();
        let opts = StateOpts {
            fast: true,
            json: false,
            hot: false,
        };
        // Should succeed on a clean repo
        let result = run(&path, &opts);
        assert!(result.is_ok(), "run() failed: {:?}", result.err());
    }

    #[test]
    fn test_run_with_changes() {
        let (_dir, path) = make_temp_repo();

        std::fs::write(path.join("init.txt"), "modified\n").unwrap();

        let opts = StateOpts {
            fast: true,
            json: false,
            hot: false,
        };
        let result = run(&path, &opts);
        assert!(result.is_ok(), "run() failed: {:?}", result.err());
    }

    #[test]
    fn test_run_full_mode_with_tree() {
        let (_dir, path) = make_temp_repo();

        // Create some directory structure
        std::fs::create_dir_all(path.join("src")).unwrap();
        std::fs::write(path.join("src/main.rs"), "fn main() {}\n").unwrap();

        let opts = StateOpts {
            fast: false, // includes tree
            json: false,
            hot: false,
        };
        let result = run(&path, &opts);
        assert!(result.is_ok(), "run() with tree failed: {:?}", result.err());
    }

    #[test]
    fn test_build_json_structure() {
        let (_dir, path) = make_temp_repo();
        let json = build_json(&path).unwrap();

        assert!(json.is_object());
        let obj = json.as_object().unwrap();

        // All expected fields present
        assert!(obj.contains_key("repo"), "missing 'repo' field");
        assert!(obj.contains_key("branch"), "missing 'branch' field");
        assert!(obj.contains_key("commit"), "missing 'commit' field");
        assert!(
            obj.contains_key("files_changed"),
            "missing 'files_changed' field"
        );
        assert!(obj.contains_key("insertions"), "missing 'insertions' field");
        assert!(obj.contains_key("deletions"), "missing 'deletions' field");

        // Type checks
        assert!(obj["repo"].is_string());
        assert!(obj["branch"].is_string());
        assert!(obj["commit"].is_string());
        assert!(obj["files_changed"].is_number());
        assert!(obj["insertions"].is_number());
        assert!(obj["deletions"].is_number());
    }

    #[test]
    fn test_build_json_values_clean_repo() {
        let (_dir, path) = make_temp_repo();
        let json = build_json(&path).unwrap();

        assert_eq!(json["branch"], "main");
        assert_eq!(json["files_changed"], 0);
        assert_eq!(json["insertions"], 0);
        assert_eq!(json["deletions"], 0);
        assert_eq!(json["commit"].as_str().unwrap().len(), 7);
    }

    #[test]
    fn test_build_json_values_with_changes() {
        let (_dir, path) = make_temp_repo();

        std::fs::write(path.join("init.txt"), "changed\nextra\n").unwrap();

        let json = build_json(&path).unwrap();
        assert_eq!(json["files_changed"], 1);
        assert_eq!(json["insertions"], 2);
        assert_eq!(json["deletions"], 1);
    }

    #[test]
    fn test_run_json_mode() {
        let (_dir, path) = make_temp_repo();
        let opts = StateOpts {
            fast: false,
            json: true,
            hot: false,
        };
        // run_json prints to stdout — just verify it does not error
        let result = run(&path, &opts);
        assert!(result.is_ok(), "run() json mode failed: {:?}", result.err());
    }

    #[test]
    fn test_run_on_non_repo_fails() {
        let dir = tempfile::tempdir().expect("tempdir");
        let opts = StateOpts {
            fast: true,
            json: false,
            hot: false,
        };
        let result = run(dir.path(), &opts);
        assert!(result.is_err(), "run() should fail on non-git dir");
    }

    #[test]
    fn test_collect_state_basic() {
        let (_dir, path) = make_temp_repo();
        let opts = StateOpts {
            fast: true,
            json: false,
            hot: false,
        };
        let state = collect_state(&path, &opts).unwrap();
        assert_eq!(state.branch, "main");
        assert_eq!(state.head.len(), 7);
        assert_eq!(state.files_changed, 0);
        assert!(state.hot_files.is_empty());
    }

    #[test]
    fn test_collect_state_with_hot() {
        let (_dir, path) = make_temp_repo();

        std::fs::write(path.join("init.txt"), "changed\n").unwrap();

        let opts = StateOpts {
            fast: true,
            json: false,
            hot: true,
        };
        let state = collect_state(&path, &opts).unwrap();
        assert_eq!(state.files_changed, 1);
        assert!(!state.hot_files.is_empty());
    }

    #[test]
    fn test_run_with_hot() {
        let (_dir, path) = make_temp_repo();

        std::fs::write(path.join("init.txt"), "changed\n").unwrap();

        let opts = StateOpts {
            fast: true,
            json: false,
            hot: true,
        };
        let result = run(&path, &opts);
        assert!(
            result.is_ok(),
            "run() with --hot failed: {:?}",
            result.err()
        );
    }
}
