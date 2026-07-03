//! Per-file diffs for hotspot analysis.

use super::common::HOTSPOT_THRESHOLD;
use crate::git::{Diff, Repository};
use anyhow::Result;
use std::cmp::Reverse;
use std::fmt::Write as FmtWrite;
use std::fs;
use std::path::Path;

/// Avoid exploding artifact size on very large PRs.
const MAX_PER_FILE_DIFF_PATCHES: usize = 250;

#[derive(Debug, Clone, PartialEq, Eq)]
struct PerFileDiffCandidate {
    base_commit_id: String,
    target_commit_id: String,
    path: String,
    additions: usize,
    deletions: usize,
}

/// Generate `per-file-diffs/` directory with individual patches for all changed files.
pub fn generate_per_file_diffs(dir: &Path, repo: &Repository, diffs: &[Diff]) -> Result<()> {
    let diffs_dir = dir.join("per-file-diffs");

    let candidates = select_per_file_diff_candidates(diffs);
    let total_candidates: usize = diffs.iter().map(|diff| diff.files.len()).sum();
    let truncated = candidates.len() < total_candidates;

    let mut generated: Vec<(String, usize, usize, String)> = Vec::new(); // (sanitized_name, adds, dels, source_path)

    for candidate in &candidates {
        let patch = match repo.file_diff(
            &candidate.base_commit_id,
            &candidate.target_commit_id,
            &candidate.path,
        ) {
            Ok(p) => p,
            Err(_) => continue,
        };

        if patch.is_empty() {
            continue;
        }

        if !diffs_dir.exists() {
            fs::create_dir_all(&diffs_dir)?;
        }

        let filename =
            per_file_diff_filename_from_parts(&candidate.base_commit_id, &candidate.path);
        fs::write(diffs_dir.join(&filename), &patch)?;

        generated.push((
            filename,
            candidate.additions,
            candidate.deletions,
            candidate.path.clone(),
        ));
    }

    if generated.is_empty() {
        return Ok(());
    }

    let mut index = String::new();
    if truncated {
        let _ = writeln!(
            index,
            "# Per-file diffs (top {} of {} changed files by churn)",
            generated.len(),
            total_candidates
        );
        let _ = writeln!(
            index,
            "# Truncated to control artifact size on very large diffs.\n"
        );
    } else {
        let _ = writeln!(index, "# Per-file diffs (all changed files)");
        let _ = writeln!(index, "# {} files extracted\n", generated.len());
    }

    for (name, adds, dels, source_path) in &generated {
        let total = adds + dels;
        let hotspot_tag = if total >= HOTSPOT_THRESHOLD {
            " [HOTSPOT]"
        } else {
            ""
        };
        let _ = writeln!(
            index,
            "{}    +{} -{}  ({} total){}    {}",
            name, adds, dels, total, hotspot_tag, source_path
        );
    }

    fs::write(diffs_dir.join("00-INDEX.txt"), index)?;

    Ok(())
}

fn per_file_diff_filename_from_parts(base_commit_id: &str, path: &str) -> String {
    let base = short_id(base_commit_id);
    let encoded_path = sanitize_path(path);
    format!("{base}__{encoded_path}.patch")
}

fn short_id(commit_id: &str) -> &str {
    commit_id.get(..8).unwrap_or(commit_id)
}

/// Encode file path for use as filename — injective (collision-free).
///
/// Preserves `[A-Za-z0-9._]`, encodes everything else as `~XX`.
/// Readability comes from `00-INDEX.txt` which maps encoded → source path.
pub fn sanitize_path(path: &str) -> String {
    let mut encoded = String::with_capacity(path.len() + 8);
    for byte in path.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'.' | b'_' => {
                encoded.push(byte as char);
            }
            _ => {
                let _ = write!(encoded, "~{byte:02X}");
            }
        }
    }
    encoded
}

/// Find the per-file diff patch filename for a source path in the available patches list.
///
/// The generator creates filenames as `{base_commit}__{sanitize_path(path)}.patch`.
/// Consumers don't know the commit prefix, so we match by suffix: `__{sanitize_path(path)}.patch`.
pub fn find_per_file_patch<'a>(source_path: &str, available: &'a [String]) -> Option<&'a str> {
    let suffix = format!("__{}.patch", sanitize_path(source_path));
    available
        .iter()
        .find(|p| p.ends_with(&suffix))
        .map(|s| s.as_str())
}

fn select_per_file_diff_candidates(diffs: &[Diff]) -> Vec<PerFileDiffCandidate> {
    let mut candidates: Vec<PerFileDiffCandidate> = diffs
        .iter()
        .flat_map(|diff| {
            diff.files.iter().map(|file| PerFileDiffCandidate {
                base_commit_id: diff.base_commit_id.clone(),
                target_commit_id: diff.target_commit_id.clone(),
                path: file.path.clone(),
                additions: file.additions,
                deletions: file.deletions,
            })
        })
        .collect();

    if candidates.len() <= MAX_PER_FILE_DIFF_PATCHES {
        return candidates;
    }

    candidates.sort_by_key(|candidate| Reverse(candidate.additions + candidate.deletions));
    candidates.truncate(MAX_PER_FILE_DIFF_PATCHES);
    candidates
}

