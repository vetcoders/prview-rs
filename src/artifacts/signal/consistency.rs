//! Cross-artifact consistency checker.
//!
//! Compares key counters between MERGE_GATE, report.json, coverage, breaking
//! changes, and inline findings to detect mismatches that would erode trust.

use serde::Serialize;
use std::path::Path;

/// Counters recovered from the ALREADY-SERIALIZED artifacts on disk.
///
/// This is the independent side of the cross-check. MERGE_GATE.json computes its
/// verdict via `derive_decision(...)` and report.json serializes the dashboard
/// context's verdict — both from the same inputs, so a mismatch means a real
/// divergence (the b1697d4 vocabulary-drift class the old self-comparing checker
/// could never see). A missing or unparseable artifact yields `None` for that
/// source, so the pair is simply not checked — never faked into a green result.
#[derive(Debug, Default)]
pub struct DiskArtifactCounters {
    pub verdict_gate: Option<String>,
    pub findings_count_gate: Option<usize>,
    pub findings_count_sarif: Option<usize>,
    pub verdict_report: Option<String>,
    pub files_changed_report: Option<usize>,
    pub findings_count_report: Option<usize>,
    pub commit_count_report: Option<usize>,
}

/// Read the serialized cross-artifact counters under `pack_root` (the pack
/// out_dir): MERGE_GATE.json (`00_summary/`), INLINE_FINDINGS.sarif
/// (`30_context/`, absent when there are zero findings), and report.json (pack
/// root, absent while it is still being built). Absent/unparseable → `None`.
pub fn read_disk_artifact_counters(pack_root: &Path) -> DiskArtifactCounters {
    fn load_json(path: std::path::PathBuf) -> Option<serde_json::Value> {
        let text = std::fs::read_to_string(path).ok()?;
        serde_json::from_str(&text).ok()
    }
    fn usize_at(value: &serde_json::Value, pointer: &str) -> Option<usize> {
        value
            .pointer(pointer)
            .and_then(serde_json::Value::as_u64)
            .map(|n| n as usize)
    }
    fn string_at(value: &serde_json::Value, pointer: &str) -> Option<String> {
        value
            .pointer(pointer)
            .and_then(|v| v.as_str())
            .map(str::to_string)
    }

    let mut out = DiskArtifactCounters::default();

    if let Some(gate) = load_json(pack_root.join("00_summary").join("MERGE_GATE.json")) {
        out.verdict_gate = string_at(&gate, "/decision/verdict");
        out.findings_count_gate = usize_at(&gate, "/inline_findings/findings_count");
    }

    if let Some(sarif) = load_json(pack_root.join("30_context").join("INLINE_FINDINGS.sarif")) {
        out.findings_count_sarif = sarif
            .pointer("/runs/0/results")
            .and_then(|r| r.as_array())
            .map(Vec::len);
    }

    if let Some(report) = load_json(pack_root.join("report.json")) {
        out.verdict_report = string_at(&report, "/gate/verdict");
        out.files_changed_report = usize_at(&report, "/diff/stats/files_changed");
        out.findings_count_report = usize_at(&report, "/quality/sarif/findings_count");
        out.commit_count_report = usize_at(&report, "/diff/stats/commits");
    }

    out
}

