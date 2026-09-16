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

impl ConsistencyReport {
    /// Fold the provenance cross-check into this report.
    ///
    /// A contradiction between what the run says about its substrate and what a
    /// check says about the tree it read is a consistency failure like any
    /// counter mismatch: the pack cannot be published as `consistent` while two
    /// of its own statements describe different trees.
    pub fn merge_provenance(&mut self, provenance: &ProvenanceConsistency) {
        self.checked_fields += provenance.comparisons;
        for contradiction in &provenance.contradictions {
            self.warnings.push(ConsistencyWarning {
                field: contradiction.field.to_string(),
                sources: vec![
                    ConsistencySource {
                        artifact: "PROVENANCE.json (run)".to_string(),
                        value: contradiction.run_value.clone(),
                    },
                    ConsistencySource {
                        artifact: format!("PROVENANCE.json (check {})", contradiction.check_id),
                        value: contradiction.check_value.clone(),
                    },
                ],
                message: format!("{}: {}", contradiction.code, contradiction.explanation),
            });
        }
        self.consistent = self.warnings.is_empty();
    }
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

/// The code every provenance contradiction carries, so a reader greps one
/// token across PROVENANCE.json, CONSISTENCY_CHECK.json and MERGE_GATE.json.
pub const PROVENANCE_CONTRADICTION_CODE: &str = "PROVENANCE_CONTRADICTION";

/// Which pair of statements disagree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProvenanceContradictionKind {
    /// The run froze the operator working tree as clean (or dirty) before the
    /// checks ran, and a check that read THAT SAME tree recorded the opposite.
    OperatorWorktreeState,
    /// A check scanned a commit that is not the commit the pack judges.
    CheckTargetSha,
    /// A check ran in a checkout that is not this repository at all, so its
    /// evidence belongs to a substrate the pack never declared.
    ForeignSubstrate,
}

/// One named disagreement between the run's substrate and a check's substrate.
///
/// Both sides are carried verbatim: a reader must be able to see WHICH two
/// statements cannot both be true without re-deriving them from the pack.
#[derive(Debug, Clone, Serialize)]
pub struct ProvenanceContradiction {
    pub code: &'static str,
    pub kind: ProvenanceContradictionKind,
    pub check_id: String,
    /// The run-level field the check row contradicts.
    pub field: &'static str,
    pub run_value: String,
    pub check_value: String,
    pub explanation: String,
}

/// Outcome of the provenance cross-check: what was compared, and what did not
/// add up. `comparisons` is reported even when nothing is wrong — "checked and
/// agreed" and "never checked" are different results (class 19).
#[derive(Debug, Clone, Default, Serialize)]
pub struct ProvenanceConsistency {
    pub comparisons: usize,
    pub contradictions: Vec<ProvenanceContradiction>,
}

impl ProvenanceConsistency {
    pub fn is_empty(&self) -> bool {
        self.contradictions.is_empty()
    }

    /// The review signals these contradictions contribute, rendered ONCE here.
    ///
    /// Every surface that publishes review caveats reads this list:
    /// `MERGE_GATE.json`'s `decision.review_caveats`, `report.json`'s
    /// `gate.review_caveats`, and — through the dashboard context both the
    /// dashboard HTML and its "Copy PR comment" projection sit on — the
    /// operator-facing summary. The format is the contract
    /// (`docs/contracts/merge_gate.md`): `<code>: <explanation>`, one entry per
    /// row, which `tools/validate_merge_gate.py` matches against the typed rows
    /// as a multiset.
    ///
    /// It is a method rather than a copied `format!` at each consumer because
    /// that copy is exactly how the surfaces drifted: the gate named a
    /// contradiction that report.json and the PR comment did not.
    pub fn review_caveats(&self) -> Vec<String> {
        self.contradictions
            .iter()
            .map(|contradiction| format!("{}: {}", contradiction.code, contradiction.explanation))
            .collect()
    }
}

