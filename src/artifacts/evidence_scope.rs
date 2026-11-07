//! Additive descriptions of evidence. No policy axes are computed here.

use super::*;
use serde_json::{Value, json};

pub(crate) fn coverage_evidence(coverage: &CoverageDelta) -> Value {
    json!({
        "kind": "source_test_change_match",
        "execution_coverage_measured": false,
        "measured_field_meaning": "heuristic_evaluated",
        "weak_match_count": coverage.covered.iter().filter(|pair| pair.tier == signal::CoverageMatchTier::Low).count(),
    })
}

pub(crate) fn loctree_evidence(heuristics: Option<&HeuristicsResult>) -> Value {
    let comparison = heuristics.and_then(|h| h.regression.as_ref());
    json!({
        "scope": "whole_repository",
        "comparison_kind": if comparison.is_some() { "net_count_delta" } else { "not_available" },
        "finding_identities_compared": false,
        "net_counts_unchanged": comparison.map(|reg| reg.dead_exports_delta == 0 && reg.cycles_delta == 0 && reg.unused_symbols_delta() == 0),
        "count_deltas": comparison.map(|reg| json!({ "dead_exports": reg.dead_exports_delta, "cycles": reg.cycles_delta, "unused_symbols": reg.unused_symbols_delta() })),
        "base_sha": comparison.map(|reg| &reg.base_sha),
        "target_sha": comparison.map(|reg| &reg.target_sha),
    })
}

pub(crate) fn loctree_warning_context(heuristics: Option<&HeuristicsResult>) -> String {
    let detail = match heuristics.and_then(|h| h.regression.as_ref()) {
        Some(reg) => format!(
            "net count deltas: dead exports {:+}, cycles {:+}, unused symbols {:+}; finding identities not compared",
            reg.dead_exports_delta,
            reg.cycles_delta,
            reg.unused_symbols_delta()
        ),
        None => "baseline count comparison unavailable; finding identities not compared".into(),
    };
    format!("whole-repository structural signal; {detail}")
}

/// Enrich the existing warning instead of adding a new decision signal or
/// treating zero net counts as proof that all findings predate the change.
pub(crate) fn explain_loctree_warnings(
    caveats: &mut [String],
    heuristics: Option<&HeuristicsResult>,
) {
    for caveat in caveats {
        if caveat == "heuristics_loctree returned warnings" {
            *caveat = format!("{caveat} ({})", loctree_warning_context(heuristics));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dogfood_loctree_equal_counts_do_not_claim_identical_findings_or_pass() {
        let mut h = HeuristicsResult::default();
        let base = HeuristicsResult::default();
        h.regression = Some(crate::heuristics::compute_delta(
            &base, &h, "base", "target",
        ));
        let mut caveats = vec![
            "heuristics_loctree returned warnings".into(),
            "other warning".into(),
        ];
        explain_loctree_warnings(&mut caveats, Some(&h));
        assert_eq!(caveats.len(), 2);
        assert!(caveats[0].contains("whole-repository"));
        assert!(caveats[0].contains("finding identities not compared"));
        assert_eq!(caveats[1], "other warning");
        assert_eq!(loctree_evidence(Some(&h))["net_counts_unchanged"], true);
        assert_eq!(
            loctree_evidence(Some(&h))["finding_identities_compared"],
            false
        );
        assert!(loctree_evidence(None)["net_counts_unchanged"].is_null());
        assert!(loctree_warning_context(None).contains("unavailable"));
        h.regression.as_mut().unwrap().dead_exports_delta = 1;
        assert_eq!(loctree_evidence(Some(&h))["net_counts_unchanged"], false);
        assert!(loctree_warning_context(Some(&h)).contains("dead exports +1"));
    }
}
