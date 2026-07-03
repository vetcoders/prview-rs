//! Per-file risk scoring based on multiple signals.

use super::breaking::BreakingFinding;
use super::common::{HOTSPOT_THRESHOLD, contains_path_token_match};
use super::coverage::CoverageDelta;
use crate::git::{Diff, FileChange, FileStatus};
use crate::paths::normalize_path_display;
use crate::regression::tests::is_test_file;
use std::collections::{HashMap, HashSet};
use std::path::Path;

/// Per-file risk score with contributing factors (B4).
#[derive(Debug, Clone)]
pub struct FileRiskScore {
    pub path: String,
    pub score: u32,
    pub factors: Vec<&'static str>,
    // Never read on the runtime path: zone aggregation recomputes membership in
    // compute_risk_heatmap, so per-file zones are only asserted by unit tests.
    // Kept because the PolicyEngine verdict wiring (policy/engine.rs) is the
    // planned consumer for per-file zone attribution; remove if that wiring
    // ships without it. Re-confirmed still-dead after the wave-7 verdict wiring
    // landed. Tracked in the vc-prune forgotten-gems report 2026-07-02.
    #[allow(dead_code)]
    pub zones: Vec<&'static str>,
}

/// Domain risk zone touched by a PR.
#[derive(Debug, Clone)]
pub struct RiskZone {
    pub name: &'static str,
    pub files_touched: usize,
    pub total_churn: usize,
    pub max_file_risk: u32,
}

/// Risk heatmap for the entire PR — aggregates per-file scores into zones.
#[derive(Debug, Clone)]
pub struct RiskHeatmap {
    pub zones: Vec<RiskZone>,
    pub total_risk_score: u32,
    pub risk_level: &'static str,
}

const DOMAIN_ZONES: &[(&str, &[&str])] = &[
    (
        "auth/session",
        &[
            "auth",
            "login",
            "logout",
            "session",
            "oauth",
            "jwt",
            "token",
            "sso",
            "saml",
            "password",
            "credential",
        ],
    ),
    (
        "persistence/database",
        &[
            "database",
            "db",
            "migration",
            "schema",
            "model",
            "repository",
            "query",
            "sql",
            "prisma",
            "diesel",
            "sqlx",
            "orm",
        ],
    ),
    (
        "storage/cleanup",
        &[
            "storage", "upload", "s3", "blob", "cleanup", "purge", "gc", "retain",
        ],
    ),
    (
        "security/permissions",
        &[
            "security",
            "permission",
            "acl",
            "rbac",
            "policy",
            "encrypt",
            "decrypt",
            "crypto",
            "secret",
            "vault",
            "tauri",
        ],
    ),
    (
        "api/public",
        &[
            "api",
            "endpoint",
            "route",
            "handler",
            "controller",
            "graphql",
            "grpc",
            "openapi",
            "swagger",
        ],
    ),
    (
        "migrations/schema",
        &["migration", "migrate", "schema", "alter", "ddl"],
    ),
];

/// Compute risk scores for changed files based on multiple signals (B4).
/// Returns top 10 files sorted by score descending.
///
/// Test-only convenience wrapper: every runtime caller goes through
/// `compute_file_risk_scores_with_root` (context_artifacts.rs, merge_gate.rs)
/// with an explicit repo root; only the unit tests below and the signal
/// facade test exercise the rootless form.
#[cfg(test)]
pub fn compute_file_risk_scores(
    diffs: &[Diff],
    coverage: &CoverageDelta,
    breaking: &[BreakingFinding],
) -> Vec<FileRiskScore> {
    compute_file_risk_scores_with_root(diffs, coverage, breaking, None)
}