/// The run-level substrate every check row is held against.
#[derive(Debug, Clone, Copy)]
pub struct RunProvenance<'a> {
    /// The commit whose tree the pack judges (`PROVENANCE.json.target_sha`).
    pub target_sha: &'a str,
    /// Operator working-tree cleanliness frozen before the checks ran
    /// (`PROVENANCE.json.operator_worktree.clean`). `None` is unknown and is
    /// never compared — an absent observation contradicts nothing.
    pub operator_worktree_clean: Option<bool>,
}

/// Two commit ids name the same commit when one abbreviates the other.
///
/// Provenance rows are written by different producers (a `git2` object id, a
/// recorded analysis sha, a resolved ref), and an abbreviation is not a
/// disagreement. Seven hex characters is git's own floor for an abbreviated id,
/// so anything shorter is treated as no evidence rather than a match.
fn commit_ids_agree(a: &str, b: &str) -> bool {
    let (a, b) = (a.trim(), b.trim());
    if a.is_empty() || b.is_empty() {
        return true;
    }
    let (short, long) = if a.len() <= b.len() { (a, b) } else { (b, a) };
    if short.len() < 7 {
        return true;
    }
    long.get(..short.len())
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case(short))
}

/// Cross-check every check's recorded substrate against the run's own.
///
/// PROVENANCE.json states the substrate twice — once for the pack, once per
/// check — and before this checker the two could disagree in the same file
/// without anything noticing (class 10/11). Three disagreements are provable
/// from the rows alone, and each one means the evidence may not describe the
/// reviewed commit:
///
/// 1. the operator tree is frozen clean while a check that read that live tree
///    recorded `local-dirty` (or the reverse);
/// 2. a check scanned a commit other than the reviewed target;
/// 3. a check ran in a checkout that is not this repository.
///
/// Rows replayed from cache are excluded from all three: their provenance
/// describes the ORIGINAL execution's tree, so holding it against THIS run's
/// substrate would claim a contradiction where there is only a cache hit
/// (`CheckResult::cached` is what marks it, and stale replays are already named
/// by the gate's stale-cache caveats). A check with no provenance at all is not
/// compared either — silence is not a contradiction, it is an evidence gap the
/// PROVENANCE rows already show as nulls.
pub fn detect_provenance_contradictions(
    run: RunProvenance<'_>,
    checks: &[crate::checks::CheckResult],
) -> ProvenanceConsistency {
    use crate::checks::TreeState;

    let mut out = ProvenanceConsistency::default();

    for check in checks {
        if check.cached {
            continue;
        }
        let Some(prov) = check.provenance.as_ref() else {
            continue;
        };
        let check_id = crate::check_id::check_id_from_name(&check.name);

        // A foreign tree's HEAD belongs to another project, so its `target_sha`
        // mismatch is the same single fact as its identity — reported once.
        if prov.tree_state == Some(TreeState::Foreign) {
            out.comparisons += 1;
            out.contradictions.push(ProvenanceContradiction {
                code: PROVENANCE_CONTRADICTION_CODE,
                kind: ProvenanceContradictionKind::ForeignSubstrate,
                check_id: check_id.clone(),
                field: "checks[].cwd",
                run_value: format!("this repository at {}", run.target_sha),
                check_value: prov.cwd.clone(),
                explanation: format!(
                    "Check `{check_id}` ran in `{}`, a checkout that is not this repository, so its \
                     evidence cannot be attributed to the declared substrate of {}.",
                    prov.cwd, run.target_sha
                ),
            });
            continue;
        }

        if let (Some(run_clean), Some(state)) = (run.operator_worktree_clean, prov.tree_state) {
            let check_local_clean = match state {
                TreeState::LocalClean => Some(true),
                TreeState::LocalDirty => Some(false),
                // Snapshot states describe an ephemeral worktree, not the
                // operator's checkout; they cannot contradict its cleanliness.
                _ => None,
            };
            if let Some(check_clean) = check_local_clean {
                out.comparisons += 1;
                if check_clean != run_clean {
                    out.contradictions.push(ProvenanceContradiction {
                        code: PROVENANCE_CONTRADICTION_CODE,
                        kind: ProvenanceContradictionKind::OperatorWorktreeState,
                        check_id: check_id.clone(),
                        field: "operator_worktree.clean",
                        run_value: if run_clean {
                            "clean".to_string()
                        } else {
                            "dirty".to_string()
                        },
                        check_value: state.as_str().to_string(),
                        explanation: format!(
                            "The run froze the operator working tree as {}, but check `{check_id}` \
                             recorded `{}` for that same tree, so the pack states two different \
                             states for one substrate.",
                            if run_clean { "clean" } else { "dirty" },
                            state.as_str()
                        ),
                    });
                }
            }
        }

        if let Some(check_sha) = prov.target_sha.as_deref() {
            out.comparisons += 1;
            if !commit_ids_agree(check_sha, run.target_sha) {
                out.contradictions.push(ProvenanceContradiction {
                    code: PROVENANCE_CONTRADICTION_CODE,
                    kind: ProvenanceContradictionKind::CheckTargetSha,
                    check_id: check_id.clone(),
                    field: "target_sha",
                    run_value: run.target_sha.to_string(),
                    check_value: check_sha.to_string(),
                    explanation: format!(
                        "Check `{check_id}` scanned commit {check_sha}, which is not the reviewed \
                         target {}, so its result does not describe the tree this pack judges.",
                        run.target_sha
                    ),
                });
            }
        }
    }

    out
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

    // ── provenance contradictions (truth hardening, classes 10/11) ──

    const RUN_TARGET: &str = "abc1234abc1234abc1234abc1234abc1234ab12";

    fn checked_with(
        name: &str,
        cwd: &str,
        target_sha: Option<&str>,
        tree_state: Option<crate::checks::TreeState>,
    ) -> crate::checks::CheckResult {
        crate::checks::CheckResult {
            name: name.to_string(),
            status: crate::checks::CheckStatus::Passed,
            duration: std::time::Duration::from_secs(1),
            output: String::new(),
            cached: false,
            provenance: Some(crate::checks::CheckProvenance {
                command: format!("{name} --check"),
                tool_version: None,
                cwd: cwd.to_string(),
                target_sha: target_sha.map(str::to_string),
                tree_state,
                exit_code: Some(0),
                started_at: "2026-09-11T10:00:00+02:00".to_string(),
                finished_at: "2026-09-11T10:00:01+02:00".to_string(),
                hard_fail_signatures: Vec::new(),
                cache_key: None,
                executed_scope: None,
            }),
        }
    }

    fn run_at(clean: Option<bool>) -> RunProvenance<'static> {
        RunProvenance {
            target_sha: RUN_TARGET,
            operator_worktree_clean: clean,
        }
    }

    #[test]
    fn agreeing_provenance_reports_the_comparisons_and_no_contradiction() {
        use crate::checks::TreeState;
        let checks = [
            checked_with(
                "Cargo clippy",
                "/tmp/snapshot",
                Some(RUN_TARGET),
                Some(TreeState::Snapshot),
            ),
            // An abbreviated id is the same commit, not a second one.
            checked_with(
                "Cargo fmt",
                "/repo",
                Some(&RUN_TARGET[..12]),
                Some(TreeState::LocalClean),
            ),
        ];

        let result = detect_provenance_contradictions(run_at(Some(true)), &checks);

        assert!(result.is_empty(), "{:?}", result.contradictions);
        assert_eq!(
            result.comparisons, 3,
            "two target shas plus one local tree state were actually compared"
        );
    }

    #[test]
    fn a_clean_operator_tree_contradicts_a_check_that_read_it_dirty() {
        use crate::checks::TreeState;
        let checks = [checked_with(
            "Cargo clippy",
            "/repo",
            Some(RUN_TARGET),
            Some(TreeState::LocalDirty),
        )];

        let result = detect_provenance_contradictions(run_at(Some(true)), &checks);

        assert_eq!(result.contradictions.len(), 1);
        let contradiction = &result.contradictions[0];
        assert_eq!(contradiction.code, PROVENANCE_CONTRADICTION_CODE);
        assert_eq!(
            contradiction.kind,
            ProvenanceContradictionKind::OperatorWorktreeState
        );
        assert_eq!(contradiction.check_id, "cargo_clippy");
        assert_eq!(contradiction.run_value, "clean");
        assert_eq!(contradiction.check_value, "local-dirty");
        assert!(contradiction.explanation.contains("cargo_clippy"));
    }

    #[test]
    fn a_check_scanning_another_commit_contradicts_the_reviewed_target() {
        use crate::checks::TreeState;
        let other = "9999999999999999999999999999999999999999";
        let checks = [checked_with(
            "Cargo test",
            "/tmp/snapshot",
            Some(other),
            Some(TreeState::Snapshot),
        )];

        let result = detect_provenance_contradictions(run_at(Some(true)), &checks);

        assert_eq!(result.contradictions.len(), 1);
        let contradiction = &result.contradictions[0];
        assert_eq!(
            contradiction.kind,
            ProvenanceContradictionKind::CheckTargetSha
        );
        assert_eq!(contradiction.run_value, RUN_TARGET);
        assert_eq!(contradiction.check_value, other);
    }

    #[test]
    fn a_foreign_checkout_contradicts_the_declared_substrate_once() {
        use crate::checks::TreeState;
        let checks = [checked_with(
            "Cargo check",
            "/elsewhere/other-repo",
            // A foreign checkout's HEAD is another project's commit; the row
            // must be named once as a foreign substrate, not twice.
            Some("5555555555555555555555555555555555555555"),
            Some(TreeState::Foreign),
        )];

        let result = detect_provenance_contradictions(run_at(Some(true)), &checks);

        assert_eq!(result.contradictions.len(), 1);
        let contradiction = &result.contradictions[0];
        assert_eq!(
            contradiction.kind,
            ProvenanceContradictionKind::ForeignSubstrate
        );
        assert_eq!(contradiction.check_value, "/elsewhere/other-repo");
    }

    #[test]
    fn a_cache_replay_and_an_unknown_operator_state_are_not_contradictions() {
        use crate::checks::TreeState;
        let mut replayed = checked_with(
            "Cargo clippy",
            "/repo",
            Some("5555555555555555555555555555555555555555"),
            Some(TreeState::LocalDirty),
        );
        replayed.cached = true;
        let unknown_operator_state = checked_with(
            "Cargo fmt",
            "/repo",
            Some(RUN_TARGET),
            Some(TreeState::LocalDirty),
        );
        let no_provenance = crate::checks::CheckResult {
            provenance: None,
            ..checked_with("Cargo test", "/repo", None, None)
        };

        let result = detect_provenance_contradictions(
            run_at(None),
            &[replayed, unknown_operator_state, no_provenance],
        );

        assert!(result.is_empty(), "{:?}", result.contradictions);
        assert_eq!(
            result.comparisons, 1,
            "only the fresh row's target sha could be compared"
        );
    }

    #[test]
    fn a_contradiction_makes_the_consistency_report_inconsistent() {
        use crate::checks::TreeState;
        let checks = [checked_with(
            "Cargo clippy",
            "/repo",
            Some(RUN_TARGET),
            Some(TreeState::LocalDirty),
        )];
        let provenance = detect_provenance_contradictions(run_at(Some(true)), &checks);

        let mut report = ArtifactCounters {
            files_changed_diff: Some(3),
            files_changed_report: Some(3),
            ..Default::default()
        }
        .check_consistency();
        assert!(report.consistent);
        let counter_fields = report.checked_fields;

        report.merge_provenance(&provenance);

        assert!(!report.consistent);
        assert_eq!(
            report.checked_fields,
            counter_fields + provenance.comparisons
        );
        let warning = report
            .warnings
            .iter()
            .find(|warning| warning.field == "operator_worktree.clean")
            .expect("the contradiction must surface as a consistency warning");
        assert!(warning.message.starts_with(PROVENANCE_CONTRADICTION_CODE));
        assert_eq!(warning.sources.len(), 2);
    }
}
