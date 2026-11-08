//! Dependency/architecture regression analysis (v3)
//!
//! Computes deltas for cycles, dead exports, and unused symbols from the
//! loctree heuristic's counts (current vs base snapshot).

use super::RegressionContext;
use serde::{Deserialize, Serialize};

/// Maximum items in capped lists.
const MAX_LIST_SIZE: usize = 20;

/// Unknown is the safe read-back default for reports predating availability.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum EvidenceStatus {
    Measured,
    NotRun,
    #[default]
    Unknown,
}

#[derive(Debug, Clone, Default)]
pub struct DependencyEvidence {
    pub status: EvidenceStatus,
    pub reason: Option<String>,
}

pub fn current_evidence(
    enabled: bool,
    heuristics: Option<&crate::heuristics::HeuristicsResult>,
) -> DependencyEvidence {
    let (status, reason) = if !enabled {
        (EvidenceStatus::NotRun, Some("heuristics disabled"))
    } else if let Some(h) = heuristics {
        if !h.loctree.as_ref().is_some_and(|l| l.available) {
            (EvidenceStatus::Unknown, Some("loctree unavailable"))
        } else if h.summary.total_files == 0 {
            (EvidenceStatus::NotRun, Some("loctree scanned no files"))
        } else {
            (EvidenceStatus::Measured, None)
        }
    } else {
        (EvidenceStatus::NotRun, Some("heuristics not run"))
    };
    DependencyEvidence {
        status,
        reason: reason.map(str::to_owned),
    }
}