/// Compute risk scores with optional repo root for path normalization.
pub fn compute_file_risk_scores_with_root(
    diffs: &[Diff],
    coverage: &CoverageDelta,
    breaking: &[BreakingFinding],
    repo_root: Option<&Path>,
) -> Vec<FileRiskScore> {
    // Dedup by path: with multiple bases the same file appears once per diff,
    // which would otherwise double-count churn and emit duplicate rows (coverage
    // already dedups on exactly this scenario).
    let mut seen_paths = HashSet::new();
    let all_files: Vec<&FileChange> = diffs
        .iter()
        .flat_map(|d| &d.files)
        .filter(|f| seen_paths.insert(f.path.as_str()))
        .collect();
    let uncovered_paths: HashSet<&str> =
        coverage.uncovered.iter().map(|f| f.path.as_str()).collect();
    let covered_paths: HashSet<&str> = coverage
        .covered
        .iter()
        .map(|p| p.src_path.as_str())
        .collect();
    let breaking_paths: HashSet<&str> = breaking.iter().map(|b| b.file.as_str()).collect();

    let security_keywords = [
        "auth",
        "security",
        "crypto",
        "permission",
        "tauri",
        "secret",
        "token",
        "password",
        "credential",
        "session",
        "oauth",
    ];

    let mut scores: Vec<FileRiskScore> = Vec::new();

    for file in &all_files {
        if is_test_file(&file.path) {
            continue;
        }

        let mut score: u32 = 0;
        let mut factors: Vec<&'static str> = Vec::new();

        // +20 if deleted
        if file.status == FileStatus::Deleted {
            score += 20;
            factors.push("deleted");
        }

        // +15 if path contains security-related keywords
        let lower_path = file.path.to_lowercase();
        if security_keywords
            .iter()
            .any(|kw| contains_path_token_match(&lower_path, kw))
        {
            score += 15;
            factors.push("security-path");
        }

        // +10 if hotspot (churn >= threshold)
        let churn = file.additions + file.deletions;
        if churn >= HOTSPOT_THRESHOLD {
            score += 10;
            factors.push("hotspot");
        }

        // +10 if file has breaking changes
        if breaking_paths.contains(file.path.as_str()) {
            score += 10;
            factors.push("breaking");
        }

        // -10 if file has matching test change
        if covered_paths.contains(file.path.as_str()) {
            score = score.saturating_sub(10);
            factors.push("has-test");
        }

        // +5 if uncovered
        if uncovered_paths.contains(file.path.as_str()) {
            score += 5;
            factors.push("uncovered");
        }

        // Domain zone matching
        let mut zones: Vec<&'static str> = Vec::new();
        for &(zone_name, keywords) in DOMAIN_ZONES {
            if keywords
                .iter()
                .any(|kw| contains_path_token_match(&lower_path, kw))
                && !zones.contains(&zone_name)
            {
                zones.push(zone_name);
            }
        }

        if score > 0 {
            let normalized = match repo_root {
                Some(root) => normalize_path_display(&file.path, root),
                None => file.path.clone(),
            };
            scores.push(FileRiskScore {
                path: normalized,
                score,
                factors,
                zones,
            });
        }
    }

    scores.sort_by_key(|entry| std::cmp::Reverse(entry.score));
    scores.truncate(10);
    scores
}

/// Compute risk heatmap from per-file scores and diff data.
pub fn compute_risk_heatmap(diffs: &[Diff], file_scores: &[FileRiskScore]) -> RiskHeatmap {
    let mut seen_paths = HashSet::new();
    let all_files: Vec<&FileChange> = diffs
        .iter()
        .flat_map(|d| &d.files)
        .filter(|f| seen_paths.insert(f.path.as_str()))
        .collect();
    let mut zone_data: HashMap<&'static str, (usize, usize, u32)> = HashMap::new();

    for file in &all_files {
        if is_test_file(&file.path) {
            continue;
        }
        let lower = file.path.to_lowercase();
        let churn = file.additions + file.deletions;
        let file_risk = file_scores
            .iter()
            .find(|s| s.path == file.path)
            .map(|s| s.score)
            .unwrap_or(0);

        for &(zone_name, keywords) in DOMAIN_ZONES {
            if keywords
                .iter()
                .any(|kw| contains_path_token_match(&lower, kw))
            {
                let entry = zone_data.entry(zone_name).or_insert((0, 0, 0));
                entry.0 += 1;
                entry.1 += churn;
                if file_risk > entry.2 {
                    entry.2 = file_risk;
                }
            }
        }
    }

    let mut zones: Vec<RiskZone> = zone_data
        .into_iter()
        .map(|(name, (files, churn, max_risk))| RiskZone {
            name,
            files_touched: files,
            total_churn: churn,
            max_file_risk: max_risk,
        })
        .collect();
    zones.sort_by_key(|zone| std::cmp::Reverse(zone.max_file_risk));

    let total_risk_score: u32 = file_scores.iter().map(|s| s.score).sum();
    let risk_level = if total_risk_score >= 100 {
        "high"
    } else if total_risk_score >= 40 {
        "medium"
    } else {
        "low"
    };

    RiskHeatmap {
        zones,
        total_risk_score,
        risk_level,
    }
}

