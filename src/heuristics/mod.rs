//! Heuristics module - structural analysis
//!
//! Integrates with:
//! - Loctree: dead code, circular imports, complexity, exact twins — universal,
//!   including JS/TS source.

mod loctree;

pub use loctree::{CycleInfo, DeadExport, DeadParrot, LoctreeAnalysis, TwinsAnalysis, run_loctree};

use crate::Config;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::Path;

/// Combined heuristics results
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HeuristicsResult {
    pub loctree: Option<LoctreeAnalysis>,
    pub summary: HeuristicsSummary,
    /// Path used for analysis (snapshot or repo root). None = local cwd.
    pub analysis_root: Option<String>,
    /// Regression delta (base vs target heuristics). None if no base available.
    pub regression: Option<HeuristicsRegression>,
}

/// Delta between base and target heuristics runs
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HeuristicsRegression {
    pub base_sha: String,
    pub target_sha: String,
    pub dead_exports_delta: i64,
    pub cycles_delta: i64,
    #[serde(rename = "unused_symbols_delta", alias = "dead_parrots_delta")]
    pub dead_parrots_delta: i64,
    pub base_dead_exports: usize,
    pub target_dead_exports: usize,
    pub base_circular_imports: usize,
    pub target_circular_imports: usize,
    #[serde(rename = "base_unused_symbols", alias = "base_dead_parrots")]
    pub base_dead_parrots: usize,
    #[serde(rename = "target_unused_symbols", alias = "target_dead_parrots")]
    pub target_dead_parrots: usize,
    pub regression_detected: bool,
    pub improvement_detected: bool,
}

impl HeuristicsRegression {
    pub fn unused_symbols_delta(&self) -> i64 {
        self.dead_parrots_delta
    }

    pub fn base_unused_symbols(&self) -> usize {
        self.base_dead_parrots
    }

