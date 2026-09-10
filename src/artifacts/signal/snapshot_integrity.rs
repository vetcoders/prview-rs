//! Retained check-boundary observations of the shared review snapshot, never a check result.

use crate::checks::snapshot_integrity::{SnapshotIntegrityStatus, SnapshotObservation};
use anyhow::Result;
use serde::Serialize;
use std::collections::BTreeSet;
use std::path::Path;

use crate::policy::engine::{AnalysisStatus, MergeRecommendation};

#[derive(Debug, Clone, Serialize)]
pub(crate) struct SnapshotIntegrity {
    schema_version: &'static str,
    observation: &'static str,
    expected_target_sha: String,
    observed_head_sha: Option<String>,
    status: SnapshotIntegrityStatus,
    changed_paths: Option<Vec<String>>,
    error: Option<String>,
    observations: Vec<SnapshotObservation>,
}

impl SnapshotIntegrity {
    #[cfg(test)]
    pub(crate) fn observe(snapshot: &Path, repo_root: &Path, expected_target_sha: &str) -> Self {
        Self::from_observations(
            SnapshotObservation::observe(snapshot, repo_root, expected_target_sha),
            Vec::new(),
        )
    }

    /// Retain every non-clean boundary even when later checks restore the tree.
    pub(crate) fn from_observations(
        final_observation: SnapshotObservation,
        mut observations: Vec<SnapshotObservation>,
    ) -> Self {
        let mut evidence = Self {
            schema_version: "1.0",
            observation: "shared-snapshot-check-boundaries",
            expected_target_sha: final_observation.expected_target_sha.clone(),
            observed_head_sha: final_observation.observed_head_sha.clone(),
            status: SnapshotIntegrityStatus::Clean,
            changed_paths: Some(Vec::new()),
            error: None,
            observations: Vec::new(),
        };
        observations.push(final_observation);
        let mut paths = BTreeSet::new();
        let mut errors = BTreeSet::new();
        for observation in &observations {
            match observation.status {
                SnapshotIntegrityStatus::Unknown => {
                    evidence.status = SnapshotIntegrityStatus::Unknown
                }
                SnapshotIntegrityStatus::Modified
                    if evidence.status != SnapshotIntegrityStatus::Unknown =>
                {
                    evidence.status = SnapshotIntegrityStatus::Modified;
                }
                _ => {}
            }
            paths.extend(observation.changed_paths.iter().flatten().cloned());
            if let Some(error) = &observation.error {
                errors.insert(error.clone());
            }
        }
        evidence.changed_paths = if evidence.status == SnapshotIntegrityStatus::Unknown {
            None
        } else {
            Some(paths.into_iter().collect())
        };
        if !errors.is_empty() {
            evidence.error = Some(errors.into_iter().collect::<Vec<_>>().join("; "));
        }
        evidence.observations = observations;
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
        } else if let Some(changed_paths) = &self.changed_paths {
            let paths = changed_paths
                .iter()
                .take(8)
                .map(|path| format!("{path:?}"))
                .collect::<Vec<_>>()
                .join(", ");
            let extra = changed_paths.len().saturating_sub(8);
            let suffix = if extra > 0 {
                format!(" (+{extra} more)")
            } else {
                String::new()
            };
            let head = if self.observations.iter().any(|observation| {
                observation
                    .observed_head_sha
                    .as_deref()
                    .is_some_and(|head| head != observation.expected_target_sha)
            }) {
                "; snapshot HEAD changed"
            } else {
                ""
            };
            format!(
                "{} tracked path(s) observed changed: {paths}{suffix}{head}",
                changed_paths.len()
            )
        } else {
            "tracked-path comparison is unavailable".to_owned()
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
            "# Shared snapshot integrity\n\n{}\n\nExpected target: `{}`\n\nObserved HEAD: `{}`\n\n## Tracked paths\n\n```json\n{}\n```\n\nObservations are taken before and after each live check and once after checks, before context generation. Earlier non-clean observations remain even when later checks restore the tree. Check names identify observation boundaries, not the writer: concurrent tools may share the snapshot. Newly untracked files, including a generated Cargo.lock, are excluded; changes to an already tracked lockfile are included. Index and worktree differences are both retained. Check statuses and exit codes are unchanged. Endpoint observations cannot detect a change restored between observations. Results overlapping an observed change are not written to cache. The JSON retains each non-clean boundary and the final observation.\n",
            self.review_caveat(),
            self.expected_target_sha,
            self.observed_head_sha.as_deref().unwrap_or("unknown"),
            serde_json::to_string_pretty(&self.changed_paths)?,
        );
        std::fs::write(quality_dir.join("SNAPSHOT_INTEGRITY.md"), md)?;
        Ok(())
    }
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
    fn snapshot_integrity_explains_a_restored_head_without_path_changes() {
        let (owner, snapshot, target) = fixture();
        let repo = git2::Repository::open(&snapshot.worktree_path).unwrap();
        let parent = repo.head().unwrap().peel_to_commit().unwrap();
        let signature = git2::Signature::now("Fixture", "fixture@example.test").unwrap();
        repo.commit(
            Some("HEAD"),
            &signature,
            &signature,
            "empty commit",
            &parent.tree().unwrap(),
            &[&parent],
        )
        .unwrap();
        let changed = SnapshotObservation::observe(&snapshot.worktree_path, owner.path(), &target);
        assert_eq!(changed.status, SnapshotIntegrityStatus::Modified);
        assert_eq!(changed.changed_paths, Some(Vec::new()));
        repo.set_head_detached(git2::Oid::from_str(&target).unwrap())
            .unwrap();
        let restored = SnapshotObservation::observe(&snapshot.worktree_path, owner.path(), &target);
        assert!(!restored.requires_review());
        let evidence = SnapshotIntegrity::from_observations(restored, vec![changed]);
        assert!(evidence.requires_review());
        assert!(evidence.review_caveat().contains("snapshot HEAD changed"));
    }

    #[test]
    fn snapshot_integrity_report_keeps_a_restored_boundary_and_unknown_evidence() {
        let (owner, snapshot, target) = fixture();
        let cwd = &snapshot.worktree_path;
        std::fs::write(cwd.join("source.rs"), "changed\n").unwrap();
        let changed = SnapshotObservation::observe(cwd, owner.path(), &target);
        std::fs::write(cwd.join("source.rs"), "fn original() {}\n").unwrap();
        let clean = SnapshotObservation::observe(cwd, owner.path(), &target);
        assert!(!clean.requires_review());
        let report = SnapshotIntegrity::from_observations(clean.clone(), vec![changed.clone()]);
        assert_eq!(report.status, SnapshotIntegrityStatus::Modified);
        assert_eq!(report.changed_paths, Some(vec!["source.rs".to_owned()]));
        assert!(report.requires_review());
        let unknown = SnapshotObservation::observe(cwd, owner.path(), "missing");
        let report = SnapshotIntegrity::from_observations(clean, vec![changed, unknown]);
        assert_eq!(report.status, SnapshotIntegrityStatus::Unknown);
        assert!(report.changed_paths.is_none());
        assert_eq!(
            report.observations[0].changed_paths,
            Some(vec!["source.rs".to_owned()])
        );
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
        assert_eq!(evidence.changed_paths, Some(vec!["Cargo.lock".to_owned()]));
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
        assert_eq!(evidence.changed_paths, Some(vec!["source.rs".to_owned()]));
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
        assert_eq!(evidence.changed_paths, Some(vec!["source.rs".to_owned()]));
        assert_ne!(evidence.observed_head_sha.as_deref(), Some(target.as_str()));
        assert!(evidence.requires_review());
    }

    #[test]
    fn snapshot_integrity_unknown_never_certifies_a_clean_tree_or_changes_a_block() {
        let (owner, snapshot, _) = fixture();
        let evidence = SnapshotIntegrity::observe(&snapshot.worktree_path, owner.path(), "missing");
        assert_eq!(evidence.status, SnapshotIntegrityStatus::Unknown);
        assert!(evidence.changed_paths.is_none());
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