pub fn baseline_evidence(
    regression: Option<&crate::heuristics::HeuristicsRegression>,
) -> DependencyEvidence {
    let (status, reason) = match regression {
        None => (EvidenceStatus::NotRun, Some("no measured baseline")),
        Some(reg) => match reg.base_scanned_files {
            Some(0) => (
                EvidenceStatus::NotRun,
                Some("baseline loctree scanned no files"),
            ),
            Some(_) => (EvidenceStatus::Measured, None),
            None => (
                EvidenceStatus::Unknown,
                Some("baseline scan evidence not recorded"),
            ),
        },
    };
    DependencyEvidence {
        status,
        reason: reason.map(str::to_owned),
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DepsRegression {
    #[serde(default)]
    pub current_status: EvidenceStatus,
    #[serde(default)]
    pub current_reason: Option<String>,
    #[serde(default)]
    pub comparison_status: EvidenceStatus,
    #[serde(default)]
    pub comparison_reason: Option<String>,
    // Legacy counters require the corresponding MEASURED status. Their numeric
    // representation and the scoring algorithm remain compatible.
    pub cycles_delta: i64,
    pub dead_exports_delta: i64,
    pub unused_symbols_delta: i64,
    pub current_cycles: usize,
    pub current_dead_exports: usize,
    pub current_unused_symbols: usize,
    pub top_cycles: Vec<String>,
    pub top_unused: Vec<String>,
    pub dependency_regression_detected: bool,
}

pub fn analyze(ctx: &RegressionContext) -> DepsRegression {
    let cycles_delta = ctx
        .base_cycles
        .map(|base| ctx.cycles as i64 - base as i64)
        .unwrap_or(0);
    let dead_exports_delta = ctx
        .base_dead_exports
        .map(|base| ctx.dead_exports as i64 - base as i64)
        .unwrap_or(0);
    let unused_symbols_delta = ctx
        .base_unused_symbols
        .map(|base| ctx.unused_symbols as i64 - base as i64)
        .unwrap_or(0);

    let detected = cycles_delta > 0 || dead_exports_delta > 0 || unused_symbols_delta > 0;

    let mut top_cycles = ctx.top_cycles.clone();
    top_cycles.truncate(MAX_LIST_SIZE);
    let mut top_unused = ctx.top_unused.clone();
    top_unused.truncate(MAX_LIST_SIZE);

    let has_base = ctx.base_cycles.is_some()
        && ctx.base_dead_exports.is_some()
        && ctx.base_unused_symbols.is_some();
    let comparison = if ctx.deps_availability.status != EvidenceStatus::Measured {
        ctx.deps_availability.clone()
    } else if ctx.deps_base_availability.status != EvidenceStatus::Measured {
        ctx.deps_base_availability.clone()
    } else if has_base {
        DependencyEvidence {
            status: EvidenceStatus::Measured,
            reason: None,
        }
    } else {
        DependencyEvidence {
            status: EvidenceStatus::Unknown,
            reason: Some("baseline counters incomplete".to_owned()),
        }
    };

    DepsRegression {
        current_status: ctx.deps_availability.status,
        current_reason: ctx.deps_availability.reason.clone(),
        comparison_status: comparison.status,
        comparison_reason: comparison.reason,
        cycles_delta,
        dead_exports_delta,
        unused_symbols_delta,
        current_cycles: ctx.cycles,
        current_dead_exports: ctx.dead_exports,
        current_unused_symbols: ctx.unused_symbols,
        top_cycles,
        top_unused,
        dependency_regression_detected: detected,
    }
}

#[cfg(test)]
mod availability_tests {
    use super::*;
    use crate::heuristics::{HeuristicsResult, LoctreeAnalysis};

    #[test]
    fn dependency_measurement_rejects_disabled_unavailable_and_zero_file_scans() {
        let mut h = HeuristicsResult::default();
        assert_eq!(
            current_evidence(false, Some(&h)).status,
            EvidenceStatus::NotRun
        );
        assert_eq!(current_evidence(true, None).status, EvidenceStatus::NotRun);
        assert_eq!(
            current_evidence(true, Some(&h)).status,
            EvidenceStatus::Unknown
        );
        h.loctree = Some(LoctreeAnalysis {
            available: true,
            ..Default::default()
        });
        assert_eq!(
            current_evidence(true, Some(&h)).status,
            EvidenceStatus::NotRun
        );
        h.summary.total_files = 1;
        assert_eq!(
            current_evidence(true, Some(&h)).status,
            EvidenceStatus::Measured
        );
    }

    #[test]
    fn dependency_current_measurement_and_baseline_availability_are_separate() {
        let mut ctx = RegressionContext {
            cycles: 3,
            deps_availability: DependencyEvidence {
                status: EvidenceStatus::Measured,
                reason: None,
            },
            deps_base_availability: baseline_evidence(None),
            ..Default::default()
        };
        let without_base = analyze(&ctx);
        assert_eq!(without_base.current_status, EvidenceStatus::Measured);
        assert_eq!(without_base.current_cycles, 3);
        assert_eq!(without_base.comparison_status, EvidenceStatus::NotRun);
        assert_eq!(
            without_base.comparison_reason.as_deref(),
            Some("no measured baseline")
        );
        ctx.base_cycles = Some(1);
        ctx.base_dead_exports = Some(0);
        ctx.base_unused_symbols = Some(0);
        ctx.deps_base_availability =
            baseline_evidence(Some(&crate::heuristics::HeuristicsRegression {
                base_scanned_files: Some(1),
                ..Default::default()
            }));
        let measured = analyze(&ctx);
        assert_eq!(measured.comparison_status, EvidenceStatus::Measured);
        assert_eq!(measured.cycles_delta, 2);
        assert!(measured.dependency_regression_detected);
        let measured_score = super::super::compute_regression(&ctx).score.score;
        ctx.deps_availability = DependencyEvidence::default();
        assert_eq!(
            super::super::compute_regression(&ctx).score.score,
            measured_score
        );
        assert_eq!(analyze(&ctx).current_status, EvidenceStatus::Unknown);
    }

    #[test]
    fn legacy_dependency_report_defaults_to_unknown_not_measured_zero() {
        let mut wire = serde_json::to_value(DepsRegression::default()).unwrap();
        for field in [
            "current_status",
            "current_reason",
            "comparison_status",
            "comparison_reason",
        ] {
            wire.as_object_mut().unwrap().remove(field);
        }
        let read: DepsRegression = serde_json::from_value(wire).unwrap();
        assert_eq!(read.current_status, EvidenceStatus::Unknown);
        assert_eq!(read.comparison_status, EvidenceStatus::Unknown);
    }

    #[test]
    fn dependency_comparison_requires_real_baseline_scan_scope() {
        use crate::heuristics::{HeuristicsSummary, compute_delta_checked};
        let mut base = HeuristicsResult {
            loctree: Some(LoctreeAnalysis {
                available: true,
                ..Default::default()
            }),
            ..Default::default()
        };
        let target = HeuristicsResult {
            loctree: Some(LoctreeAnalysis {
                available: true,
                ..Default::default()
            }),
            summary: HeuristicsSummary {
                total_files: 2,
                circular_imports: 3,
                ..Default::default()
            },
            ..Default::default()
        };
        for (files, expected) in [(0, EvidenceStatus::NotRun), (1, EvidenceStatus::Measured)] {
            base.summary.total_files = files;
            let reg = compute_delta_checked(&base, &target, "a", "b").unwrap();
            assert_eq!(reg.base_scanned_files, Some(files));
            let ctx = RegressionContext {
                cycles: target.summary.circular_imports,
                base_cycles: Some(reg.base_circular_imports),
                base_dead_exports: Some(reg.base_dead_exports),
                base_unused_symbols: Some(reg.base_unused_symbols()),
                deps_availability: current_evidence(true, Some(&target)),
                deps_base_availability: baseline_evidence(Some(&reg)),
                ..Default::default()
            };
            let comparison = analyze(&ctx);
            assert_eq!(comparison.current_status, EvidenceStatus::Measured);
            assert_eq!(comparison.comparison_status, expected);
            // New metadata never changes the legacy delta or score inputs.
            assert_eq!(comparison.cycles_delta, 3);
        }
        let old = crate::heuristics::HeuristicsRegression::default();
        let mut wire = serde_json::to_value(&old).unwrap();
        wire.as_object_mut().unwrap().remove("base_scanned_files");
        let old: crate::heuristics::HeuristicsRegression = serde_json::from_value(wire).unwrap();
        assert_eq!(
            baseline_evidence(Some(&old)).status,
            EvidenceStatus::Unknown
        );
    }
}
