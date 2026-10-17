//! Describe comparison coverage independently of checks on the target tree.

use super::*;
use crate::policy::engine::{AnalysisStatus, MergeRecommendation};

#[derive(Debug, Clone, Default, serde::Serialize)]
pub(super) struct ReviewRange {
    pub current_only: bool,
    pub explicit_bases: bool,
    pub comparisons: Vec<ReviewComparison>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub(super) struct ReviewComparison {
    pub base: String,
    pub requested_base_sha: String,
    pub effective_base_sha: String,
    pub target_sha: String,
    pub status: &'static str,
}

impl ReviewRange {
    pub fn assess(
        config: &Config,
        explicit_bases: bool,
        target: &ResolvedRef,
        bases: &[ResolvedRef],
        diffs: &[Diff],
    ) -> Self {
        let comparisons = bases
            .iter()
            .map(|base| {
                let diff = diffs.iter().find(|diff| diff.base == base.name);
                // generate_diffs omits ONLY effective-base == target. Distinct
                // commits with equal trees still produce a Diff with zero files.
                let effective_base_sha = diff
                    .map(|d| d.base_commit_id.as_str())
                    .unwrap_or(&target.commit_id)
                    .to_owned();
                let status = if base.commit_id == target.commit_id {
                    "self_diff"
                } else if effective_base_sha == target.commit_id {
                    "merge_base_equals_target"
                } else if diff.is_some_and(|d| d.files.is_empty()) {
                    "equal_trees"
                } else {
                    "compared"
                };
                ReviewComparison {
                    base: base.name.clone(),
                    requested_base_sha: base.commit_id.clone(),
                    effective_base_sha,
                    target_sha: target.commit_id.clone(),
                    status,
                }
            })
            .collect();
        Self {
            current_only: config.current_only,
            explicit_bases: config.bases_were_explicitly_selected(explicit_bases),
            comparisons,
        }
    }

    pub fn requires_review(&self) -> bool {
        !self.current_only
            && self.explicit_bases
            && self.comparisons.iter().any(|c| c.status == "self_diff")
    }

    pub fn apply_review(
        &self,
        analysis: &mut AnalysisStatus,
        merge: &mut MergeRecommendation,
    ) -> Vec<String> {
        if self.requires_review() {
            *analysis = AnalysisStatus::Incomplete;
            if *merge == MergeRecommendation::Approve {
                *merge = MergeRecommendation::ReviewRequired;
            }
        }
        self.review_caveats()
    }

    pub fn review_caveats(&self) -> Vec<String> {
        self.comparisons.iter().filter(|c| matches!(c.status, "self_diff" | "merge_base_equals_target"))
            .map(|c| format!(
                "No change reviewed against '{}' ({}): {}; target-tree checks do not establish review of a change. Select a different base to review a change.",
                c.base, c.effective_base_sha, c.status,
            )).collect()
    }
}