#[cfg(test)]
mod tests {
    use super::super::coverage::CoverageDelta;
    use super::super::test_helpers::{mock_diff, mock_file_change};
    use super::*;
    use crate::git::FileStatus;

    fn empty_coverage() -> CoverageDelta {
        CoverageDelta {
            pct: 100,
            total_source: 0,
            covered_count: 0,
            uncovered: vec![],
            covered: vec![],
            non_code_count: 0,
            ghost_tests: vec![],
        }
    }

    #[test]
    fn test_risk_scores_empty_diffs() {
        let scores = compute_file_risk_scores(&[], &empty_coverage(), &[]);
        assert!(scores.is_empty());
    }

    #[test]
    fn test_risk_scores_deleted_file_gets_bonus() {
        let diff = mock_diff(vec![mock_file_change(
            "src/old_module.rs",
            FileStatus::Deleted,
            0,
            50,
        )]);
        let scores = compute_file_risk_scores(&[diff], &empty_coverage(), &[]);
        assert_eq!(scores.len(), 1);
        assert_eq!(scores[0].path, "src/old_module.rs");
        assert!(scores[0].factors.contains(&"deleted"));
        assert!(scores[0].score >= 20, "Deletion should add at least +20");
    }

    #[test]
    fn test_risk_scores_test_file_excluded() {
        let diff = mock_diff(vec![
            mock_file_change("src/lib.rs", FileStatus::Modified, 100, 10),
            mock_file_change("tests/lib_test.rs", FileStatus::Modified, 50, 5),
        ]);
        let scores = compute_file_risk_scores(&[diff], &empty_coverage(), &[]);
        // Test file should be excluded
        assert!(
            scores.iter().all(|s| s.path != "tests/lib_test.rs"),
            "Test files should be excluded from risk scoring"
        );
    }

    #[test]
    fn security_path_uses_token_boundaries_not_substring() {
        let diff = mock_diff(vec![
            mock_file_change("src/author.rs", FileStatus::Modified, 5, 5),
            mock_file_change("src/tokenizer.rs", FileStatus::Modified, 5, 5),
            mock_file_change("src/auth/mod.rs", FileStatus::Modified, 5, 5),
        ]);
        let scores = compute_file_risk_scores(&[diff], &empty_coverage(), &[]);
        let security_paths: Vec<&str> = scores
            .iter()
            .filter(|s| s.factors.contains(&"security-path"))
            .map(|s| s.path.as_str())
            .collect();
        assert!(
            security_paths.contains(&"src/auth/mod.rs"),
            "a real auth path component must score security-path"
        );
        assert!(
            !security_paths.contains(&"src/author.rs"),
            "'author' must not substring-match 'auth'"
        );
        assert!(
            !security_paths.contains(&"src/tokenizer.rs"),
            "'tokenizer' must not substring-match 'token'"
        );
    }

