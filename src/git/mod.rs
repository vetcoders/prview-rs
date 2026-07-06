//! Git operations using libgit2
//!
//! Provides repository access, diff generation, and commit analysis.

pub mod cmd;
pub use cmd::git_cmd;

mod snapshot;
pub use snapshot::AnalysisSnapshot;

use crate::Config;
use anyhow::{Context, Result};
use git2::{DiffOptions, Repository as Git2Repo};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Minimum similarity percentage for rename/copy detection (like `git diff -M50`).
const RENAME_SIMILARITY_THRESHOLD: u16 = 50;

/// Maximum number of commits loaded by `commits_between()`.
/// PRs with more commits are truncated: first and last commits are always preserved.
/// Aligned with `MAX_COMMITS_FOR_PER_COMMIT_DIFFS` in artifacts (50 << 500).
pub const MAX_COMMITS: usize = 500;

const TRUNCATED_COMMIT_SENTINEL_ID: &str = "0000000000000000000000000000000000000000";
const TRUNCATED_COMMIT_SENTINEL_SHORT_ID: &str = "0000000";

/// Truncate a SHA to 7 chars safely (returns full string if shorter).
pub fn short_sha(sha: &str) -> &str {
    if sha.len() >= 7 { &sha[..7] } else { sha }
}

/// Wrapper around git2::Repository with prview-specific operations
pub struct Repository {
    inner: Git2Repo,
    path: PathBuf,
}

/// A resolved git reference (branch/tag/commit)
#[derive(Debug, Clone)]
pub struct ResolvedRef {
    pub name: String,
    pub commit_id: String,
    pub is_remote: bool,
}