    pub fn target_unused_symbols(&self) -> usize {
        self.target_dead_parrots
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct HeuristicsSummary {
    pub total_files: usize,
    pub total_loc: usize,
    pub dead_exports: usize,
    pub circular_imports: usize,
    pub dead_parrots: usize,
    pub exact_twins: usize,
}

/// Run all heuristics.
///
/// `analysis_root` overrides `config.repo_root` as the directory to scan.
/// Used in snapshot mode to point heuristics at an extracted git tree.
pub async fn run_all(config: &Config, analysis_root: Option<&Path>) -> Result<HeuristicsResult> {
    use colored::Colorize;

    if !config.run_heuristics {
        return Ok(HeuristicsResult::default());
    }
    let root = analysis_root.unwrap_or(&config.repo_root);
    let emit_human_stdout = !config.json && !config.quiet;

    if emit_human_stdout {
        println!("{}", "Running heuristics...".cyan());
        if analysis_root.is_some() {
            println!("  {} Analysis root: {}", "ℹ".blue(), root.display());
        }
        println!();
    }

    let mut result = HeuristicsResult::default();

    // Run loctree
    match run_loctree(root).await {
        Ok(analysis) => {
            result.summary.total_files = analysis.stats.total_files;
            result.summary.total_loc = analysis.stats.total_loc;
            result.summary.dead_exports = analysis.dead_exports.len();
            result.summary.circular_imports = analysis.cycles.len();
            result.summary.dead_parrots = analysis.twins.dead_parrots.len();
            result.summary.exact_twins = analysis.twins.exact_twins.len();

            let status = if analysis.dead_exports.is_empty() && analysis.cycles.is_empty() {
                "✓".green()
            } else {
                "⚠".yellow()
            };

            if emit_human_stdout {
                println!(
                    "  {} Loctree: {} files, {} LOC, {} dead exports, {} unused symbols, {} cycles",
                    status,
                    analysis.stats.total_files,
                    analysis.stats.total_loc,
                    analysis.dead_exports.len(),
                    analysis.twins.dead_parrots.len(),
                    analysis.cycles.len()
                );
            }

            result.loctree = Some(analysis);
        }
        Err(e) => {
            // Honest degraded status: loctree produced no signal this run, so
            // say so instead of leaving `result.loctree = None` behind a green
            // line. Downstream (compute_delta_checked, findings) treats the
            // absent signal as "skipped", never as a healthy zero.
            if emit_human_stdout {
                println!("  {} Loctree: not available ({})", "○".dimmed(), e);
            }
        }
    }

    if emit_human_stdout {
        println!();
    }

    Ok(result)
}

/// Compute delta between base and target heuristics snapshots.
pub fn compute_delta(
    base: &HeuristicsResult,
    target: &HeuristicsResult,
    base_sha: &str,
    target_sha: &str,
) -> HeuristicsRegression {
    let dead_delta = target.summary.dead_exports as i64 - base.summary.dead_exports as i64;
    let cycles_delta =
        target.summary.circular_imports as i64 - base.summary.circular_imports as i64;
    let parrots_delta = target.summary.dead_parrots as i64 - base.summary.dead_parrots as i64;

    HeuristicsRegression {
        base_sha: base_sha.to_string(),
        target_sha: target_sha.to_string(),
        dead_exports_delta: dead_delta,
        cycles_delta,
        dead_parrots_delta: parrots_delta,
        base_dead_exports: base.summary.dead_exports,
        target_dead_exports: target.summary.dead_exports,
        base_circular_imports: base.summary.circular_imports,
        target_circular_imports: target.summary.circular_imports,
        base_dead_parrots: base.summary.dead_parrots,
        target_dead_parrots: target.summary.dead_parrots,
        regression_detected: dead_delta > 0 || cycles_delta > 0 || parrots_delta > 0,
        improvement_detected: dead_delta < 0 || cycles_delta < 0 || parrots_delta < 0,
    }
}

/// True only when loctree actually produced a signal for this result.
///
/// A failed loctree run leaves `loctree = None` (see `run_all`), so its zeroed
/// summary is meaningless. Availability is the honest gate before any delta.
fn loctree_available(result: &HeuristicsResult) -> bool {
    result
        .loctree
        .as_ref()
        .map(|l| l.available)
        .unwrap_or(false)
}

/// Compute a regression delta only when BOTH sides carry a real loctree signal.
///
/// This is the fail-open guard for the regression surface: if either side is
/// blind (loctree failed → not available), its zeroed dead-export/cycle counts
/// would manufacture a false regression (blind base → target's N reads as +N)
/// or a false improvement (blind target → base's N reads as -N). Returning
/// `None` makes the caller report "no signal" instead of a fabricated delta.
pub fn compute_delta_checked(
    base: &HeuristicsResult,
    target: &HeuristicsResult,
    base_sha: &str,
    target_sha: &str,
) -> Option<HeuristicsRegression> {
    if !loctree_available(base) || !loctree_available(target) {
        return None;
    }
    Some(compute_delta(base, target, base_sha, target_sha))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_heuristics_result_default() {
        let result = HeuristicsResult::default();
        assert!(result.loctree.is_none());
        assert_eq!(result.summary.total_files, 0);
        assert_eq!(result.summary.total_loc, 0);
        assert!(result.analysis_root.is_none());
        assert!(result.regression.is_none());
    }

    #[test]
    fn test_heuristics_summary_default() {
        let summary = HeuristicsSummary::default();
        assert_eq!(summary.total_files, 0);
        assert_eq!(summary.total_loc, 0);
        assert_eq!(summary.dead_exports, 0);
        assert_eq!(summary.circular_imports, 0);
    }

    #[test]
    fn test_heuristics_summary_creation() {
        let summary = HeuristicsSummary {
            total_files: 100,
            total_loc: 5000,
            dead_exports: 5,
            circular_imports: 2,
            dead_parrots: 0,
            exact_twins: 0,
        };
        assert_eq!(summary.total_files, 100);
        assert_eq!(summary.total_loc, 5000);
        assert_eq!(summary.dead_exports, 5);
        assert_eq!(summary.circular_imports, 2);
    }

    #[test]
    fn test_heuristics_result_with_summary() {
        let result = HeuristicsResult {
            summary: HeuristicsSummary {
                total_files: 50,
                total_loc: 2500,
                dead_exports: 3,
                circular_imports: 1,
                dead_parrots: 0,
                exact_twins: 0,
            },
            ..Default::default()
        };
        assert!(result.loctree.is_none());
        assert_eq!(result.summary.total_files, 50);
    }

    #[test]
    fn test_heuristics_result_clone() {
        let original = HeuristicsResult {
            summary: HeuristicsSummary {
                total_files: 25,
                total_loc: 1000,
                ..Default::default()
            },
            ..Default::default()
        };
        let cloned = original.clone();
        assert_eq!(original.summary.total_files, cloned.summary.total_files);
        assert_eq!(original.summary.total_loc, cloned.summary.total_loc);
    }

    #[test]
    fn test_heuristics_summary_serialization() {
        let summary = HeuristicsSummary {
            total_files: 10,
            total_loc: 500,
            dead_exports: 2,
            circular_imports: 1,
            dead_parrots: 0,
            exact_twins: 0,
        };
        let json = serde_json::to_string(&summary).unwrap();
        assert!(json.contains("\"total_files\":10"));
        assert!(json.contains("\"total_loc\":500"));
    }

    #[test]
    fn test_heuristics_summary_deserialization() {
        let json = r#"{"total_files":20,"total_loc":1000,"dead_exports":1,"circular_imports":0}"#;
        let summary: HeuristicsSummary = serde_json::from_str(json).unwrap();
        assert_eq!(summary.total_files, 20);
        assert_eq!(summary.total_loc, 1000);
        assert_eq!(summary.dead_exports, 1);
    }

    fn available_result(dead: usize, cycles: usize) -> HeuristicsResult {
        HeuristicsResult {
            summary: HeuristicsSummary {
                dead_exports: dead,
                circular_imports: cycles,
                ..Default::default()
            },
            loctree: Some(LoctreeAnalysis {
                available: true,
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn compute_delta_checked_skips_when_a_side_is_blind() {
        // A failed loctree run leaves loctree=None (not available). Its zeroed
        // summary must NOT become a real delta: a target with 5 dead exports
        // against a blind base would otherwise manufacture a +5 "regression".
        let blind_base = HeuristicsResult {
            loctree: None,
            ..Default::default()
        };
        let target = available_result(5, 0);
        assert!(
            compute_delta_checked(&blind_base, &target, "a", "b").is_none(),
            "blind base must not produce a false regression"
        );

        // Symmetric: a blind target must not read as a false improvement.
        let base = available_result(5, 0);
        let blind_target = HeuristicsResult {
            loctree: None,
            ..Default::default()
        };
        assert!(
            compute_delta_checked(&base, &blind_target, "a", "b").is_none(),
            "blind target must not produce a false improvement"
        );
    }

    #[test]
    fn compute_delta_checked_computes_when_both_available() {
        let base = available_result(3, 1);
        let target = available_result(5, 1);
        let reg = compute_delta_checked(&base, &target, "a", "b")
            .expect("both sides carry a real loctree signal");
        assert_eq!(reg.dead_exports_delta, 2);
        assert!(reg.regression_detected);
    }

    #[test]
    fn test_compute_delta_no_change() {
        let base = HeuristicsResult {
            summary: HeuristicsSummary {
                dead_exports: 5,
                circular_imports: 2,
                dead_parrots: 3,
                ..Default::default()
            },
            ..Default::default()
        };
        let target = base.clone();
        let reg = compute_delta(&base, &target, "aaa", "bbb");
        assert_eq!(reg.dead_exports_delta, 0);
        assert_eq!(reg.cycles_delta, 0);
        assert!(!reg.regression_detected);
        assert!(!reg.improvement_detected);
    }

    #[test]
    fn test_compute_delta_detected() {
        let base = HeuristicsResult {
            summary: HeuristicsSummary {
                dead_exports: 3,
                circular_imports: 1,
                dead_parrots: 0,
                ..Default::default()
            },
            ..Default::default()
        };
        let target = HeuristicsResult {
            summary: HeuristicsSummary {
                dead_exports: 5,
                circular_imports: 4,
                dead_parrots: 0,
                ..Default::default()
            },
            ..Default::default()
        };
        let reg = compute_delta(&base, &target, "aaa", "bbb");
        assert_eq!(reg.dead_exports_delta, 2);
        assert_eq!(reg.cycles_delta, 3);
        assert!(reg.regression_detected);
        assert!(!reg.improvement_detected);
    }

    #[test]
    fn test_compute_delta_improvement() {
        let base = HeuristicsResult {
            summary: HeuristicsSummary {
                dead_exports: 10,
                circular_imports: 5,
                dead_parrots: 8,
                ..Default::default()
            },
            ..Default::default()
        };
        let target = HeuristicsResult {
            summary: HeuristicsSummary {
                dead_exports: 3,
                circular_imports: 2,
                dead_parrots: 4,
                ..Default::default()
            },
            ..Default::default()
        };
        let reg = compute_delta(&base, &target, "aaa", "bbb");
        assert_eq!(reg.dead_exports_delta, -7);
        assert_eq!(reg.cycles_delta, -3);
        assert_eq!(reg.dead_parrots_delta, -4);
        assert!(!reg.regression_detected);
        assert!(reg.improvement_detected);
    }

    #[test]
    fn test_compute_delta_mixed_signals() {
        // dead_exports goes up (regression), cycles goes down (improvement)
        // Both flags should be true simultaneously
        let base = HeuristicsResult {
            summary: HeuristicsSummary {
                dead_exports: 2,
                circular_imports: 6,
                dead_parrots: 0,
                ..Default::default()
            },
            ..Default::default()
        };
        let target = HeuristicsResult {
            summary: HeuristicsSummary {
                dead_exports: 5,
                circular_imports: 2,
                dead_parrots: 0,
                ..Default::default()
            },
            ..Default::default()
        };
        let reg = compute_delta(&base, &target, "base111", "target222");

        assert_eq!(reg.dead_exports_delta, 3, "dead_exports went up by 3");
        assert_eq!(reg.cycles_delta, -4, "cycles went down by 4");
        assert_eq!(reg.dead_parrots_delta, 0);
        assert!(
            reg.regression_detected,
            "dead_exports increase should trigger regression"
        );
        assert!(
            reg.improvement_detected,
            "cycles decrease should trigger improvement"
        );
        assert_eq!(reg.base_sha, "base111");
        assert_eq!(reg.target_sha, "target222");
        assert_eq!(reg.base_dead_exports, 2);
        assert_eq!(reg.target_dead_exports, 5);
        assert_eq!(reg.base_circular_imports, 6);
        assert_eq!(reg.target_circular_imports, 2);
    }

    #[test]
    fn test_regression_serialization() {
        let reg = HeuristicsRegression {
            base_sha: "abc".to_string(),
            target_sha: "def".to_string(),
            dead_exports_delta: 2,
            cycles_delta: -1,
            dead_parrots_delta: 0,
            base_dead_exports: 3,
            target_dead_exports: 5,
            base_circular_imports: 4,
            target_circular_imports: 3,
            base_dead_parrots: 2,
            target_dead_parrots: 2,
            regression_detected: true,
            improvement_detected: true,
        };
        let json = serde_json::to_string(&reg).unwrap();
        assert!(json.contains("\"dead_exports_delta\":2"));
        assert!(json.contains("\"regression_detected\":true"));
    }

    #[test]
    fn test_regression_unused_symbol_accessors_follow_serialized_names() {
        let reg = HeuristicsRegression {
            dead_parrots_delta: -3,
            base_dead_parrots: 9,
            target_dead_parrots: 6,
            ..Default::default()
        };

        assert_eq!(reg.unused_symbols_delta(), -3);
        assert_eq!(reg.base_unused_symbols(), 9);
        assert_eq!(reg.target_unused_symbols(), 6);
    }
}