    #[test]
    fn multi_base_duplicate_files_are_deduped() {
        let make = || mock_file_change("src/auth/handler.rs", FileStatus::Modified, 50, 50);
        let diff_a = mock_diff(vec![make()]);
        let diff_b = mock_diff(vec![make()]);
        let scores = compute_file_risk_scores(&[diff_a, diff_b], &empty_coverage(), &[]);
        let count = scores
            .iter()
            .filter(|s| s.path == "src/auth/handler.rs")
            .count();
        assert_eq!(count, 1, "the same file across bases must be scored once");
    }

    #[test]
    fn test_risk_scores_factors_accumulate() {
        // File with security keyword + deleted + hotspot churn
        let diff = mock_diff(vec![mock_file_change(
            "src/auth/handler.rs",
            FileStatus::Deleted,
            0,
            100, // churn = 100 >= HOTSPOT_THRESHOLD (80)
        )]);
        let scores = compute_file_risk_scores(&[diff], &empty_coverage(), &[]);
        assert_eq!(scores.len(), 1);
        let s = &scores[0];
        assert!(s.factors.contains(&"deleted"), "Should have deleted factor");
        assert!(
            s.factors.contains(&"security-path"),
            "Should have security-path factor"
        );
        assert!(s.factors.contains(&"hotspot"), "Should have hotspot factor");
        // deleted(+20) + security(+15) + hotspot(+10) = 45
        assert_eq!(s.score, 45);
    }

    #[test]
    fn test_risk_scores_max_10_sorted_descending() {
        // Create 15 files, all with hotspot churn and varying deletions
        let files: Vec<FileChange> = (0..15)
            .map(|i| {
                mock_file_change(
                    &format!("src/mod_{i}.rs"),
                    if i < 5 {
                        FileStatus::Deleted
                    } else {
                        FileStatus::Modified
                    },
                    50 + i,
                    50 + i, // churn = 100+2i >= 80 -> hotspot
                )
            })
            .collect();
        let diff = mock_diff(files);
        let scores = compute_file_risk_scores(&[diff], &empty_coverage(), &[]);
        assert!(scores.len() <= 10, "Should return at most 10 results");
        // Verify descending order
        for w in scores.windows(2) {
            assert!(
                w[0].score >= w[1].score,
                "Scores should be sorted descending: {} >= {}",
                w[0].score,
                w[1].score
            );
        }
    }

    #[test]
    fn test_risk_scores_include_domain_zones() {
        let diff = mock_diff(vec![mock_file_change(
            "src/auth/handler.rs",
            FileStatus::Modified,
            50,
            10,
        )]);
        let scores = compute_file_risk_scores(&[diff], &empty_coverage(), &[]);
        assert_eq!(scores.len(), 1);
        assert!(
            scores[0].zones.contains(&"auth/session"),
            "auth path should map to auth/session zone"
        );
        assert!(
            scores[0].zones.contains(&"api/public"),
            "handler path should map to api/public zone"
        );
    }

    #[test]
    fn test_risk_heatmap_aggregates_zones() {
        let diff = mock_diff(vec![
            mock_file_change("src/auth/login.rs", FileStatus::Modified, 30, 10),
            mock_file_change("src/auth/session.rs", FileStatus::Modified, 20, 5),
            mock_file_change("src/db/migration.rs", FileStatus::Modified, 50, 50),
        ]);
        let scores = compute_file_risk_scores(std::slice::from_ref(&diff), &empty_coverage(), &[]);
        let heatmap = compute_risk_heatmap(&[diff], &scores);

        assert!(
            heatmap.zones.iter().any(|z| z.name == "auth/session"),
            "Should have auth/session zone"
        );
        let auth_zone = heatmap
            .zones
            .iter()
            .find(|z| z.name == "auth/session")
            .unwrap();
        assert_eq!(auth_zone.files_touched, 2);
    }

    #[test]
    fn test_risk_heatmap_levels() {
        // Low risk
        let heatmap = RiskHeatmap {
            zones: vec![],
            total_risk_score: 10,
            risk_level: "low",
        };
        assert_eq!(heatmap.risk_level, "low");
    }
}