/// Diff between two refs
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Diff {
    pub base: String,
    pub target: String,
    pub base_commit_id: String,
    pub target_commit_id: String,
    pub files: Vec<FileChange>,
    pub stats: DiffStats,
    pub commits: Vec<CommitInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileChange {
    pub path: String,
    pub status: FileStatus,
    pub additions: usize,
    pub deletions: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FileStatus {
    Added,
    Modified,
    Deleted,
    Renamed,
    Copied,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct DiffStats {
    pub files_changed: usize,
    pub additions: usize,
    pub deletions: usize,
    pub copied: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommitInfo {
    pub id: String,
    pub short_id: String,
    pub author: String,
    pub email: String,
    pub date: String,
    pub message: String,
}

/// Information about a single branch
#[derive(Debug, Clone)]
pub struct BranchInfo {
    pub name: String,
    pub is_current: bool,
    pub is_remote: bool,
    pub remote_name: Option<String>,
}

/// List of branches in a repository
#[derive(Debug, Clone)]
pub struct BranchList {
    pub local: Vec<BranchInfo>,
    pub remote: Vec<BranchInfo>,
    pub current: Option<String>,
}

impl Repository {
    /// Open repository at path
    pub fn open(path: &Path) -> Result<Self> {
        let inner = Git2Repo::open(path)
            .with_context(|| format!("Failed to open git repository at {}", path.display()))?;

        Ok(Self {
            inner,
            path: path.to_path_buf(),
        })
    }

    /// Refresh refs needed for the current run when fetch is enabled.
    pub fn prepare_refs(&self, config: &Config) -> Result<()> {
        if !config.do_fetch || self.inner.find_remote("origin").is_err() {
            return Ok(());
        }

        let fetch_origin = git_cmd()
            .args(["fetch", "--quiet", "--prune", "origin"])
            .current_dir(&self.path)
            .status()
            .context("Failed to run git fetch origin")?;

        if !fetch_origin.success() {
            anyhow::bail!("git fetch origin failed with status {}", fetch_origin);
        }

        if let Some(pr_number) = config.pr_number {
            let pr_ref = format!("pull/{pr_number}/head:refs/remotes/origin/pr/{pr_number}");
            let fetch_pr = git_cmd()
                .args(["fetch", "--quiet", "origin", &pr_ref])
                .current_dir(&self.path)
                .status()
                .with_context(|| format!("Failed to fetch PR ref for #{pr_number}"))?;

            if !fetch_pr.success() {
                anyhow::bail!("git fetch PR ref failed with status {}", fetch_pr);
            }
        }

        Ok(())
    }

    /// Commit id currently checked out (`HEAD`).
    ///
    /// Used to decide whether a diff-scoped tool that reads the working tree
    /// (e.g. semgrep `--baseline-commit`) may trust that tree as the analysed
    /// target: baseline scans only make sense when the analysed target is the
    /// commit actually checked out.
    pub fn head_commit_id(&self) -> Result<String> {
        let commit = self.inner.head()?.peel_to_commit()?;
        Ok(commit.id().to_string())
    }

    /// Resolve target branch/ref
    pub fn resolve_target(&self, config: &Config) -> Result<ResolvedRef> {
        let name = config.target.clone().unwrap_or_else(|| {
            self.inner
                .head()
                .ok()
                .and_then(|h| h.shorthand().map(String::from))
                .unwrap_or_else(|| "HEAD".to_string())
        });

        if let Some(pr_number) = config.pr_number {
            if let Some(commit_id) = config.pr_head_oid.as_deref()
                && self.resolve_ref(commit_id).is_ok()
            {
                return Ok(ResolvedRef {
                    name,
                    commit_id: commit_id.to_string(),
                    is_remote: true,
                });
            }

            let pr_ref = format!("origin/pr/{pr_number}");
            if let Ok(commit_id) = self.resolve_ref(&pr_ref) {
                return Ok(ResolvedRef {
                    name,
                    commit_id,
                    is_remote: true,
                });
            }
        }

        let (commit_id, is_remote) = if config.remote_mode {
            let remote_name = format!("origin/{}", name);
            let oid = self.resolve_ref(&remote_name)?;
            (oid, true)
        } else {
            let oid = self.resolve_ref(&name)?;
            (oid, false)
        };

        Ok(ResolvedRef {
            name,
            commit_id,
            is_remote,
        })
    }

    /// Resolve base branches
    pub fn resolve_bases(&self, config: &Config) -> Result<Vec<ResolvedRef>> {
        let mut resolved = Vec::new();

        for base in &config.bases {
            if let Some(pr_base_oid) = config.pr_base_oid.as_deref()
                && self.resolve_ref(pr_base_oid).is_ok()
            {
                resolved.push(ResolvedRef {
                    name: base.clone(),
                    commit_id: pr_base_oid.to_string(),
                    is_remote: true,
                });
                continue;
            }

            // Try local first, then remote
            let result = if config.remote_only {
                self.resolve_remote_ref(base)
            } else if config.local_only {
                self.resolve_ref(base).map(|id| (id, false))
            } else {
                // Auto-fallback: try local, then remote
                self.resolve_ref(base)
                    .map(|id| (id, false))
                    .or_else(|_| self.resolve_remote_ref(base))
            };

            match result {
                Ok((commit_id, is_remote)) => {
                    resolved.push(ResolvedRef {
                        name: base.clone(),
                        commit_id,
                        is_remote,
                    });
                }
                Err(e) => {
                    if !config.quiet {
                        eprintln!("Warning: Could not resolve base '{}': {}", base, e);
                    }
                }
            }
        }

        Ok(resolved)
    }

    /// Generate diffs between target and all bases
    /// Skips bases that have the same commit as target (would produce empty diff)
    pub fn generate_diffs(
        &self,
        target: &ResolvedRef,
        bases: &[ResolvedRef],
        quiet: bool,
    ) -> Result<Vec<Diff>> {
        use colored::Colorize;
        let mut diffs = Vec::new();

        for base in bases {
            // Skip if base and target point to same commit (empty diff)
            if base.commit_id == target.commit_id {
                if !quiet {
                    eprintln!(
                        "  {} Skipping '{}' - same commit as target ({})",
                        "ℹ".blue(),
                        base.name,
                        short_sha(&base.commit_id)
                    );
                }
                continue;
            }
            let diff = self.diff_refs(base, target)?;
            diffs.push(diff);
        }

        Ok(diffs)
    }

    /// Generate diff between two refs
    pub(crate) fn diff_refs(&self, base: &ResolvedRef, target: &ResolvedRef) -> Result<Diff> {
        let base_commit = self
            .inner
            .find_commit(git2::Oid::from_str(&base.commit_id)?)?;
        let target_commit = self
            .inner
            .find_commit(git2::Oid::from_str(&target.commit_id)?)?;

        let base_tree = base_commit.tree()?;
        let target_tree = target_commit.tree()?;

        let mut opts = DiffOptions::new();
        opts.patience(true);
        opts.context_lines(3);

        let mut diff =
            self.inner
                .diff_tree_to_tree(Some(&base_tree), Some(&target_tree), Some(&mut opts))?;

        // Enable rename/copy detection (like git diff -M -C)
        let mut find_opts = git2::DiffFindOptions::new();
        find_opts.renames(true);
        find_opts.copies(true);
        find_opts.rename_threshold(RENAME_SIMILARITY_THRESHOLD);
        diff.find_similar(Some(&mut find_opts))?;

        // Collect file changes with per-file line stats
        let mut files = Vec::new();
        let num_deltas = diff.deltas().len();
        for i in 0..num_deltas {
            let Some(delta) = diff.get_delta(i) else {
                continue;
            };
            let path = delta
                .new_file()
                .path()
                .or_else(|| delta.old_file().path())
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_default();

            let status = match delta.status() {
                git2::Delta::Added => FileStatus::Added,
                git2::Delta::Deleted => FileStatus::Deleted,
                git2::Delta::Modified => FileStatus::Modified,
                git2::Delta::Renamed => FileStatus::Renamed,
                git2::Delta::Copied => FileStatus::Copied,
                _ => FileStatus::Modified,
            };

            // Extract per-file additions/deletions from patch
            let (additions, deletions) = match git2::Patch::from_diff(&diff, i) {
                Ok(Some(patch)) => {
                    let (_, adds, dels) = patch.line_stats().unwrap_or((0, 0, 0));
                    (adds, dels)
                }
                _ => (0, 0),
            };

            files.push(FileChange {
                path,
                status,
                additions,
                deletions,
            });
        }

        // Get stats
        let diff_stats = diff.stats()?;
        let copied_count = files
            .iter()
            .filter(|f| f.status == FileStatus::Copied)
            .count();
        let stats = DiffStats {
            files_changed: diff_stats.files_changed(),
            additions: diff_stats.insertions(),
            deletions: diff_stats.deletions(),
            copied: copied_count,
        };

        // Get commits in range
        let commits = self.commits_between(&base.commit_id, &target.commit_id)?;

        Ok(Diff {
            base: base.name.clone(),
            target: target.name.clone(),
            base_commit_id: base.commit_id.clone(),
            target_commit_id: target.commit_id.clone(),
            files,
            stats,
            commits,
        })
    }

    /// Get commits between two refs.
    ///
    /// At most `MAX_COMMITS` entries are returned. When the real commit count exceeds
    /// that limit the list is truncated to the first `MAX_COMMITS - 2` commits plus a
    /// sentinel entry and the very last commit, so callers always have both ends of the
    /// range and a visible note about the omitted middle section.
    pub(crate) fn commits_between(
        &self,
        base_id: &str,
        target_id: &str,
    ) -> Result<Vec<CommitInfo>> {
        let mut revwalk = self.inner.revwalk()?;

        let target_oid = git2::Oid::from_str(target_id)?;
        let base_oid = git2::Oid::from_str(base_id)?;

        revwalk.push(target_oid)?;
        revwalk.hide(base_oid)?;
        revwalk.set_sorting(git2::Sort::TOPOLOGICAL | git2::Sort::REVERSE)?;

        let mut commits = Vec::new();

        for oid in revwalk {
            let oid = oid?;
            let commit = self.inner.find_commit(oid)?;

            let info = CommitInfo {
                id: oid.to_string(),
                short_id: {
                    let full = oid.to_string();
                    short_sha(&full).to_string()
                },
                author: commit.author().name().unwrap_or("Unknown").to_string(),
                email: commit.author().email().unwrap_or("").to_string(),
                date: chrono::DateTime::from_timestamp(commit.time().seconds(), 0)
                    .map(|dt| dt.format("%Y-%m-%dT%H:%M:%S").to_string())
                    .unwrap_or_default(),
                message: commit.summary().unwrap_or("").to_string(),
            };

            commits.push(info);
        }

        truncate_commit_list(&mut commits);

        Ok(commits)
    }

    /// Resolve a ref name to commit ID
    fn resolve_ref(&self, name: &str) -> Result<String> {
        // Try as branch
        if let Ok(branch) = self.inner.find_branch(name, git2::BranchType::Local)
            && let Some(target) = branch.get().target()
        {
            return Ok(target.to_string());
        }

        // Try as reference
        if let Ok(reference) = self.inner.find_reference(name)
            && let Some(target) = reference.target()
        {
            return Ok(target.to_string());
        }

        // Try as commit
        if let Ok(obj) = self.inner.revparse_single(name) {
            return Ok(obj.id().to_string());
        }

        anyhow::bail!("Could not resolve ref: {}", name)
    }

    /// Resolve a remote ref
    fn resolve_remote_ref(&self, name: &str) -> Result<(String, bool)> {
        let remote_name = if name.starts_with("origin/") {
            name.to_string()
        } else {
            format!("origin/{}", name)
        };

        // Try as remote branch
        if let Ok(branch) = self
            .inner
            .find_branch(&remote_name, git2::BranchType::Remote)
            && let Some(target) = branch.get().target()
        {
            return Ok((target.to_string(), true));
        }

        // Try refs/remotes/
        let ref_name = format!("refs/remotes/{}", remote_name);
        if let Ok(reference) = self.inner.find_reference(&ref_name)
            && let Some(target) = reference.target()
        {
            return Ok((target.to_string(), true));
        }

        anyhow::bail!("Could not resolve remote ref: {}", remote_name)
    }

    /// Generate patch for a single commit
    pub fn commit_patch(&self, commit_id: &str) -> Result<String> {
        let oid = git2::Oid::from_str(commit_id)?;
        let commit = self.inner.find_commit(oid)?;

        let parent = commit.parent(0).ok();
        let parent_tree = parent.as_ref().map(|p| p.tree()).transpose()?;
        let commit_tree = commit.tree()?;

        let diff = self
            .inner
            .diff_tree_to_tree(parent_tree.as_ref(), Some(&commit_tree), None)?;

        Ok(Self::diff_to_patch(&diff))
    }

    /// Compute the merge-base (best common ancestor) of two commits.
    ///
    /// Diffing from the merge-base — rather than the base tip — is what keeps a
    /// review pack showing only the target's own changes when the base branch
    /// has moved on independently after divergence.
    pub fn merge_base(&self, a: &str, b: &str) -> Result<String> {
        let a_oid = git2::Oid::from_str(a)?;
        let b_oid = git2::Oid::from_str(b)?;
        Ok(self.inner.merge_base(a_oid, b_oid)?.to_string())
    }

    /// True if the commit is a merge commit (more than one parent).
    pub fn is_merge_commit(&self, commit_id: &str) -> Result<bool> {
        let oid = git2::Oid::from_str(commit_id)?;
        let commit = self.inner.find_commit(oid)?;
        Ok(commit.parent_count() > 1)
    }

    /// Generate patch for a single commit, filtered to specific file paths.
    pub fn commit_patch_scoped(&self, commit_id: &str, paths: &[&str]) -> Result<String> {
        let oid = git2::Oid::from_str(commit_id)?;
        let commit = self.inner.find_commit(oid)?;

        let parent = commit.parent(0).ok();
        let parent_tree = parent.as_ref().map(|p| p.tree()).transpose()?;
        let commit_tree = commit.tree()?;

        let mut opts = DiffOptions::new();
        for path in paths {
            opts.pathspec(path);
        }

        let diff = self.inner.diff_tree_to_tree(
            parent_tree.as_ref(),
            Some(&commit_tree),
            Some(&mut opts),
        )?;

        Ok(Self::diff_to_patch(&diff))
    }

    /// True if the commit's first-parent diff touches at least one of `paths`.
    ///
    /// Lightweight membership test: it inspects the delta count only and never
    /// formats a patch, so it is far cheaper than calling [`commit_patch_scoped`]
    /// just to check emptiness. With an empty `paths` slice no pathspec is set,
    /// so any non-empty commit is reported as touching (mirrors the pathspec
    /// semantics of `commit_patch_scoped`).
    pub fn commit_touches_paths(&self, commit_id: &str, paths: &[&str]) -> Result<bool> {
        let oid = git2::Oid::from_str(commit_id)?;
        let commit = self.inner.find_commit(oid)?;

        let parent = commit.parent(0).ok();
        let parent_tree = parent.as_ref().map(|p| p.tree()).transpose()?;
        let commit_tree = commit.tree()?;

        let mut opts = DiffOptions::new();
        for path in paths {
            opts.pathspec(path);
        }

        let diff = self.inner.diff_tree_to_tree(
            parent_tree.as_ref(),
            Some(&commit_tree),
            Some(&mut opts),
        )?;

        Ok(diff.deltas().len() > 0)
    }

    /// Generate WIP diff (HEAD vs working tree including staged + unstaged), filtered to paths.
    /// Returns empty string if no WIP changes match the given paths.
    pub fn wip_diff_scoped(&self, paths: &[&str]) -> Result<String> {
        let head = self.inner.head()?.peel_to_tree()?;

        let mut opts = DiffOptions::new();
        for path in paths {
            opts.pathspec(path);
        }

        // HEAD → index (staged changes)
        let staged = self
            .inner
            .diff_tree_to_index(Some(&head), None, Some(&mut opts))?;

        // index → workdir (unstaged changes). Include untracked files — and
        // their content — so a brand-new, never-added file shows up under --wip
        // with its full added lines, not just a bare header.
        let mut opts2 = DiffOptions::new();
        opts2.include_untracked(true);
        opts2.recurse_untracked_dirs(true);
        opts2.show_untracked_content(true);
        for path in paths {
            opts2.pathspec(path);
        }
        let unstaged = self.inner.diff_index_to_workdir(None, Some(&mut opts2))?;

        let mut patch = Self::diff_to_patch(&staged);
        let unstaged_patch = Self::diff_to_patch(&unstaged);
        if !unstaged_patch.is_empty() {
            if !patch.is_empty() {
                patch.push('\n');
            }
            patch.push_str(&unstaged_patch);
        }

        Ok(patch)
    }

    /// List files with WIP changes (staged + unstaged), filtered to given paths.
    pub fn wip_files_scoped(&self, paths: &[&str]) -> Result<Vec<String>> {
        let head = self.inner.head()?.peel_to_tree()?;

        let mut opts = DiffOptions::new();
        for path in paths {
            opts.pathspec(path);
        }
        let staged = self
            .inner
            .diff_tree_to_index(Some(&head), None, Some(&mut opts))?;

        let mut opts2 = DiffOptions::new();
        opts2.include_untracked(true);
        opts2.recurse_untracked_dirs(true);
        for path in paths {
            opts2.pathspec(path);
        }
        let unstaged = self.inner.diff_index_to_workdir(None, Some(&mut opts2))?;

        let mut files: Vec<String> = Vec::new();
        for diff in [&staged, &unstaged] {
            for i in 0..diff.deltas().len() {
                if let Some(delta) = diff.get_delta(i) {
                    let path = delta
                        .new_file()
                        .path()
                        .or_else(|| delta.old_file().path())
                        .map(|p| p.to_string_lossy().to_string())
                        .unwrap_or_default();
                    if !files.contains(&path) {
                        files.push(path);
                    }
                }
            }
        }

        Ok(files)
    }

    /// Get the repository path
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Read file content at a specific commit/ref
    pub fn file_at_commit(&self, commit_ref: &str, file_path: &str) -> Result<String> {
        let obj = self.inner.revparse_single(commit_ref)?;
        let tree = obj.peel_to_tree()?;
        let safe_path = crate::paths::validate_repo_relative_str(file_path)?;
        let entry = tree.get_path(safe_path)?;
        let blob = self.inner.find_blob(entry.id())?;
        Ok(String::from_utf8_lossy(blob.content()).to_string())
    }

    /// Get diff for a specific file between two refs
    pub fn file_diff(&self, base: &str, target: &str, file_path: &str) -> Result<String> {
        let base_obj = self.inner.revparse_single(base)?;
        let target_obj = self.inner.revparse_single(target)?;
        let safe_path = crate::paths::validate_repo_relative_str(file_path)?;

        let base_tree = base_obj.peel_to_tree()?;
        let target_tree = target_obj.peel_to_tree()?;

        // Create diff with path filter
        let mut opts = git2::DiffOptions::new();
        opts.pathspec(safe_path);

        let diff =
            self.inner
                .diff_tree_to_tree(Some(&base_tree), Some(&target_tree), Some(&mut opts))?;

        Ok(Self::diff_to_patch(&diff))
    }

    /// Get full diff between two refs
    pub fn full_diff(&self, base: &str, target: &str) -> Result<String> {
        let base_obj = self.inner.revparse_single(base)?;
        let target_obj = self.inner.revparse_single(target)?;

        let base_tree = base_obj.peel_to_tree()?;
        let target_tree = target_obj.peel_to_tree()?;

        let mut diff = self
            .inner
            .diff_tree_to_tree(Some(&base_tree), Some(&target_tree), None)?;

        // Enable rename/copy detection (like git diff -M -C)
        let mut find_opts = git2::DiffFindOptions::new();
        find_opts.renames(true);
        find_opts.copies(true);
        find_opts.rename_threshold(RENAME_SIMILARITY_THRESHOLD);
        diff.find_similar(Some(&mut find_opts))?;

        Ok(Self::diff_to_patch(&diff))
    }

    /// Full diff between two refs restricted to `paths`, with rename/copy
    /// detection.
    ///
    /// Unlike per-path [`file_diff`](Self::file_diff), this keeps a rename's
    /// old-path side visible: a rename `old -> new` where `new` is in scope is
    /// emitted as a single rename/delete delta carrying `old`, instead of a bare
    /// addition of `new`. A delta is kept when *either* end is in `paths`, so the
    /// scoped patch never drops the deletion of the pre-rename path.
    pub fn scoped_full_diff(&self, base: &str, target: &str, paths: &[&str]) -> Result<String> {
        let base_obj = self.inner.revparse_single(base)?;
        let target_obj = self.inner.revparse_single(target)?;
        let base_tree = base_obj.peel_to_tree()?;
        let target_tree = target_obj.peel_to_tree()?;

        // No pathspec here: rename detection needs both ends of the diff, and a
        // pathspec on the new path alone would hide the old-path delete before
        // find_similar can pair them. We filter by scope membership afterwards.
        let mut diff = self
            .inner
            .diff_tree_to_tree(Some(&base_tree), Some(&target_tree), None)?;
        let mut find_opts = git2::DiffFindOptions::new();
        find_opts.renames(true);
        find_opts.copies(true);
        find_opts.rename_threshold(RENAME_SIMILARITY_THRESHOLD);
        diff.find_similar(Some(&mut find_opts))?;

        let scope: std::collections::HashSet<&str> = paths.iter().copied().collect();
        let in_scope = |p: Option<&Path>| {
            p.map(|p| scope.contains(p.to_string_lossy().as_ref()))
                .unwrap_or(false)
        };

        let mut patch = Vec::new();
        if let Err(e) = diff.print(git2::DiffFormat::Patch, |delta, _hunk, line| {
            if !in_scope(delta.new_file().path()) && !in_scope(delta.old_file().path()) {
                return true; // skip every line of an out-of-scope delta
            }
            let origin = line.origin();
            if origin == '+' || origin == '-' || origin == ' ' {
                patch.push(origin as u8);
            }
            patch.extend_from_slice(line.content());
            true
        }) {
            eprintln!("[prview] warning: scoped diff patch formatting failed: {e}");
        }
        Ok(String::from_utf8_lossy(&patch).to_string())
    }

    /// Convert a git2 Diff to a unified patch string.
    /// git2's `line.content()` does NOT include the origin character (+/-/space),
    /// so we must prepend it for content lines to produce valid unified diff output.
    fn diff_to_patch(diff: &git2::Diff<'_>) -> String {
        let mut patch = Vec::new();
        if let Err(e) = diff.print(git2::DiffFormat::Patch, |_delta, _hunk, line| {
            let origin = line.origin();
            if origin == '+' || origin == '-' || origin == ' ' {
                patch.push(origin as u8);
            }
            patch.extend_from_slice(line.content());
            true
        }) {
            eprintln!("[prview] warning: diff patch formatting failed: {e}");
        }
        String::from_utf8_lossy(&patch).to_string()
    }

    /// List all branches (local and remote)
    pub fn list_branches(&self) -> Result<BranchList> {
        let mut local = Vec::new();
        let mut remote = Vec::new();

        // Get current branch name
        let current = self
            .inner
            .head()
            .ok()
            .and_then(|h| h.shorthand().map(String::from));

        // List local branches
        for branch_result in self.inner.branches(Some(git2::BranchType::Local))? {
            let (branch, _) = branch_result?;
            if let Some(name) = branch.name()? {
                local.push(BranchInfo {
                    name: name.to_string(),
                    is_current: current.as_deref() == Some(name),
                    is_remote: false,
                    remote_name: None,
                });
            }
        }

        // List remote branches
        for branch_result in self.inner.branches(Some(git2::BranchType::Remote))? {
            let (branch, _) = branch_result?;
            if let Some(name) = branch.name()? {
                // Skip HEAD refs like origin/HEAD
                if name.ends_with("/HEAD") {
                    continue;
                }
                // Extract remote name (e.g., "origin" from "origin/main")
                let remote_name = name.split('/').next().map(String::from);
                // Get the branch name without remote prefix
                let short_name = name.split('/').skip(1).collect::<Vec<_>>().join("/");

                remote.push(BranchInfo {
                    name: short_name,
                    is_current: false,
                    is_remote: true,
                    remote_name,
                });
            }
        }

        // Sort branches alphabetically
        local.sort_by(|a, b| a.name.cmp(&b.name));
        remote.sort_by(|a, b| a.name.cmp(&b.name));

        Ok(BranchList {
            local,
            remote,
            current,
        })
    }

    /// Get the current branch name
    pub fn current_branch(&self) -> Option<String> {
        self.inner
            .head()
            .ok()
            .and_then(|h| h.shorthand().map(String::from))
    }
}

fn truncate_commit_list(commits: &mut Vec<CommitInfo>) {
    if commits.len() <= MAX_COMMITS {
        return;
    }

    let total_before = commits.len();
    let last = commits.pop().expect("len > MAX_COMMITS implies non-empty");
    commits.truncate(MAX_COMMITS - 2);
    let omitted = total_before.saturating_sub(commits.len() + 1);
    commits.push(CommitInfo {
        id: String::from(TRUNCATED_COMMIT_SENTINEL_ID),
        short_id: String::from(TRUNCATED_COMMIT_SENTINEL_SHORT_ID),
        author: String::from("[prview]"),
        email: String::new(),
        date: String::new(),
        message: format!(
            "[commit list truncated: omitted {} commit{} beyond the {} entry limit; last commit follows]",
            omitted,
            if omitted == 1 { "" } else { "s" },
            MAX_COMMITS
        ),
    });
    commits.push(last);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{test_config_builder, test_generic_profile};
    use std::fs;

    fn run_git(repo: &Path, args: &[&str]) {
        let status = git_cmd()
            .args(args)
            .current_dir(repo)
            .status()
            .expect("git command");
        assert!(status.success(), "git {:?} failed with {status}", args);
    }

    fn write_commit(repo: &Path, name: &str, body: &str) -> String {
        fs::write(repo.join(name), body).expect("write fixture");
        run_git(repo, &["add", name]);
        run_git(
            repo,
            &[
                "-c",
                "user.name=prview test",
                "-c",
                "user.email=prview@example.test",
                "commit",
                "-m",
                name,
            ],
        );
        let output = git_cmd()
            .args(["rev-parse", "HEAD"])
            .current_dir(repo)
            .output()
            .expect("rev-parse");
        assert!(output.status.success());
        String::from_utf8(output.stdout).unwrap().trim().to_string()
    }

    fn init_repo_with_diverged_local_base() -> (tempfile::TempDir, String, String) {
        let tmp = tempfile::tempdir().expect("tempdir");
        run_git(tmp.path(), &["init", "-q", "-b", "main"]);
        let _initial_oid = write_commit(tmp.path(), "file.txt", "initial\n");
        run_git(tmp.path(), &["checkout", "-q", "-b", "base"]);
        let local_base_oid = write_commit(tmp.path(), "base.txt", "local base\n");
        run_git(tmp.path(), &["checkout", "-q", "-b", "feature"]);
        let head_oid = write_commit(tmp.path(), "feature.txt", "feature\n");
        run_git(tmp.path(), &["checkout", "-q", "base"]);
        let github_base_oid = write_commit(tmp.path(), "base.txt", "github base\n");
        assert_ne!(local_base_oid, github_base_oid);
        (tmp, head_oid, github_base_oid)
    }

    fn commit(id_suffix: usize) -> CommitInfo {
        let full_id = format!("{id_suffix:040x}");
        CommitInfo {
            short_id: short_sha(&full_id).to_string(),
            id: full_id,
            author: format!("Author {id_suffix}"),
            email: format!("author{id_suffix}@example.com"),
            date: format!("2024-01-{day:02}T00:00:00", day = (id_suffix % 28) + 1),
            message: format!("commit {id_suffix}"),
        }
    }

    #[test]
    fn test_resolved_ref_creation() {
        let resolved = ResolvedRef {
            name: "feature/test".to_string(),
            commit_id: "abc123def456".to_string(),
            is_remote: false,
        };
        assert_eq!(resolved.name, "feature/test");
        assert_eq!(resolved.commit_id, "abc123def456");
        assert!(!resolved.is_remote);
    }

    #[test]
    fn test_resolved_ref_remote() {
        let resolved = ResolvedRef {
            name: "main".to_string(),
            commit_id: "deadbeef".to_string(),
            is_remote: true,
        };
        assert_eq!(resolved.name, "main");
        assert!(resolved.is_remote);
    }

    #[test]
    fn pr_mode_resolves_target_and_base_by_github_oids() {
        let (tmp, head_oid, github_base_oid) = init_repo_with_diverged_local_base();
        let repo = Repository::open(tmp.path()).expect("repo");
        let mut config = test_config_builder()
            .repo_root(tmp.path())
            .target(Some("feature"))
            .bases(&["base"])
            .profile(test_generic_profile())
            .build();
        config.pr_number = Some(6);
        config.pr_head_oid = Some(head_oid.clone());
        config.pr_base_oid = Some(github_base_oid.clone());

        let target = repo.resolve_target(&config).expect("target");
        let bases = repo.resolve_bases(&config).expect("bases");

        assert_eq!(target.name, "feature");
        assert_eq!(target.commit_id, head_oid);
        assert!(target.is_remote);
        assert_eq!(bases.len(), 1);
        assert_eq!(bases[0].name, "base");
        assert_eq!(bases[0].commit_id, github_base_oid);
        assert!(bases[0].is_remote);
    }

    #[test]
    fn test_file_status_equality() {
        assert_eq!(FileStatus::Added, FileStatus::Added);
        assert_ne!(FileStatus::Added, FileStatus::Modified);
        assert_ne!(FileStatus::Deleted, FileStatus::Renamed);
    }

    #[test]
    fn test_file_status_variants() {
        let statuses = [
            FileStatus::Added,
            FileStatus::Modified,
            FileStatus::Deleted,
            FileStatus::Renamed,
            FileStatus::Copied,
        ];
        assert_eq!(statuses.len(), 5);
    }

    #[test]
    fn test_file_change_creation() {
        let change = FileChange {
            path: "src/main.rs".to_string(),
            status: FileStatus::Modified,
            additions: 10,
            deletions: 5,
        };
        assert_eq!(change.path, "src/main.rs");
        assert_eq!(change.status, FileStatus::Modified);
        assert_eq!(change.additions, 10);
        assert_eq!(change.deletions, 5);
    }

    #[test]
    fn test_file_change_added() {
        let change = FileChange {
            path: "new_file.rs".to_string(),
            status: FileStatus::Added,
            additions: 100,
            deletions: 0,
        };
        assert_eq!(change.status, FileStatus::Added);
        assert_eq!(change.deletions, 0);
    }

    #[test]
    fn test_file_change_deleted() {
        let change = FileChange {
            path: "old_file.rs".to_string(),
            status: FileStatus::Deleted,
            additions: 0,
            deletions: 50,
        };
        assert_eq!(change.status, FileStatus::Deleted);
        assert_eq!(change.additions, 0);
    }

    #[test]
    fn test_diff_stats_default() {
        let stats = DiffStats::default();
        assert_eq!(stats.files_changed, 0);
        assert_eq!(stats.additions, 0);
        assert_eq!(stats.deletions, 0);
    }

    #[test]
    fn test_diff_stats_creation() {
        let stats = DiffStats {
            files_changed: 5,
            additions: 100,
            deletions: 50,
            copied: 0,
        };
        assert_eq!(stats.files_changed, 5);
        assert_eq!(stats.additions, 100);
        assert_eq!(stats.deletions, 50);
        assert_eq!(stats.copied, 0);
    }

    #[test]
    fn test_commit_info_creation() {
        let commit = CommitInfo {
            id: "abc123def456789".to_string(),
            short_id: "abc123d".to_string(),
            author: "Test Author".to_string(),
            email: "test@example.com".to_string(),
            date: "2024-01-15T10:30:00".to_string(),
            message: "Test commit message".to_string(),
        };
        assert_eq!(commit.id, "abc123def456789");
        assert_eq!(commit.short_id, "abc123d");
        assert_eq!(commit.author, "Test Author");
        assert_eq!(commit.email, "test@example.com");
        assert_eq!(commit.message, "Test commit message");
    }

    #[test]
    fn test_commit_info_empty_email() {
        let commit = CommitInfo {
            id: "abc123".to_string(),
            short_id: "abc".to_string(),
            author: "Unknown".to_string(),
            email: String::new(),
            date: "2024-01-01T00:00:00".to_string(),
            message: "Initial commit".to_string(),
        };
        assert!(commit.email.is_empty());
    }

    #[test]
    fn test_diff_creation() {
        let diff = Diff {
            base: "main".to_string(),
            target: "feature/test".to_string(),
            base_commit_id: "abc123".to_string(),
            target_commit_id: "def456".to_string(),
            files: vec![FileChange {
                path: "src/lib.rs".to_string(),
                status: FileStatus::Modified,
                additions: 10,
                deletions: 5,
            }],
            stats: DiffStats {
                files_changed: 1,
                additions: 10,
                deletions: 5,
                copied: 0,
            },
            commits: vec![],
        };
        assert_eq!(diff.base, "main");
        assert_eq!(diff.target, "feature/test");
        assert_eq!(diff.files.len(), 1);
        assert_eq!(diff.stats.files_changed, 1);
    }

    #[test]
    fn test_diff_with_multiple_files() {
        let diff = Diff {
            base: "develop".to_string(),
            target: "feature/multi".to_string(),
            base_commit_id: "aaa111".to_string(),
            target_commit_id: "bbb222".to_string(),
            files: vec![
                FileChange {
                    path: "file1.rs".to_string(),
                    status: FileStatus::Added,
                    additions: 50,
                    deletions: 0,
                },
                FileChange {
                    path: "file2.rs".to_string(),
                    status: FileStatus::Modified,
                    additions: 30,
                    deletions: 10,
                },
                FileChange {
                    path: "file3.rs".to_string(),
                    status: FileStatus::Deleted,
                    additions: 0,
                    deletions: 100,
                },
            ],
            stats: DiffStats {
                files_changed: 3,
                additions: 80,
                deletions: 110,
                copied: 0,
            },
            commits: vec![],
        };
        assert_eq!(diff.files.len(), 3);
        assert_eq!(diff.stats.files_changed, 3);
    }

    #[test]
    fn test_diff_with_commits() {
        let diff = Diff {
            base: "main".to_string(),
            target: "feature/commits".to_string(),
            base_commit_id: "base123".to_string(),
            target_commit_id: "target456".to_string(),
            files: vec![],
            stats: DiffStats::default(),
            commits: vec![
                CommitInfo {
                    id: "commit1".to_string(),
                    short_id: "c1".to_string(),
                    author: "Author1".to_string(),
                    email: "a1@test.com".to_string(),
                    date: "2024-01-01T00:00:00".to_string(),
                    message: "First commit".to_string(),
                },
                CommitInfo {
                    id: "commit2".to_string(),
                    short_id: "c2".to_string(),
                    author: "Author2".to_string(),
                    email: "a2@test.com".to_string(),
                    date: "2024-01-02T00:00:00".to_string(),
                    message: "Second commit".to_string(),
                },
            ],
        };
        assert_eq!(diff.commits.len(), 2);
        assert_eq!(diff.commits[0].message, "First commit");
        assert_eq!(diff.commits[1].message, "Second commit");
    }

    #[test]
    fn test_file_status_serialization() {
        let status = FileStatus::Added;
        let serialized = serde_json::to_string(&status).unwrap();
        assert_eq!(serialized, "\"added\"");
    }

    #[test]
    fn test_file_status_deserialization() {
        let deserialized: FileStatus = serde_json::from_str("\"modified\"").unwrap();
        assert_eq!(deserialized, FileStatus::Modified);
    }

    #[test]
    fn test_diff_stats_serialization() {
        let stats = DiffStats {
            files_changed: 10,
            additions: 200,
            deletions: 50,
            copied: 0,
        };
        let json = serde_json::to_string(&stats).unwrap();
        assert!(json.contains("\"files_changed\":10"));
        assert!(json.contains("\"additions\":200"));
        assert!(json.contains("\"deletions\":50"));
    }

    #[test]
    fn test_diff_clone() {
        let original = Diff {
            base: "main".to_string(),
            target: "feature".to_string(),
            base_commit_id: "clone1".to_string(),
            target_commit_id: "clone2".to_string(),
            files: vec![],
            stats: DiffStats::default(),
            commits: vec![],
        };
        let cloned = original.clone();
        assert_eq!(original.base, cloned.base);
        assert_eq!(original.target, cloned.target);
    }

    #[test]
    fn test_truncate_commit_list_preserves_last_commit_and_reports_omitted_count() {
        let mut commits: Vec<CommitInfo> = (1..=MAX_COMMITS + 3).map(commit).collect();
        let expected_last = commits.last().cloned().expect("last commit");

        truncate_commit_list(&mut commits);

        assert_eq!(commits.len(), MAX_COMMITS);
        assert_eq!(commits[MAX_COMMITS - 2].id, TRUNCATED_COMMIT_SENTINEL_ID);
        assert_eq!(
            commits[MAX_COMMITS - 2].short_id,
            TRUNCATED_COMMIT_SENTINEL_SHORT_ID
        );
        assert!(
            commits[MAX_COMMITS - 2]
                .message
                .contains("omitted 4 commits beyond the 500 entry limit"),
            "unexpected sentinel message: {}",
            commits[MAX_COMMITS - 2].message
        );
        assert_eq!(
            commits.last().map(|commit| &commit.id),
            Some(&expected_last.id)
        );
        assert_eq!(
            commits.last().map(|commit| &commit.message),
            Some(&expected_last.message)
        );
    }

    #[test]
    fn test_file_change_clone() {
        let original = FileChange {
            path: "test.rs".to_string(),
            status: FileStatus::Added,
            additions: 10,
            deletions: 0,
        };
        let cloned = original.clone();
        assert_eq!(original.path, cloned.path);
        assert_eq!(original.status, cloned.status);
    }

    #[test]
    fn test_commit_info_clone() {
        let original = CommitInfo {
            id: "abc123".to_string(),
            short_id: "abc".to_string(),
            author: "Author".to_string(),
            email: "email@test.com".to_string(),
            date: "2024-01-01".to_string(),
            message: "Test".to_string(),
        };
        let cloned = original.clone();
        assert_eq!(original.id, cloned.id);
        assert_eq!(original.author, cloned.author);
    }
}
