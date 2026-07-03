//! Unified regression score (v4)
//!
//! Computes a 0-100 score with severity levels and explainability.
//! Per-file counts are used for perf dimension (not per-hunk).
//! Reasons include top file names for actionability.

use super::deps::DepsRegression;
use super::diff::DiffRegression;
use super::perf::PerfRegression;
use super::tests::TestRegression;
use serde::{Deserialize, Serialize};

/// Max characters for a single reason string.
const REASON_MAX_LEN: usize = 120;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RegressionScore {
    pub score: u32,
    pub severity: Severity,
    pub score_reasons: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Severity {
    #[default]
    OK,
    LOW,
    MED,
    HIGH,
    CRITICAL,
}

impl Severity {
    pub fn as_str(&self) -> &'static str {
        match self {
            Severity::OK => "OK",
            Severity::LOW => "LOW",
            Severity::MED => "MED",
            Severity::HIGH => "HIGH",
            Severity::CRITICAL => "CRITICAL",
        }
    }
}

impl std::fmt::Display for Severity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Extract just the filename (basename) from a path.
fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// Build a suffix like ": foo.ts, bar.rs, ..." from a list of paths.
/// Caps at `max_names` entries and truncates to fit within `REASON_MAX_LEN`
/// when combined with the `prefix`.
fn top_files_suffix(files: &[String], prefix_len: usize, max_names: usize) -> String {
    if files.is_empty() {
        return String::new();
    }

    let names: Vec<&str> = files.iter().take(max_names).map(|f| basename(f)).collect();
    let remaining = files.len().saturating_sub(max_names);

    let mut suffix = String::from(": ");
    suffix.push_str(&names.join(", "));
    if remaining > 0 {
        suffix.push_str(&format!(", +{} more", remaining));
    }

    // Truncate if combined string would exceed REASON_MAX_LEN
    let budget = REASON_MAX_LEN.saturating_sub(prefix_len);
    if suffix.len() > budget {
        truncate_display_suffix(&mut suffix, budget);
    }

    suffix
}

fn truncate_display_suffix(value: &mut String, budget: usize) {
    if value.len() <= budget {
        return;
    }

    let ellipsis = if budget > 3 { "..." } else { "" };
    let mut truncate_at = budget.saturating_sub(ellipsis.len());
    while truncate_at > 0 && !value.is_char_boundary(truncate_at) {
        truncate_at -= 1;
    }
    value.truncate(truncate_at);
    value.push_str(ellipsis);
}

/// Collect file paths from `suspected_files` that have at least one reason containing `needle`.
fn perf_files_with_reason(perf: &PerfRegression, needle: &str) -> Vec<String> {
    perf.suspected_files
        .iter()
        .filter(|s| !s.test_context_only && s.reasons.iter().any(|r| r.contains(needle)))
        .map(|s| s.file.clone())
        .collect()
}