#[derive(Debug, Clone, Serialize)]
pub struct ConsistencyWarning {
    pub field: String,
    pub sources: Vec<ConsistencySource>,
    pub message: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ConsistencySource {
    pub artifact: String,
    pub value: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ConsistencyReport {
    pub warnings: Vec<ConsistencyWarning>,
    pub checked_fields: usize,
    pub consistent: bool,
}

/// Snapshot of key counters collected from different artifact surfaces.
#[derive(Debug, Default)]
pub struct ArtifactCounters {
    pub files_changed_diff: Option<usize>,
    pub files_changed_report: Option<usize>,
    pub findings_count_sarif: Option<usize>,
    pub findings_count_gate: Option<usize>,
    pub findings_count_report: Option<usize>,
    pub breaking_count_signal: Option<usize>,
    pub breaking_count_report: Option<usize>,
    pub skipped_checks_gate: Option<usize>,
    pub skipped_checks_report: Option<usize>,
    pub commit_count_diff: Option<usize>,
    pub commit_count_report: Option<usize>,
    pub coverage_pct_signal: Option<u32>,
    pub coverage_pct_report: Option<u32>,
    pub verdict_gate: Option<String>,
    pub verdict_report: Option<String>,
}

impl ArtifactCounters {
    pub fn check_consistency(&self) -> ConsistencyReport {
        let mut warnings = Vec::new();
        let mut checked = 0usize;

        checked += check_pair(
            &mut warnings,
            "files_changed",
            self.files_changed_diff,
            "diff",
            self.files_changed_report,
            "report.json",
        );

        // INLINE_FINDINGS.sarif is omitted entirely for zero-finding runs, so an
        // absent SARIF count is a legitimate 0 — but only trust that once the
        // counterpart artifact exists (i.e. the run got far enough to serialize
        // it). Defaulting the SARIF side to 0 in that case turns a "gate/report
        // reports N findings while SARIF is absent" state into a caught mismatch
        // instead of a silently skipped pair (strengthens the b1697d4 guard).
        let sarif_when = |counterpart: Option<usize>| {
            counterpart.map(|_| self.findings_count_sarif.unwrap_or(0))
        };

        checked += check_pair(
            &mut warnings,
            "findings_count",
            sarif_when(self.findings_count_gate),
            "SARIF",
            self.findings_count_gate,
            "MERGE_GATE",
        );

        checked += check_pair(
            &mut warnings,
            "findings_count",
            sarif_when(self.findings_count_report),
            "SARIF",
            self.findings_count_report,
            "report.json",
        );

        checked += check_pair(
            &mut warnings,
            "breaking_changes",
            self.breaking_count_signal,
            "signal",
            self.breaking_count_report,
            "report.json",
        );

        checked += check_pair(
            &mut warnings,
            "skipped_checks",
            self.skipped_checks_gate,
            "MERGE_GATE",
            self.skipped_checks_report,
            "report.json",
        );

        checked += check_pair(
            &mut warnings,
            "commit_count",
            self.commit_count_diff,
            "diff",
            self.commit_count_report,
            "report.json",
        );

        checked += check_pair(
            &mut warnings,
            "coverage_pct",
            self.coverage_pct_signal,
            "signal",
            self.coverage_pct_report,
            "report.json",
        );

        if let (Some(a), Some(b)) = (&self.verdict_gate, &self.verdict_report) {
            checked += 1;
            if a != b {
                warnings.push(ConsistencyWarning {
                    field: "verdict".to_string(),
                    sources: vec![
                        ConsistencySource {
                            artifact: "MERGE_GATE".to_string(),
                            value: a.clone(),
                        },
                        ConsistencySource {
                            artifact: "report.json".to_string(),
                            value: b.clone(),
                        },
                    ],
                    message: format!(
                        "Verdict mismatch: MERGE_GATE says '{}', report.json says '{}'",
                        a, b
                    ),
                });
            }
        }

        let consistent = warnings.is_empty();
        ConsistencyReport {
            warnings,
            checked_fields: checked,
            consistent,
        }
    }
}

fn check_pair<T: PartialEq + std::fmt::Display>(
    warnings: &mut Vec<ConsistencyWarning>,
    field: &str,
    a_val: Option<T>,
    a_name: &str,
    b_val: Option<T>,
    b_name: &str,
) -> usize {
    match (a_val, b_val) {
        (Some(a), Some(b)) => {
            if a != b {
                warnings.push(ConsistencyWarning {
                    field: field.to_string(),
                    sources: vec![
                        ConsistencySource {
                            artifact: a_name.to_string(),
                            value: a.to_string(),
                        },
                        ConsistencySource {
                            artifact: b_name.to_string(),
                            value: b.to_string(),
                        },
                    ],
                    message: format!(
                        "{} mismatch: {} reports {}, {} reports {}",
                        field, a_name, a, b_name, b
                    ),
                });
            }
            1
        }
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn consistent_counters_produce_no_warnings() {
        let counters = ArtifactCounters {
            files_changed_diff: Some(5),
            files_changed_report: Some(5),
            findings_count_sarif: Some(3),
            findings_count_gate: Some(3),
            findings_count_report: Some(3),
            breaking_count_signal: Some(1),
            breaking_count_report: Some(1),
            ..Default::default()
        };
        let report = counters.check_consistency();
        assert!(report.consistent);
        assert!(report.warnings.is_empty());
        assert!(report.checked_fields >= 4);
    }

    #[test]
    fn mismatched_findings_count_produces_warning() {
        let counters = ArtifactCounters {
            findings_count_sarif: Some(5),
            findings_count_gate: Some(3),
            ..Default::default()
        };
        let report = counters.check_consistency();
        assert!(!report.consistent);
        assert_eq!(report.warnings.len(), 1);
        assert!(report.warnings[0].message.contains("findings_count"));
        assert!(report.warnings[0].message.contains("5"));
        assert!(report.warnings[0].message.contains("3"));
    }

    #[test]
    fn absent_sarif_reads_as_zero_and_catches_nonzero_gate() {
        // Regression (PR #13): SARIF is omitted for zero-finding runs, so
        // findings_count_sarif is None. When the gate nonetheless reports
        // findings, the pair must be compared (SARIF=0 vs gate=N) and flagged,
        // not silently skipped.
        let counters = ArtifactCounters {
            findings_count_sarif: None,
            findings_count_gate: Some(2),
            ..Default::default()
        };
        let report = counters.check_consistency();
        assert!(!report.consistent);
        assert!(
            report
                .warnings
                .iter()
                .any(|w| w.field == "findings_count" && w.message.contains('2')),
            "gate=2 with absent SARIF must be flagged, got: {:?}",
            report.warnings
        );
    }

    #[test]
    fn absent_sarif_with_zero_gate_stays_consistent() {
        // The other side of the same rule: absent SARIF genuinely means 0, so a
        // gate that also reports 0 must NOT produce a false mismatch.
        let counters = ArtifactCounters {
            findings_count_sarif: None,
            findings_count_gate: Some(0),
            findings_count_report: Some(0),
            ..Default::default()
        };
        let report = counters.check_consistency();
        assert!(report.consistent, "0 vs absent-SARIF is consistent");
        assert!(
            report.checked_fields >= 2,
            "the pairs must still be checked"
        );
    }

    #[test]
    fn absent_sarif_and_absent_counterpart_stays_unchecked() {
        // When neither side has data we must not fabricate a 0-vs-0 comparison.
        let counters = ArtifactCounters {
            findings_count_sarif: None,
            findings_count_gate: None,
            findings_count_report: None,
            ..Default::default()
        };
        let report = counters.check_consistency();
        assert!(report.consistent);
        assert_eq!(report.checked_fields, 0);
    }

    #[test]
    fn verdict_mismatch_detected() {
        let counters = ArtifactCounters {
            verdict_gate: Some("PASS".to_string()),
            verdict_report: Some("CONDITIONAL".to_string()),
            ..Default::default()
        };
        let report = counters.check_consistency();
        assert!(!report.consistent);
        assert!(report.warnings[0].field == "verdict");
    }

    #[test]
    fn missing_values_are_not_checked() {
        let counters = ArtifactCounters::default();
        let report = counters.check_consistency();
        assert!(report.consistent);
        assert_eq!(report.checked_fields, 0);
    }

    #[test]
    fn disk_counters_read_serialized_artifacts() {
        use std::fs;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("00_summary")).unwrap();
        fs::create_dir_all(root.join("30_context")).unwrap();
        fs::write(
            root.join("00_summary/MERGE_GATE.json"),
            r#"{"decision":{"verdict":"BLOCK"},"inline_findings":{"findings_count":4}}"#,
        )
        .unwrap();
        fs::write(
            root.join("30_context/INLINE_FINDINGS.sarif"),
            r#"{"runs":[{"results":[{},{},{},{}]}]}"#,
        )
        .unwrap();
        fs::write(
            root.join("report.json"),
            r#"{"gate":{"verdict":"BLOCK"},"diff":{"stats":{"files_changed":7,"commits":3}},"quality":{"sarif":{"findings_count":4}}}"#,
        )
        .unwrap();

        let disk = read_disk_artifact_counters(root);
        assert_eq!(disk.verdict_gate.as_deref(), Some("BLOCK"));
        assert_eq!(disk.findings_count_gate, Some(4));
        assert_eq!(disk.findings_count_sarif, Some(4));
        assert_eq!(disk.verdict_report.as_deref(), Some("BLOCK"));
        assert_eq!(disk.files_changed_report, Some(7));
        assert_eq!(disk.commit_count_report, Some(3));
        assert_eq!(disk.findings_count_report, Some(4));
    }

    #[test]
    fn disk_counters_missing_artifacts_are_none_not_faked() {
        let dir = tempfile::tempdir().unwrap();
        let disk = read_disk_artifact_counters(dir.path());
        assert!(disk.verdict_gate.is_none());
        assert!(disk.findings_count_sarif.is_none());
        assert!(disk.verdict_report.is_none());
    }

    #[test]
    fn cross_artifact_verdict_divergence_is_caught_from_disk() {
        // Regression: the checker must catch a real divergence between the
        // SERIALIZED MERGE_GATE verdict and the SERIALIZED report verdict (the
        // b1697d4 class). The old checker compared ctx.verdict against itself and
        // could never see it — a tautology that was always "consistent".
        use std::fs;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("00_summary")).unwrap();
        fs::write(
            root.join("00_summary/MERGE_GATE.json"),
            r#"{"decision":{"verdict":"BLOCK"}}"#,
        )
        .unwrap();
        fs::write(
            root.join("report.json"),
            r#"{"gate":{"verdict":"PASS"},"diff":{"stats":{"files_changed":1,"commits":1}}}"#,
        )
        .unwrap();

        let disk = read_disk_artifact_counters(root);
        let counters = ArtifactCounters {
            verdict_gate: disk.verdict_gate,
            verdict_report: disk.verdict_report,
            ..Default::default()
        };
        let report = counters.check_consistency();
        assert!(
            !report.consistent,
            "diverging serialized verdicts must produce a warning"
        );
        assert!(report.warnings.iter().any(|w| w.field == "verdict"));
    }

    #[test]
    fn multiple_mismatches_all_reported() {
        let counters = ArtifactCounters {
            files_changed_diff: Some(10),
            files_changed_report: Some(8),
            findings_count_sarif: Some(5),
            findings_count_gate: Some(5),
            findings_count_report: Some(3),
            ..Default::default()
        };
        let report = counters.check_consistency();
        assert!(!report.consistent);
        // files_changed mismatch + findings SARIF vs report mismatch
        assert!(report.warnings.len() >= 2);
    }
}
