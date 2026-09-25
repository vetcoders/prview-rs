//! Read-only observations of tracked snapshot state against its original commit.

use anyhow::{Context, Result};
use serde::Serialize;
use std::collections::BTreeSet;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum SnapshotIntegrityStatus {
    Clean,
    Modified,
    Unknown,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct SnapshotObservation {
    pub(crate) expected_target_sha: String,
    pub(crate) observed_head_sha: Option<String>,
    pub(crate) status: SnapshotIntegrityStatus,
    pub(crate) changed_paths: Option<Vec<String>>,
    pub(crate) error: Option<String>,
    pub(crate) phase: &'static str,
    pub(crate) check_name: Option<String>,
}

impl SnapshotObservation {
    /// The ledger owns this snapshot until publication. Observe against the
    /// original reviewed target, not the HEAD a test may have committed later.
    pub(crate) fn observe(snapshot: &Path, repo_root: &Path, expected_target_sha: &str) -> Self {
        let mut evidence = Self {
            expected_target_sha: expected_target_sha.to_owned(),
            observed_head_sha: None,
            status: SnapshotIntegrityStatus::Unknown,
            changed_paths: None,
            error: None,
            phase: "after-checks",
            check_name: None,
        };
        match observe_tracked_changes(snapshot, repo_root, expected_target_sha) {
            Ok((head, paths)) => {
                evidence.status = if paths.is_empty() && head == expected_target_sha {
                    SnapshotIntegrityStatus::Clean
                } else {
                    SnapshotIntegrityStatus::Modified
                };
                evidence.observed_head_sha = Some(head);
                evidence.changed_paths = Some(paths);
            }
            Err(error) => evidence.error = Some(format!("{error:#}")),
        }
        evidence
    }

    pub(crate) fn requires_review(&self) -> bool {
        self.status != SnapshotIntegrityStatus::Clean
    }
}

fn observe_tracked_changes(
    snapshot: &Path,
    repo_root: &Path,
    target: &str,
) -> Result<(String, Vec<String>)> {
    let repo = git2::Repository::open(snapshot).context("open shared snapshot")?;
    let owner = git2::Repository::discover(repo_root).context("open snapshot owner")?;
    anyhow::ensure!(
        std::fs::canonicalize(repo.commondir())? == std::fs::canonicalize(owner.commondir())?,
        "snapshot no longer belongs to the reviewed repository"
    );
    let head = repo.head()?.peel_to_commit()?.id();
    let tree = repo.find_commit(git2::Oid::from_str(target)?)?.tree()?;
    let index = repo.index()?;
    let mut options = git2::DiffOptions::new();
    options.include_untracked(false).include_typechange(true);
    // Keep both axes: combining them can hide a staged change that the working
    // file subsequently reverted. Comparing with target also catches commits.
    let staged = repo.diff_tree_to_index(Some(&tree), Some(&index), Some(&mut options))?;
    let working = repo.diff_index_to_workdir(Some(&index), Some(&mut options))?;
    let mut paths = BTreeSet::new();
    let mut record = |delta: git2::DiffDelta<'_>| {
        if delta.status() == git2::Delta::Untracked || delta.status() == git2::Delta::Ignored {
            return;
        }
        for path in [delta.old_file().path_bytes(), delta.new_file().path_bytes()]
            .into_iter()
            .flatten()
        {
            // Git paths are bytes. Debug-escape non-UTF8 names instead of
            // silently replacing bytes and merging two distinct paths.
            let display = match std::str::from_utf8(path) {
                Ok(path) => path.to_owned(),
                Err(_) => format!("git-path-bytes:{path:02x?}"),
            };
            paths.insert(display);
        }
    };
    staged
        .deltas()
        .chain(working.deltas())
        .for_each(&mut record);
    // A check can mark an entry skip-worktree or assume-unchanged, and the
    // index-to-workdir diff then reports it unmodified however its file was
    // edited, and an assume-unchanged one even once it is removed: libgit2
    // trusts both flags (`maybe_modified`, `diff_delta__from_one`). Those
    // entries are read once more on disk against the target itself, past the
    // index.
    let flagged: Vec<Vec<u8>> = index
        .iter()
        .filter(|entry| {
            entry.flags_extended & git2::IndexEntryExtendedFlag::SKIP_WORKTREE.bits() != 0
                || entry.flags & git2::IndexEntryFlag::VALID.bits() != 0
        })
        .map(|entry| entry.path)
        .collect();
    if !flagged.is_empty() {
        let mut on_disk_options = git2::DiffOptions::new();
        on_disk_options
            .include_untracked(false)
            .include_typechange(true)
            .disable_pathspec_match(true);
        for path in flagged {
            on_disk_options.pathspec(path);
        }
        let on_disk = repo.diff_tree_to_workdir(Some(&tree), Some(&mut on_disk_options))?;
        on_disk.deltas().for_each(&mut record);
    }
    anyhow::ensure!(
        repo.head()?.peel_to_commit()?.id() == head,
        "snapshot HEAD moved during observation"
    );
    Ok((head.to_string(), paths.into_iter().collect()))
}