pub fn compute(
    diff: &DiffRegression,
    tests: &TestRegression,
    deps: &DepsRegression,
    perf: &PerfRegression,
) -> RegressionScore {
    let mut score: u32 = 0;
    let mut reasons: Vec<String> = Vec::new();

    // -- Diff dimension (max ~35 points, code-only metrics) --
    if diff.max_code_file_churn > 500 {
        let pts = ((diff.max_code_file_churn - 500) / 100).clamp(1, 15) as u32;
        score += pts;
        reasons.push(format!(
            "max code churn {} (+{})",
            diff.max_code_file_churn, pts
        ));
    }
    if diff.code_files_changed > 50 {
        let pts = ((diff.code_files_changed - 50) / 20).clamp(1, 5) as u32;
        score += pts;
        reasons.push(format!(
            "{} code files changed (+{})",
            diff.code_files_changed, pts
        ));
    }
    let code_churn = diff.code_insertions + diff.code_deletions;
    if code_churn > 2000 {
        let pts = ((code_churn - 2000) / 500).clamp(1, 15) as u32;
        score += pts;
        reasons.push(format!("{} code churn (+{})", code_churn, pts));
    }

    // -- Test dimension (max ~25 points) --
    if let Some(delta) = tests.coverage_delta
        && delta < -0.05
    {
        let pts = ((-delta * 100.0) as u32).min(15);
        score += pts;
        reasons.push(format!("coverage dropped {:.1}% (+{})", delta * 100.0, pts));
    }
    if tests.untested_code_count > 0 {
        let pts = (tests.untested_code_count as u32 * 2).min(10);
        score += pts;
        let prefix = format!(
            "{} untested code files (+{})",
            tests.untested_code_count, pts
        );
        let suffix = top_files_suffix(&tests.untested_code_files, prefix.len(), 3);
        reasons.push(format!("{}{}", prefix, suffix));
    }

    // -- Dependencies dimension (max ~20 points) --
    if deps.cycles_delta > 0 {
        let pts = ((deps.cycles_delta as u32).saturating_mul(3)).min(10);
        score += pts;
        reasons.push(format!("+{} cycles (+{})", deps.cycles_delta, pts));
    }
    if deps.dead_exports_delta > 0 {
        let pts = (deps.dead_exports_delta as u32).min(5);
        score += pts;
        reasons.push(format!(
            "+{} dead exports (+{})",
            deps.dead_exports_delta, pts
        ));
    }
    if deps.unused_symbols_delta > 0 {
        let pts = (deps.unused_symbols_delta as u32).min(5);
        score += pts;
        reasons.push(format!(
            "+{} unused symbols (+{})",
            deps.unused_symbols_delta, pts
        ));
    }

    // -- Perf dimension (max ~15 points) --
    // Use per-file counts from suspected_files, not per-hunk counts
    let query_files = perf_files_with_reason(perf, "query in loop");
    let query_files_count = query_files.len();
    if query_files_count > 0 {
        let pts = ((query_files_count as u32).saturating_mul(5)).min(10);
        score += pts;
        let prefix = format!("{} query-in-loop files (+{})", query_files_count, pts);
        let suffix = top_files_suffix(&query_files, prefix.len(), 3);
        reasons.push(format!("{}{}", prefix, suffix));
    }

    let clone_files = perf_files_with_reason(perf, "clone/collect");
    let clone_files_count = clone_files.len();
    if clone_files_count > 0 {
        let pts = ((clone_files_count as u32).saturating_mul(2)).min(5);
        score += pts;
        let prefix = format!(
            "{} clone/collect-in-loop files (+{})",
            clone_files_count, pts
        );
        let suffix = top_files_suffix(&clone_files, prefix.len(), 3);
        reasons.push(format!("{}{}", prefix, suffix));
    }

    // Cap at 100
    score = score.min(100);

    let severity = match score {
        0..=10 => Severity::OK,
        11..=25 => Severity::LOW,
        26..=50 => Severity::MED,
        51..=75 => Severity::HIGH,
        _ => Severity::CRITICAL,
    };

    RegressionScore {
        score,
        severity,
        score_reasons: reasons,
    }
}

#[cfg(test)]
mod score_tests {
    use super::super::perf::PerfSuspect;
    use super::*;

    fn empty_diff() -> DiffRegression {
        DiffRegression::default()
    }
    fn empty_tests() -> TestRegression {
        TestRegression::default()
    }
    fn empty_deps() -> DepsRegression {
        DepsRegression::default()
    }
    fn empty_perf() -> PerfRegression {
        PerfRegression::default()
    }

    // -- Part 1: Per-file perf counts --

    #[test]
    fn perf_query_uses_file_count_not_hunk_count() {
        // 1 file with query-in-loop reason, but hunk count = 5
        let perf = PerfRegression {
            query_in_loop_count: 5, // per-hunk (should be ignored by score)
            suspected_files: vec![PerfSuspect {
                file: "src/handler.rs".into(),
                reasons: vec!["query in loop".into()],
                ..Default::default()
            }],
            ..Default::default()
        };

        let result = compute(&empty_diff(), &empty_tests(), &empty_deps(), &perf);
        // 1 file * 5 = 5 points (not 5 hunks * 5 = 25)
        assert_eq!(result.score, 5);
        assert!(result.score_reasons[0].contains("1 query-in-loop files"));
    }

