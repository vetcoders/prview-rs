//! Dependency/architecture regression analysis (v3)
//!
//! Computes deltas for cycles, dead exports, and unused symbols from the
//! loctree heuristic's counts (current vs base snapshot).

use super::RegressionContext;
use serde::{Deserialize, Serialize};

/// Maximum items in capped lists.
const MAX_LIST_SIZE: usize = 20;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DepsRegression {
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

    DepsRegression {
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
