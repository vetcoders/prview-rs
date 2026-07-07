//! Semantic cross-file rules — domain-aware finding generation.
//!
//! First rule: detect delete flows (DB record removal) without corresponding
//! resource cleanup (file/storage/S3 artifact deletion).

use crate::git::{Diff, FileStatus};
use serde::Serialize;

/// A semantic finding backed by multi-file evidence.
#[derive(Debug, Clone, Serialize)]
pub struct SemanticFinding {
    pub rule_id: &'static str,
    pub severity: &'static str,
    pub confidence: &'static str,
    pub title: String,
    pub description: String,
    pub evidence: Vec<EvidenceItem>,
}

/// A single piece of evidence in a semantic finding.
#[derive(Debug, Clone, Serialize)]
pub struct EvidenceItem {
    pub file: String,
    pub line: Option<usize>,
    pub role: &'static str,
    pub snippet: String,
}

/// Indicators that a file contains resource path references (DB/model side).
// Unreferenced: the orphaned-resource rule (delete flow touching a model that
// stores file paths, with no cleanup call) was designed but never implemented —
// only the delete-flow half shipped (DELETE_INDICATORS below is live). This
// curated list is the model-side half of that rule. Remove together with
// CLEANUP_INDICATORS if the rule is abandoned. Re-confirmed still-dead after
// the wave-7 verdict wiring landed. Tracked in the vc-prune forgotten-gems
// report 2026-07-02.
#[allow(dead_code)]
const PATH_COLUMN_INDICATORS: &[&str] = &[
    "file_path",
    "file_url",
    "storage_path",
    "attachment_path",
    "asset_path",
    "blob_path",
    "media_url",
    "image_url",
    "avatar_url",
    "thumbnail_path",
    "upload_path",
    "artifact_path",
    "s3_key",
    "object_key",
];

/// Indicators of record deletion.
const DELETE_INDICATORS: &[&str] = &[
    "delete",
    "remove",
    "destroy",
    "drop",
    "purge",
    "truncate",
    "soft_delete",
    "hard_delete",
];

/// Indicators of resource cleanup.
// Unreferenced: cleanup-side half of the unimplemented orphaned-resource rule —
// see the note on PATH_COLUMN_INDICATORS above; the two lists live or die
// together. Re-confirmed still-dead after the wave-7 verdict wiring landed.
// Tracked in the vc-prune forgotten-gems report 2026-07-02.
#[allow(dead_code)]
const CLEANUP_INDICATORS: &[&str] = &[
    "unlink",
    "remove_file",
    "delete_file",
    "delete_object",
    "delete_blob",
    "remove_blob",
    "fs::remove",
    "std::fs::remove",
    "os.remove",
    "os.unlink",
    "shutil.rmtree",
    "s3.delete",
    "s3_client.delete",
    "storage.delete",
    "blob.delete",
    "cleanup",
    "clean_up",
];