    #[test]
    fn perf_clone_uses_file_count_not_hunk_count() {
        let perf = PerfRegression {
            clone_collect_in_loop_count: 4, // per-hunk (ignored)
            suspected_files: vec![
                PerfSuspect {
                    file: "src/a.rs".into(),
                    reasons: vec!["clone/collect in loop".into()],
                    ..Default::default()
                },
                PerfSuspect {
                    file: "src/b.rs".into(),
                    reasons: vec!["clone/collect in loop".into()],
                    ..Default::default()
                },
            ],
            ..Default::default()
        };

        let result = compute(&empty_diff(), &empty_tests(), &empty_deps(), &perf);
        // 2 files * 2 = 4 points (not 4 hunks * 2 = 8)
        assert_eq!(result.score, 4);
        assert!(result.score_reasons[0].contains("2 clone/collect-in-loop files"));
    }

    #[test]
    fn perf_mixed_reasons_counted_separately() {
        // 1 file has both reasons, 1 file has only query
        let perf = PerfRegression {
            query_in_loop_count: 3,
            clone_collect_in_loop_count: 2,
            suspected_files: vec![
                PerfSuspect {
                    file: "src/a.rs".into(),
                    reasons: vec!["query in loop".into(), "clone/collect in loop".into()],
                    ..Default::default()
                },
                PerfSuspect {
                    file: "src/b.rs".into(),
                    reasons: vec!["query in loop".into()],
                    ..Default::default()
                },
            ],
            ..Default::default()
        };

        let result = compute(&empty_diff(), &empty_tests(), &empty_deps(), &perf);
        // query: 2 files * 5 = 10, clone: 1 file * 2 = 2 → total 12
        assert_eq!(result.score, 12);
    }

    #[test]
    fn perf_query_capped_at_10() {
        let suspects: Vec<PerfSuspect> = (0..5)
            .map(|i| PerfSuspect {
                file: format!("src/mod_{}.rs", i),
                reasons: vec!["query in loop".into()],
                ..Default::default()
            })
            .collect();
        let perf = PerfRegression {
            suspected_files: suspects,
            ..Default::default()
        };

        let result = compute(&empty_diff(), &empty_tests(), &empty_deps(), &perf);
        // 5 * 5 = 25, capped at 10
        assert_eq!(result.score, 10);
    }

    #[test]
    fn perf_clone_capped_at_5() {
        let suspects: Vec<PerfSuspect> = (0..10)
            .map(|i| PerfSuspect {
                file: format!("src/mod_{}.rs", i),
                reasons: vec!["clone/collect in loop".into()],
                ..Default::default()
            })
            .collect();
        let perf = PerfRegression {
            suspected_files: suspects,
            ..Default::default()
        };

        let result = compute(&empty_diff(), &empty_tests(), &empty_deps(), &perf);
        // 10 * 2 = 20, capped at 5
        assert_eq!(result.score, 5);
    }

    // -- Part 2: Actionable reasons --

    #[test]
    fn untested_reason_includes_file_names() {
        let tests = TestRegression {
            untested_code_count: 3,
            untested_code_files: vec![
                "src/TranscriptDock.tsx".into(),
                "src/useHook.ts".into(),
                "src/config.ts".into(),
            ],
            ..Default::default()
        };

        let result = compute(&empty_diff(), &tests, &empty_deps(), &empty_perf());
        let reason = &result.score_reasons[0];
        assert!(reason.contains("3 untested code files"));
        assert!(reason.contains("TranscriptDock.tsx"));
        assert!(reason.contains("useHook.ts"));
        assert!(reason.contains("config.ts"));
    }