#[cfg(test)]
mod tests {
    use super::super::test_helpers::{mock_diff, mock_file_change};
    use super::*;
    use crate::git::FileStatus;

    #[test]
    fn per_file_diffs_threshold_and_sanitize() {
        // Injective encoding: slash and dash both get ~XX
        assert_eq!(
            sanitize_path("src/pipeline/streaming.rs"),
            "src~2Fpipeline~2Fstreaming.rs"
        );
        assert_eq!(sanitize_path("lib.rs"), "lib.rs");
        assert_eq!(sanitize_path("src/foo_bar.rs"), "src~2Ffoo_bar.rs");
        // No collision: different paths always produce different filenames
        assert_ne!(sanitize_path("a_b.rs"), sanitize_path("a/b.rs"));
        assert_ne!(sanitize_path("a/-a"), sanitize_path("a-/a"));
        assert_ne!(sanitize_path("a--/b"), sanitize_path("a/--b"));
        // Spaces encoded
        assert_eq!(sanitize_path("a b.rs"), "a~20b.rs");
        // Dash encoded
        assert_eq!(sanitize_path("a-b.rs"), "a~2Db.rs");
        assert_eq!(HOTSPOT_THRESHOLD, 80);
    }

    #[test]
    fn sanitize_path_is_injective() {
        // Core injectiveness proof: `~` itself is encoded as `~7E`, so a literal
        // `~2F` in a path can never collide with an encoded `/` (which is `~2F`).
        assert_eq!(sanitize_path("a~2Fb"), "a~7E2Fb");
        assert_eq!(sanitize_path("a/b"), "a~2Fb");
        assert_ne!(sanitize_path("a~2Fb"), sanitize_path("a/b"));

        // Tilde at start and end
        assert_eq!(sanitize_path("~"), "~7E");
        assert_eq!(sanitize_path("~~"), "~7E~7E");

        // Unicode bytes encoded individually
        let encoded = sanitize_path("src/ä.rs");
        assert!(encoded.starts_with("src~2F"));
        assert!(encoded.ends_with(".rs"));
        assert!(!encoded.contains('ä'));

        // Empty string
        assert_eq!(sanitize_path(""), "");

        // All passthrough chars preserved
        assert_eq!(sanitize_path("AZaz09._"), "AZaz09._");

        // Exhaustive no-collision: pairs of paths that could trip a naive encoder
        let collision_pairs = [
            ("a/b", "a~2Fb"),
            ("a b", "a~20b"),
            ("a-b", "a~2Db"),
            ("a~b", "a~7Eb"),
            ("foo/bar/baz", "foo~2Fbar~2Fbaz"),
        ];
        for (a, b) in collision_pairs {
            assert_ne!(
                sanitize_path(a),
                sanitize_path(b),
                "Collision between {:?} and {:?}",
                a,
                b
            );
        }
    }

    #[test]
    fn per_file_diff_filename_includes_base_commit() {
        let diff = mock_diff(vec![mock_file_change(
            "src/lib.rs",
            FileStatus::Modified,
            1,
            1,
        )]);
        let filename = per_file_diff_filename_from_parts(&diff.base_commit_id, "src/lib.rs");
        assert!(filename.starts_with("abc123__"));
        assert!(filename.ends_with(".patch"));
    }

    #[test]
    fn find_per_file_patch_matches_by_suffix() {
        let patches = vec![
            "abc12345__src~2Flib.rs.patch".to_string(),
            "abc12345__src~2Fmain.rs.patch".to_string(),
        ];
        assert_eq!(
            find_per_file_patch("src/lib.rs", &patches),
            Some("abc12345__src~2Flib.rs.patch")
        );
        assert_eq!(
            find_per_file_patch("src/main.rs", &patches),
            Some("abc12345__src~2Fmain.rs.patch")
        );
        assert_eq!(find_per_file_patch("src/other.rs", &patches), None);
    }

    #[test]
    fn find_per_file_patch_no_false_positive_on_partial_match() {
        // Ensure "b.rs" doesn't match "lib.rs" (suffix must include `__`)
        let patches = vec!["abc12345__lib.rs.patch".to_string()];
        assert_eq!(find_per_file_patch("b.rs", &patches), None);
        assert_eq!(
            find_per_file_patch("lib.rs", &patches),
            Some("abc12345__lib.rs.patch")
        );
    }

    #[test]
    fn select_per_file_diff_candidates_truncates_large_sets_by_churn() {
        let mut files = Vec::new();
        for idx in 0..300 {
            files.push(mock_file_change(
                &format!("src/file_{idx}.rs"),
                FileStatus::Modified,
                300 - idx,
                idx % 3,
            ));
        }
        let diff = mock_diff(files);

        let selected = select_per_file_diff_candidates(&[diff]);

        assert_eq!(selected.len(), MAX_PER_FILE_DIFF_PATCHES);
        assert_eq!(selected[0].path, "src/file_0.rs");
        assert!(
            selected
                .iter()
                .all(|candidate| candidate.path.starts_with("src/file_"))
        );
        assert!(
            !selected
                .iter()
                .any(|candidate| candidate.path == "src/file_299.rs")
        );
    }
}
