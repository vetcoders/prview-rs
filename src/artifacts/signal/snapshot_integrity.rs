//! End-of-check observation of the shared review snapshot, never a check result.

use anyhow::{Context, Result};
use serde::Serialize;
use std::collections::BTreeSet;
use std::path::Path;

use crate::policy::engine::{AnalysisStatus, MergeRecommendation};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum SnapshotIntegrityStatus {
    Clean,
    Modified,
    Unknown,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct SnapshotIntegrity {
    schema_version: &'static str,
    observation: &'static str,
    expected_target_sha: String,
    observed_head_sha: Option<String>,
    status: SnapshotIntegrityStatus,
    changed_paths: Vec<String>,
    error: Option<String>,
}

impl SnapshotIntegrity {
    /// The ledger owns this snapshot until publication. Observe against the
    /// original reviewed target, not the HEAD a test may have committed later.
    pub(crate) fn observe(snapshot: &Path, repo_root: &Path, expected_target_sha: &str) -> Self {
        let mut evidence = Self {
            schema_version: "1.0",
            observation: "shared-snapshot-after-checks",
            expected_target_sha: expected_target_sha.to_owned(),
            observed_head_sha: None,
            status: SnapshotIntegrityStatus::Unknown,
            changed_paths: Vec::new(),
            error: None,
        };
        match observe_tracked_changes(snapshot, repo_root, expected_target_sha) {
            Ok((head, paths)) => {
                evidence.status = if paths.is_empty() && head == expected_target_sha {
                    SnapshotIntegrityStatus::Clean
                } else {
                    SnapshotIntegrityStatus::Modified
                };
                evidence.observed_head_sha = Some(head);
                evidence.changed_paths = paths;
            }
            Err(error) => evidence.error = Some(format!("{error:#}")),
        }
        evidence
    }

    pub(crate) fn requires_review(&self) -> bool {
        self.status != SnapshotIntegrityStatus::Clean
    }

    pub(crate) fn apply_review(
        &self,
        confidence: &mut AnalysisStatus,
        merge: &mut MergeRecommendation,
    ) -> Vec<String> {
        if !self.requires_review() {
            return Vec::new();
        }
        if *confidence == AnalysisStatus::Complete {
            *confidence = AnalysisStatus::Degraded;
        }
        if *merge == MergeRecommendation::Approve {
            *merge = MergeRecommendation::ReviewRequired;
        }
        vec![self.review_caveat()]
    }

    fn review_caveat(&self) -> String {
        let reason = if let Some(error) = &self.error {
            format!("could not verify the reviewed tree: {error}")
        } else {
            let paths = self
                .changed_paths
                .iter()
                .take(8)
                .map(|path| format!("{path:?}"))
                .collect::<Vec<_>>()
                .join(", ");
            let extra = self.changed_paths.len().saturating_sub(8);
            let suffix = if extra > 0 {
                format!(" (+{extra} more)")
            } else {
                String::new()
            };
            let head = if self.observed_head_sha.as_deref() != Some(&self.expected_target_sha) {
                "; snapshot HEAD changed"
            } else {
                ""
            };
            format!(
                "{} tracked path(s) differ: {paths}{suffix}{head}",
                self.changed_paths.len()
            )
        };
        format!(
            "Snapshot integrity: {reason}. Review 20_quality/SNAPSHOT_INTEGRITY.md; original check results are preserved."
        )
    }

    /// Clean and local runs emit no extra artifact. A failed write is fatal:
    /// the gate must not point at evidence that was never published.
    pub(crate) fn write(&self, quality_dir: &Path) -> Result<()> {
        if !self.requires_review() {
            return Ok(());
        }
        std::fs::write(
            quality_dir.join("SNAPSHOT_INTEGRITY.json"),
            serde_json::to_vec_pretty(self)?,
        )?;
        let md = format!(
            "# Shared snapshot integrity\n\n{}\n\nExpected target: `{}`\n\nObserved HEAD: `{}`\n\n## Tracked paths\n\n```json\n{}\n```\n\nThis is a run-wide observation after checks, before context generation. It does not attribute changes to a particular check. Newly untracked files, including a generated Cargo.lock, are excluded; changes to an already tracked lockfile are included. Index and worktree differences are both retained. Check statuses and exit codes are unchanged. This endpoint observation does not detect a change followed by restoration.\n",
            self.review_caveat(),
            self.expected_target_sha,
            self.observed_head_sha.as_deref().unwrap_or("unknown"),
            serde_json::to_string_pretty(&self.changed_paths)?,
        );
        std::fs::write(quality_dir.join("SNAPSHOT_INTEGRITY.md"), md)?;
        Ok(())
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
    for delta in staged.deltas().chain(working.deltas()) {
        if delta.status() == git2::Delta::Untracked || delta.status() == git2::Delta::Ignored {
            continue;
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
    }
    anyhow::ensure!(
        repo.head()?.peel_to_commit()?.id() == head,
        "snapshot HEAD moved during observation"
    );
    Ok((head.to_string(), paths.into_iter().collect()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (tempfile::TempDir, crate::git::WorktreeSnapshot, String) {
        let tmp = tempfile::tempdir().unwrap();
        let repo = git2::Repository::init(tmp.path()).unwrap();
        std::fs::write(tmp.path().join("source.rs"), "fn original() {}\n").unwrap();
        std::fs::write(tmp.path().join("Cargo.lock"), "tracked lock\n").unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(Path::new("source.rs")).unwrap();
        index.add_path(Path::new("Cargo.lock")).unwrap();
        index.write().unwrap();
        let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
        let signature = git2::Signature::now("Fixture", "fixture@example.test").unwrap();
        let target = repo
            .commit(Some("HEAD"), &signature, &signature, "target", &tree, &[])
            .unwrap()
            .to_string();
        let snapshot = crate::git::create_worktree_snapshot(tmp.path(), &target).unwrap();
        (tmp, snapshot, target)
    }

    #[test]
    fn snapshot_integrity_ignores_new_untracked_files_but_keeps_tracked_lock_changes() {
        let (repo, snapshot, target) = fixture();
        let cwd = &snapshot.worktree_path;
        std::fs::create_dir(cwd.join("nested")).unwrap();
        std::fs::write(cwd.join("nested/Cargo.lock"), "new lock").unwrap();
        std::fs::write(cwd.join("generated.txt"), "new output").unwrap();
        assert!(!SnapshotIntegrity::observe(cwd, repo.path(), &target).requires_review());
        std::fs::write(cwd.join("Cargo.lock"), "changed tracked lock").unwrap();
        let evidence = SnapshotIntegrity::observe(cwd, repo.path(), &target);
        assert_eq!(evidence.status, SnapshotIntegrityStatus::Modified);
        assert_eq!(evidence.changed_paths, ["Cargo.lock"]);
    }

    #[test]
    fn snapshot_integrity_keeps_staged_changes_even_when_working_bytes_are_restored() {
        let (owner, snapshot, target) = fixture();
        let cwd = &snapshot.worktree_path;
        let repo = git2::Repository::open(cwd).unwrap();
        std::fs::write(cwd.join("source.rs"), "fn changed() {}\n").unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(Path::new("source.rs")).unwrap();
        index.write().unwrap();
        std::fs::write(cwd.join("source.rs"), "fn original() {}\n").unwrap();
        let evidence = SnapshotIntegrity::observe(cwd, owner.path(), &target);
        assert_eq!(evidence.changed_paths, ["source.rs"]);
        assert!(evidence.requires_review());
    }

    #[test]
    fn snapshot_integrity_compares_committed_changes_with_original_target() {
        let (owner, snapshot, target) = fixture();
        let cwd = &snapshot.worktree_path;
        let repo = git2::Repository::open(cwd).unwrap();
        std::fs::remove_file(cwd.join("source.rs")).unwrap();
        let mut index = repo.index().unwrap();
        index.remove_path(Path::new("source.rs")).unwrap();
        index.write().unwrap();
        let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
        let parent = repo.head().unwrap().peel_to_commit().unwrap();
        let signature = git2::Signature::now("Fixture", "fixture@example.test").unwrap();
        repo.commit(
            Some("HEAD"),
            &signature,
            &signature,
            "test rewrites snapshot",
            &tree,
            &[&parent],
        )
        .unwrap();
        assert!(repo.statuses(None).unwrap().is_empty());
        let evidence = SnapshotIntegrity::observe(cwd, owner.path(), &target);
        assert_eq!(evidence.changed_paths, ["source.rs"]);
        assert_ne!(evidence.observed_head_sha.as_deref(), Some(target.as_str()));
        assert!(evidence.requires_review());
    }

    #[test]
    fn snapshot_integrity_unknown_never_certifies_a_clean_tree_or_changes_a_block() {
        let (owner, snapshot, _) = fixture();
        let evidence = SnapshotIntegrity::observe(&snapshot.worktree_path, owner.path(), "missing");
        assert_eq!(evidence.status, SnapshotIntegrityStatus::Unknown);
        let mut confidence = AnalysisStatus::Complete;
        let mut merge = MergeRecommendation::Block;
        assert_eq!(evidence.apply_review(&mut confidence, &mut merge).len(), 1);
        assert_eq!(confidence, AnalysisStatus::Degraded);
        assert_eq!(merge, MergeRecommendation::Block);
        let other = tempfile::tempdir().unwrap();
        git2::Repository::init(other.path()).unwrap();
        assert_eq!(
            SnapshotIntegrity::observe(other.path(), owner.path(), "missing").status,
            SnapshotIntegrityStatus::Unknown
        );
    }
}