    #[test]
    fn untested_reason_shows_top_3_with_overflow() {
        let tests = TestRegression {
            untested_code_count: 5,
            untested_code_files: vec![
                "src/a.ts".into(),
                "src/b.ts".into(),
                "src/c.ts".into(),
                "src/d.ts".into(),
                "src/e.ts".into(),
            ],
            ..Default::default()
        };

        let result = compute(&empty_diff(), &tests, &empty_deps(), &empty_perf());
        let reason = &result.score_reasons[0];
        assert!(reason.contains("a.ts"));
        assert!(reason.contains("b.ts"));
        assert!(reason.contains("c.ts"));
        assert!(reason.contains("+2 more"));
        // d.ts and e.ts should NOT appear
        assert!(!reason.contains("d.ts"));
    }

    #[test]
    fn perf_query_reason_includes_file_names() {
        let perf = PerfRegression {
            suspected_files: vec![
                PerfSuspect {
                    file: "src/useTranscriptDock.ts".into(),
                    reasons: vec!["query in loop".into()],
                    ..Default::default()
                },
                PerfSuspect {
                    file: "src/ensureRailData.ts".into(),
                    reasons: vec!["query in loop".into()],
                    ..Default::default()
                },
            ],
            ..Default::default()
        };

        let result = compute(&empty_diff(), &empty_tests(), &empty_deps(), &perf);
        let reason = &result.score_reasons[0];
        assert!(reason.contains("2 query-in-loop files"));
        assert!(reason.contains("useTranscriptDock.ts"));
        assert!(reason.contains("ensureRailData.ts"));
    }

    #[test]
    fn perf_clone_reason_includes_file_names() {
        let perf = PerfRegression {
            suspected_files: vec![PerfSuspect {
                file: "src/processor.rs".into(),
                reasons: vec!["clone/collect in loop".into()],
                ..Default::default()
            }],
            ..Default::default()
        };

        let result = compute(&empty_diff(), &empty_tests(), &empty_deps(), &perf);
        let reason = &result.score_reasons[0];
        assert!(reason.contains("1 clone/collect-in-loop files"));
        assert!(reason.contains("processor.rs"));
    }

    #[test]
    fn reason_length_capped() {
        // Generate files with very long paths to test truncation
        let files: Vec<String> = (0..3)
            .map(|i| format!("src/very/deeply/nested/module/submodule/component_{}_with_extremely_long_name.tsx", i))
            .collect();

        let tests = TestRegression {
            untested_code_count: 3,
            untested_code_files: files,
            ..Default::default()
        };

        let result = compute(&empty_diff(), &tests, &empty_deps(), &empty_perf());
        let reason = &result.score_reasons[0];
        assert!(reason.len() <= REASON_MAX_LEN);
    }

    #[test]
    fn reason_length_capped_on_utf8_boundary() {
        let files = vec![
            "src/zażółć/gęślą/jaźń_component.rs".to_string(),
            "src/éxample/über_mod.rs".to_string(),
            "src/東京/module.rs".to_string(),
        ];

        let tests = TestRegression {
            untested_code_count: files.len(),
            untested_code_files: files,
            ..Default::default()
        };

        let result = compute(&empty_diff(), &tests, &empty_deps(), &empty_perf());
        let reason = &result.score_reasons[0];
        assert!(reason.len() <= REASON_MAX_LEN);
        assert!(std::str::from_utf8(reason.as_bytes()).is_ok());
    }

    // -- Part 3: Code-only diff scoring --

    #[test]
    fn diff_score_uses_code_metrics_not_total() {
        // Total: 80 files, 5000 churn — would score high with old metrics
        // Code-only: 5 files, 200 churn — scores 0 with code-only
        let diff = DiffRegression {
            files_changed: 80,
            insertions: 3000,
            deletions: 2000,
            max_file_churn: 800,
            code_files_changed: 5,
            code_insertions: 120,
            code_deletions: 80,
            max_code_file_churn: 100,
            test_files_changed: 60,
            ..Default::default()
        };

        let result = compute(&diff, &empty_tests(), &empty_deps(), &empty_perf());
        // Code-only: max_code=100 (<500), code_files=5 (<50), code_churn=200 (<2000)
        // All below thresholds → 0 points from diff
        assert_eq!(result.score, 0);
        assert!(result.score_reasons.is_empty());
    }