/// Scan diff hunks for the orphaned-resource-after-delete pattern.
///
/// Detection logic:
/// 1. Find files with delete operations touching models/tables with path-like columns.
/// 2. Check if the same diff contains cleanup for the physical resources.
/// 3. If delete exists but cleanup is absent or partial, emit a hypothesis finding.
pub fn detect_orphaned_resource_delete(diffs: &[Diff]) -> Vec<SemanticFinding> {
    let mut findings = Vec::new();

    let mut delete_evidence: Vec<(String, usize, String)> = Vec::new(); // (file, line, snippet)
    let mut has_path_columns: Vec<(String, usize, String)> = Vec::new();
    let mut cleanup_evidence: Vec<(String, usize, String)> = Vec::new();

    for diff in diffs {
        for file in &diff.files {
            if file.status == FileStatus::Deleted {
                continue;
            }

            // We'd need the actual diff content to analyze hunks.
            // For now, use filename-based heuristics for detection.
            let lower = file.path.to_lowercase();

            // Check if this file likely contains model/schema definitions with path columns
            let is_model_file = lower.contains("model")
                || lower.contains("schema")
                || lower.contains("migration")
                || lower.contains("entity")
                || lower.contains("table");

            let is_service_file = lower.contains("service")
                || lower.contains("handler")
                || lower.contains("controller")
                || lower.contains("repository")
                || lower.contains("repo")
                || lower.contains("manager");

            let is_cleanup_file = lower.contains("cleanup")
                || lower.contains("storage")
                || lower.contains("upload")
                || lower.contains("s3")
                || lower.contains("blob")
                || lower.contains("gc");

            // Heuristic: if a service/handler file is modified and contains delete-like patterns
            // in its name, it's a candidate for orphaned resource detection
            if is_service_file {
                for indicator in DELETE_INDICATORS {
                    if contains_identifier_token(&lower, indicator) {
                        delete_evidence.push((
                            file.path.clone(),
                            0,
                            format!("File name suggests delete operation: {}", indicator),
                        ));
                    }
                }
            }

            if is_model_file {
                has_path_columns.push((
                    file.path.clone(),
                    0,
                    "Model/schema file may contain resource path references".to_string(),
                ));
            }

            if is_cleanup_file {
                cleanup_evidence.push((
                    file.path.clone(),
                    0,
                    "File suggests resource cleanup capability".to_string(),
                ));
            }
        }
    }

    // Rule: If we have delete evidence AND path-column models BUT no cleanup evidence,
    // emit a hypothesis finding.
    if !delete_evidence.is_empty() && !has_path_columns.is_empty() && cleanup_evidence.is_empty() {
        let mut evidence = Vec::new();
        for (file, line, snippet) in &delete_evidence {
            evidence.push(EvidenceItem {
                file: file.clone(),
                line: if *line > 0 { Some(*line) } else { None },
                role: "delete_operation",
                snippet: snippet.clone(),
            });
        }
        for (file, line, snippet) in &has_path_columns {
            evidence.push(EvidenceItem {
                file: file.clone(),
                line: if *line > 0 { Some(*line) } else { None },
                role: "resource_reference",
                snippet: snippet.clone(),
            });
        }

        findings.push(SemanticFinding {
            rule_id: "orphaned-resource-after-delete",
            severity: "warning",
            confidence: "hypothesis",
            title: "Delete flow may leave orphaned resources".to_string(),
            description: format!(
                "This PR modifies {} file(s) with delete operations and {} file(s) with \
                 resource path references, but no cleanup of physical resources (files, S3 \
                 objects, blobs) was detected. If these models store file paths, the \
                 corresponding resources may become orphaned after deletion.",
                delete_evidence.len(),
                has_path_columns.len()
            ),
            evidence,
        });
    }

    findings
}

fn contains_identifier_token(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return false;
    }

    haystack.match_indices(needle).any(|(start, _)| {
        let before = haystack[..start].chars().next_back();
        let after = haystack[start + needle.len()..].chars().next();
        is_identifier_token_boundary(before) && is_identifier_token_boundary(after)
    })
}

fn is_identifier_token_boundary(ch: Option<char>) -> bool {
    ch.is_none_or(|ch| !ch.is_ascii_alphanumeric())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::artifacts::signal::test_helpers::{mock_diff, mock_file_change};
    use crate::git::FileStatus;

    #[test]
    fn no_findings_for_clean_pr() {
        let diff = mock_diff(vec![
            mock_file_change("src/lib.rs", FileStatus::Modified, 10, 5),
            mock_file_change("src/utils.rs", FileStatus::Modified, 5, 2),
        ]);
        let findings = detect_orphaned_resource_delete(&[diff]);
        assert!(findings.is_empty());
    }

    #[test]
    fn detects_delete_without_cleanup() {
        let diff = mock_diff(vec![
            mock_file_change(
                "src/services/delete_user_service.rs",
                FileStatus::Modified,
                20,
                5,
            ),
            mock_file_change("src/models/user_model.rs", FileStatus::Modified, 10, 2),
        ]);
        let findings = detect_orphaned_resource_delete(&[diff]);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].rule_id, "orphaned-resource-after-delete");
        assert_eq!(findings[0].confidence, "hypothesis");
        assert!(!findings[0].evidence.is_empty());
    }

    #[test]
    fn artifact_consistency_dropdown_filename_does_not_match_drop_indicator() {
        let diff = mock_diff(vec![
            mock_file_change(
                "src/services/dropdown_service.rs",
                FileStatus::Modified,
                20,
                5,
            ),
            mock_file_change("src/models/menu_model.rs", FileStatus::Modified, 10, 2),
        ]);

        let findings = detect_orphaned_resource_delete(&[diff]);

        assert!(findings.is_empty(), "dropdown must not trigger drop");
    }

    #[test]
    fn no_finding_when_cleanup_present() {
        let diff = mock_diff(vec![
            mock_file_change(
                "src/services/delete_user_service.rs",
                FileStatus::Modified,
                20,
                5,
            ),
            mock_file_change("src/models/user_model.rs", FileStatus::Modified, 10, 2),
            mock_file_change("src/storage/cleanup.rs", FileStatus::Modified, 15, 3),
        ]);
        let findings = detect_orphaned_resource_delete(&[diff]);
        assert!(findings.is_empty(), "Cleanup file should suppress finding");
    }
}
