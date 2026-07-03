//! Signal generators — domain-specific artifact producers.
//!
//! Split from monolithic signal.rs for maintainability.
//! Each submodule owns one signal domain.

mod breaking;
mod checks_log;
mod common;
pub mod consistency;
mod coverage;
mod deps;
mod diffs;
pub mod ghost_refs;
mod i18n;
mod patterns;
pub mod public_api;
mod risk;
pub mod semantic;
pub mod tauri_commands;
pub mod unsafe_audit;

#[cfg(test)]
mod test_helpers;

// Re-export facade — callers use `signal::*` unchanged.
// `pub` and `pub(crate)` items from each submodule are re-exported and
// accessible as `signal::X` within this crate (including via glob import).
// `pub(super)` items (e.g. parse_patch_new_start) remain internal to signal/.
pub use breaking::*;
pub use checks_log::*;
pub use common::*;
pub use consistency::*;
pub use coverage::*;
pub use deps::*;
pub use diffs::*;
pub use ghost_refs::*;
pub use i18n::*;
pub use patterns::*;
pub use public_api::*;
pub use risk::*;
pub use semantic::*;
pub use tauri_commands::*;
pub use unsafe_audit::*;

#[cfg(test)]
mod facade_tests {
    use super::*;

    #[test]
    fn signal_facade_reexports_legacy_monolith_surface() {
        let legacy_removed_symbol_names = [
            "generate_checks_errors_log",
            "BreakingRisk",
            "BreakingFinding",
            "BreakingKind",
            "analyze_all_breaking_changes",
            "CoverageDelta",
            "CoverageFile",
            "CoveragePair",
            "write_breaking_changes",
            "CoverageSignal",
            "compute_coverage_signal",
            "generate_coverage_delta",
            "generate_per_file_diffs",
            "FileRiskScore",
            "compute_file_risk_scores",
            "I18nDelta",
            "compute_i18n_delta",
            "PatternHit",
            "generate_pattern_scan",
            "DepsDelta",
            "generate_deps_delta",
        ];

        assert_eq!(legacy_removed_symbol_names.len(), 21);

        let _ = generate_checks_errors_log;
        let _ = analyze_all_breaking_changes;
        let _ = write_breaking_changes;
        let _ = compute_coverage_signal;
        let _ = generate_coverage_delta;
        let _ = generate_per_file_diffs;
        let _ = compute_file_risk_scores;
        let _ = compute_i18n_delta;
        let _ = generate_pattern_scan;
        let _ = generate_deps_delta;

        let _: Option<BreakingRisk> = None;
        let _: Option<BreakingFinding> = None;
        let _: Option<BreakingKind> = None;
        let _: Option<CoverageDelta> = None;
        let _: Option<CoverageFile> = None;
        let _: Option<CoveragePair> = None;
        let _: Option<CoverageSignal> = None;
        let _: Option<FileRiskScore> = None;
        let _: Option<I18nDelta> = None;
        let _: Option<PatternHit> = None;
        let _: Option<DepsDelta> = None;
    }
}
