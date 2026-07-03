//! Post-generation sanity checks over the artifact pack.

use super::*;

/// Result of post-generation sanity checks
pub(super) struct SanityResult {
    pub(super) valid: bool,
    pub(super) checks_run: usize,
    pub(super) checks_passed: usize,
    pub(super) failures: Vec<String>,
}

/// Run sanity checks on a completed artifact pack.
/// Called AFTER generate_manifest() to verify pack integrity.
pub(super) fn run_sanity_checks(out_dir: &Path) -> Result<SanityResult> {
    use sha2::{Digest, Sha256};

    let summary_dir = out_dir.join("00_summary");
    let quality_dir = out_dir.join("20_quality");
    let mut failures = Vec::new();
    // Per-check tracking: (name, passed)
    let mut check_results: Vec<(&'static str, bool)> = Vec::new();

    // 1. Missing required files
    let pre_failures = failures.len();
    let required = [
        "00_summary/RUN.json",
        "00_summary/MANIFEST.json",
        "00_summary/system_meta.txt",
        "00_summary/git_meta.txt",
    ];
    for req in &required {
        if !out_dir.join(req).exists() {
            failures.push(format!("missing required file: {}", req));
        }
    }
    check_results.push(("required_files", failures.len() == pre_failures));

    // 2. MANIFEST hash verification (sample up to 5 files)
    let pre_failures = failures.len();
    let manifest_path = summary_dir.join("MANIFEST.json");
    if manifest_path.exists()
        && let Ok(manifest_text) = fs::read_to_string(&manifest_path)
        && let Ok(manifest) = serde_json::from_str::<serde_json::Value>(&manifest_text)
        && let Some(files) = manifest["files"].as_array()
    {
        let sample_size = files.len().min(5);
        for entry in files.iter().take(sample_size) {
            if let (Some(rel_path), Some(expected_hash)) =
                (entry["path"].as_str(), entry["sha256"].as_str())
                && let Ok(content) = read_within(out_dir, Path::new(rel_path))
            {
                let mut hasher = Sha256::new();
                hasher.update(&content);
                let actual_hash = format!("{:x}", hasher.finalize());
                if actual_hash != expected_hash {
                    failures.push(format!(
                        "MANIFEST hash mismatch for {}: expected {}, got {}",
                        rel_path,
                        &expected_hash[..8],
                        &actual_hash[..8]
                    ));
                }
            }
        }
    }
    check_results.push(("manifest_hashes", failures.len() == pre_failures));

    // 3. Gate status vs output contradiction
    let pre_failures = failures.len();
    if quality_dir.exists()
        && let Ok(entries) = read_dir_within(out_dir, Path::new("20_quality"))
    {
        for entry in entries.filter_map(|e| e.ok()) {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if !name_str.ends_with(".result.json") {
                continue;
            }
            let gate_id = name_str.trim_end_matches(".result.json");
            // Pass a `quality_dir`-relative name (not the absolute `entry.path()`
            // / a re-prefixed join): read_to_string_within joins `requested` onto
            // the canonical root, so an absolute or already-prefixed path only
            // resolves by luck under an absolute out_dir and double-prefixes under
            // a relative one (`prview -o ./rel`) — silently skipping the read and
            // making this consistency check fail-open.
            if let Ok(result_text) = read_to_string_within(&quality_dir, Path::new(&name))
                && let Ok(result) = serde_json::from_str::<serde_json::Value>(&result_text)
                && result["status"].as_str() == Some("passed")
            {
                let log_name = format!("{}.log", gate_id);
                if let Ok(log_content) = read_to_string_within(&quality_dir, Path::new(&log_name)) {
                    let sigs = crate::checks::find_hard_fail_signatures(&log_content);
                    if !sigs.is_empty() {
                        failures.push(format!(
                            "gate {} status is 'passed' but log contains hard fail signatures: {}",
                            gate_id,
                            sigs.join(", ")
                        ));
                    }
                }
            }
        }
    }
    check_results.push(("gate_status_consistency", failures.len() == pre_failures));

    // 4. RUN.json gate count vs .result.json file count
    let pre_failures = failures.len();
    let run_path = summary_dir.join("RUN.json");
    if run_path.exists()
        && quality_dir.exists()
        && let Ok(run_text) = fs::read_to_string(&run_path)
        && let Ok(run_json) = serde_json::from_str::<serde_json::Value>(&run_text)
        && let Some(checks_arr) = run_json["checks"].as_array()
    {
        let run_count = checks_arr.len();
        let result_count = read_dir_within(out_dir, Path::new("20_quality"))
            .map(|entries| {
                entries
                    .filter_map(|e| e.ok())
                    .filter(|e| e.file_name().to_string_lossy().ends_with(".result.json"))
                    .count()
            })
            .unwrap_or(0);
        // RUN.json includes all checks (real + synthetic heuristics).
        // 20_quality/ has matching .result.json files for each.
        // Flag only when result_count < run_count (missing gate files).
        if result_count < run_count {
            failures.push(format!(
                "RUN.json has {} checks but only {} .result.json files in 20_quality/",
                run_count, result_count
            ));
        }
    }
    check_results.push(("gate_result_count", failures.len() == pre_failures));

    // 5. Timings claim outputs that must exist on disk.
    // RUN.json records, per context command, the artifact it produced and a
    // status; and per context artifact, whether it was generated. A completed
    // command (or a generated artifact) that left no file on disk is the
    // AI_INDEX class of bug: the run claims an output it never wrote.
    let pre_failures = failures.len();
    let run_path = summary_dir.join("RUN.json");
    if run_path.exists()
        && let Ok(run_text) = fs::read_to_string(&run_path)
        && let Ok(run_json) = serde_json::from_str::<serde_json::Value>(&run_text)
    {
        if let Some(commands) = run_json["context_commands"].as_array() {
            for cmd in commands {
                // Only completed commands are expected to have written their
                // artifact; timed-out / errored ones legitimately may not.
                if cmd["status"].as_str() != Some("completed") {
                    continue;
                }
                if let Some(artifact) = cmd["artifact"].as_str()
                    && !out_dir.join(artifact).exists()
                {
                    let label = cmd["label"].as_str().unwrap_or("?");
                    failures.push(format!(
                        "context command '{}' status is 'completed' but its output is missing: {}",
                        label, artifact
                    ));
                }
            }
        }
        if let Some(artifacts) = run_json["context_artifacts"].as_array() {
            for artifact in artifacts {
                if artifact["generated"].as_bool() == Some(true)
                    && let Some(path) = artifact["path"].as_str()
                    && !out_dir.join(path).exists()
                {
                    let key = artifact["key"].as_str().unwrap_or("?");
                    failures.push(format!(
                        "context artifact '{}' marked generated but missing on disk: {}",
                        key, path
                    ));
                }
            }
        }
        if let Some(timings) = run_json["timings"].as_array() {
            // FAILURES_SUMMARY.md is now written on every run (clean runs get a
            // "No blocking check failures" stub), so every declared timing output
            // must exist — no conditional exemption.
            for timing in timings {
                let Some(label) = timing["label"].as_str() else {
                    continue;
                };
                for expected in expected_outputs_for_timing(label) {
                    if !out_dir.join(expected).exists() {
                        failures.push(format!(
                            "timing label '{}' declared missing output: {}",
                            label, expected
                        ));
                    }
                }
            }
        }
    }
    check_results.push(("timings_outputs_present", failures.len() == pre_failures));

    let checks_run = check_results.len();
    let checks_passed = check_results.iter().filter(|(_, passed)| *passed).count();
    let valid = failures.is_empty();

    let checks_array: Vec<serde_json::Value> = check_results
        .iter()
        .map(|(name, passed)| {
            serde_json::json!({
                "name": name,
                "passed": passed,
            })
        })
        .collect();

    // Write SANITY.json
    let sanity = serde_json::json!({
        "valid": valid,
        "checks_run": checks_run,
        "checks_passed": checks_passed,
        "checks": checks_array,
        "failures": failures,
    });
    fs::write(
        summary_dir.join("SANITY.json"),
        serde_json::to_string_pretty(&sanity)?,
    )?;

    Ok(SanityResult {
        valid,
        checks_run,
        checks_passed,
        failures,
    })
}

pub(super) fn expected_outputs_for_timing(label: &str) -> &'static [&'static str] {
    match label {
        "full.patch" => &["10_diff/full.patch"],
        "00_summary" => &["00_summary/system_meta.txt", "00_summary/git_meta.txt"],
        "20_quality" => &[
            "20_quality/full-checks.log",
            "20_quality/coverage-delta.txt",
        ],
        "MERGE_GATE + FAILURES_SUMMARY" => &[
            "00_summary/MERGE_GATE.json",
            "00_summary/FAILURES_SUMMARY.md",
        ],
        "PR_REVIEW + AI_INDEX" => &["PR_REVIEW.md", "AI_INDEX.md"],
        "REVIEW_SUMMARY + review.html + AI_INDEX" => {
            &["REVIEW_SUMMARY.md", "review.html", "AI_INDEX.md"]
        }
        "report.json + dashboard" => &["report.json", "dashboard.html"],
        "report.json" => &["report.json"],
        "artifacts.zip" => &["artifacts.zip"],
        _ => &[],
    }
}