    #[test]
    fn diff_score_triggers_on_code_churn() {
        let diff = DiffRegression {
            code_files_changed: 10,
            code_insertions: 500,
            code_deletions: 300,
            max_code_file_churn: 600, // > 500 threshold
            ..Default::default()
        };

        let result = compute(&diff, &empty_tests(), &empty_deps(), &empty_perf());
        assert!(result.score > 0);
        assert!(result.score_reasons[0].contains("max code churn 600"));
    }

    #[test]
    fn diff_score_no_dead_zone() {
        // Churn 501 — just over threshold, should still get at least 1 point
        let diff = DiffRegression {
            max_code_file_churn: 501,
            ..Default::default()
        };
        let result = compute(&diff, &empty_tests(), &empty_deps(), &empty_perf());
        assert_eq!(result.score, 1, "501 churn must award at least 1 point");

        // 51 code files — just over threshold
        let diff2 = DiffRegression {
            code_files_changed: 51,
            ..Default::default()
        };
        let result2 = compute(&diff2, &empty_tests(), &empty_deps(), &empty_perf());
        assert_eq!(result2.score, 1, "51 files must award at least 1 point");

        // 2001 code churn — just over threshold
        let diff3 = DiffRegression {
            code_insertions: 1500,
            code_deletions: 501,
            ..Default::default()
        };
        let result3 = compute(&diff3, &empty_tests(), &empty_deps(), &empty_perf());
        assert_eq!(result3.score, 1, "2001 churn must award at least 1 point");
    }

    // -- Severity thresholds unchanged --

    #[test]
    fn severity_thresholds_unchanged() {
        // Zero score → OK
        let r = compute(&empty_diff(), &empty_tests(), &empty_deps(), &empty_perf());
        assert_eq!(r.severity, Severity::OK);
        assert_eq!(r.score, 0);

        // Score 11 → LOW
        let tests = TestRegression {
            untested_code_count: 5,
            untested_code_files: (0..5).map(|i| format!("src/{}.rs", i)).collect(),
            ..Default::default()
        };
        let perf = PerfRegression {
            suspected_files: vec![PerfSuspect {
                file: "src/x.rs".into(),
                reasons: vec!["clone/collect in loop".into()],
                ..Default::default()
            }],
            ..Default::default()
        };
        // tests: 5*2=10, clone: 1*2=2 → 12
        let r = compute(&empty_diff(), &tests, &empty_deps(), &perf);
        assert_eq!(r.score, 12);
        assert_eq!(r.severity, Severity::LOW);
    }

    #[test]
    fn perf_score_ignores_test_only_suspects() {
        let perf = PerfRegression {
            suspected_files: vec![PerfSuspect {
                file: "src/portal.rs".into(),
                reasons: vec!["query in loop".into(), "clone/collect in loop".into()],
                test_context_only: true,
                ..Default::default()
            }],
            ..Default::default()
        };

        let result = compute(&empty_diff(), &empty_tests(), &empty_deps(), &perf);
        assert_eq!(result.score, 0);
        assert!(result.score_reasons.is_empty());
    }

    // -- Helpers --

    #[test]
    fn basename_extracts_filename() {
        assert_eq!(basename("src/foo/bar.rs"), "bar.rs");
        assert_eq!(basename("bar.rs"), "bar.rs");
        assert_eq!(basename("a/b/c/d.tsx"), "d.tsx");
    }

    #[test]
    fn top_files_suffix_empty() {
        assert_eq!(top_files_suffix(&[], 20, 3), "");
    }

    #[test]
    fn top_files_suffix_few_files() {
        let files = vec!["src/a.ts".into(), "src/b.ts".into()];
        let s = top_files_suffix(&files, 20, 3);
        assert_eq!(s, ": a.ts, b.ts");
    }

    #[test]
    fn top_files_suffix_overflow() {
        let files: Vec<String> = (0..5).map(|i| format!("src/{}.ts", i)).collect();
        let s = top_files_suffix(&files, 20, 3);
        assert!(s.contains("0.ts"));
        assert!(s.contains("1.ts"));
        assert!(s.contains("2.ts"));
        assert!(s.contains("+2 more"));
    }
}
