//! Previous-run deltas, run history and flaky-check scoring.

use super::*;

/// Load coverage ratio and untested count from the previous run's report.json.
///
/// Uses the `latest` symlink to find the previous run. Falls back through:
/// 1. `regression.tests.coverage_ratio` (preferred — our own output)
/// 2. `quality.coverage.heuristic_ratio` (fallback for pre-regression runs)
/// 3. Sorted sibling timestamp dirs if `latest` points to current run
///
/// Returns `(base_coverage_ratio, base_untested_critical_count)`.
/// Completely defensive: any error returns `(None, None)`.
pub(crate) fn load_previous_coverage(out_dir: &Path) -> (Option<f64>, Option<usize>) {
    let parent = match out_dir.parent() {
        Some(p) => p,
        None => return (None, None),
    };

    // Try `latest` symlink first
    let prev_report = find_previous_report(parent, out_dir);
    let json = match prev_report {
        Some(content) => content,
        None => return (None, None),
    };

    // Extract coverage_ratio: regression.tests preferred, quality.coverage fallback
    let coverage_ratio = json
        .pointer("/regression/tests/coverage_ratio")
        .and_then(|v| v.as_f64())
        .or_else(|| {
            json.pointer("/quality/coverage/heuristic_ratio")
                .and_then(|v| v.as_f64())
        });

    // Extract untested count — try code-only first, fallback to critical
    let untested_count = json
        .pointer("/regression/tests/untested_code_count")
        .and_then(|v| v.as_u64())
        .or_else(|| {
            json.pointer("/regression/tests/untested_critical_count")
                .and_then(|v| v.as_u64())
        })
        .map(|v| v as usize);

    (coverage_ratio, untested_count)
}

/// Find and parse report.json from the previous run.
///
/// Strategy:
/// 1. Follow `latest` symlink — if it doesn't point to current run, use it
/// 2. Otherwise, sort sibling timestamp dirs and pick the one before current
pub(crate) fn find_previous_report(
    branch_dir: &Path,
    current_dir: &Path,
) -> Option<serde_json::Value> {
    let latest = branch_dir.join("latest");

    // Try latest symlink first (fallthrough on any error to sort-based fallback)
    if latest.is_symlink()
        && let Ok(resolved) = fs::read_link(&latest)
    {
        let resolved_full = if resolved.is_relative() {
            branch_dir.join(&resolved)
        } else {
            resolved
        };
        if resolved_full != *current_dir
            && let Ok(content) = fs::read_to_string(resolved_full.join("report.json"))
            && let Ok(json) = serde_json::from_str(&content)
        {
            return Some(json);
        }
    }

    // Fallback: sort sibling timestamp dirs, pick the one before current
    let current_name = current_dir.file_name()?.to_str()?;
    let mut dirs: Vec<String> = fs::read_dir(branch_dir)
        .ok()?
        .filter_map(|e| e.ok())
        .filter(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            e.path().is_dir() && name != "latest" && name != current_name
        })
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect();

    dirs.sort();

    // Find the largest timestamp that is less than current
    let prev_name = dirs.iter().rev().find(|d| d.as_str() < current_name)?;
    let prev_dir = branch_dir.join(prev_name);
    let content = fs::read_to_string(prev_dir.join("report.json")).ok()?;
    serde_json::from_str(&content).ok()
}

/// Load delta from a previous run's report.json via the `latest` symlink.
/// Returns `None` on any error — completely defensive.
pub(crate) fn load_previous_run_delta(out_dir: &Path) -> Option<PreviousRunDelta> {
    // out_dir is e.g. ~/.prview/runs/<repo>/<branch>/20260301-120000/
    // latest symlink lives in the parent: ~/.prview/runs/<repo>/<branch>/latest
    let parent = out_dir.parent()?;
    let latest = parent.join("latest");

    // Only follow existing symlinks (not the current run dir)
    if !latest.is_symlink() {
        return None;
    }

    // Resolve the symlink target and ensure it's not pointing to current out_dir
    let resolved = fs::read_link(&latest).ok()?;
    let resolved_full = if resolved.is_relative() {
        parent.join(&resolved)
    } else {
        resolved
    };
    if resolved_full == *out_dir {
        return None;
    }

    let report_path = resolved_full.join("report.json");
    let content = fs::read_to_string(&report_path).ok()?;
    let json: serde_json::Value = serde_json::from_str(&content).ok()?;

    // Extract fields defensively
    let gate = json.get("gate")?;
    let quality_pass_before = gate.get("quality_pass")?.as_bool()?;

    let checks_arr = json.get("checks")?.as_array()?;
    let checks_passed_before = checks_arr
        .iter()
        .filter(|c| c.get("status").and_then(|s| s.as_str()) == Some("PASS"))
        .count();
    let checks_failed_before = checks_arr
        .iter()
        .filter(|c| {
            let s = c.get("status").and_then(|s| s.as_str()).unwrap_or("");
            s == "FAIL" || s == "ERROR"
        })
        .count();

    let findings_before = json
        .get("quality")
        .and_then(|q| q.get("sarif"))
        .and_then(|s| s.get("findings_count"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as usize;

    // PRV-P2-3: Breaking changes delta.
    // report.json schema (v1) stores `quality.breaking_changes.removed_public_symbols_count`
    // and `quality.breaking_changes.new_env_vars_count`. If these fields are missing
    // (e.g. report.json from a version before these fields existed), we fall back to 0.
    // This means the delta may show "+N new breaking changes" even if the previous run
    // had the same count — known limitation for pre-v1 reports.
    let breaking_section = json.get("quality").and_then(|q| q.get("breaking_changes"));
    let removed = breaking_section
        .and_then(|b| b.get("removed_public_symbols_count"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let env = breaking_section
        .and_then(|b| b.get("new_env_vars_count"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let breaking_before = (removed + env) as usize;

    Some(PreviousRunDelta {
        checks_passed_before,
        checks_failed_before,
        findings_before,
        breaking_before,
        quality_pass_before,
    })
}

/// Load historical run data from sibling timestamp directories.
///
/// `out_dir` is e.g. `~/.prview/runs/<repo>/<branch>/20260302-143022/`.
/// We scan the parent (`~/.prview/runs/<repo>/<branch>/`) for all timestamped
/// subdirectories, read each `report.json`, and extract summary metrics.
/// Returns newest-first, capped at `max_runs`. Completely defensive:
/// any parse error or missing file causes that run to be skipped silently.
pub(crate) fn load_run_history(out_dir: &Path, max_runs: usize) -> Vec<HistoricalRun> {
    let parent = match out_dir.parent() {
        Some(p) => p,
        None => return Vec::new(),
    };

    // List all subdirectories (excluding `latest` symlink and current run)
    let entries = match fs::read_dir(parent) {
        Ok(e) => e,
        Err(_) => return Vec::new(),
    };

    let mut dirs: Vec<String> = entries
        .filter_map(|e| e.ok())
        .filter(|e| {
            let name = e.file_name();
            let name_str = name.to_string_lossy();
            // Skip `latest` symlink and non-directories
            name_str != "latest"
                && e.path().is_dir()
                // Must look like a timestamp (starts with digit)
                && name_str.chars().next().map(|c| c.is_ascii_digit()).unwrap_or(false)
        })
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect();

    // Sort lexicographically (timestamps sort chronologically) and take last N
    dirs.sort();
    if dirs.len() > max_runs {
        dirs = dirs.split_off(dirs.len() - max_runs);
    }

    // Parse each report.json, skip on any error
    let mut history: Vec<HistoricalRun> = dirs
        .iter()
        .filter_map(|dir_name| {
            let report_path = parent.join(dir_name).join("report.json");
            let content = fs::read_to_string(&report_path).ok()?;
            let json: serde_json::Value = serde_json::from_str(&content).ok()?;

            let gate = json.get("gate")?;
            let quality_pass = gate.get("quality_pass")?.as_bool()?;

            let checks_arr = json.get("checks")?.as_array()?;
            let checks_passed = checks_arr
                .iter()
                .filter(|c| c.get("status").and_then(|s| s.as_str()) == Some("PASS"))
                .count();
            let checks_failed = checks_arr
                .iter()
                .filter(|c| {
                    let s = c.get("status").and_then(|s| s.as_str()).unwrap_or("");
                    s == "FAIL" || s == "ERROR"
                })
                .count();
            let checks_warned = checks_arr
                .iter()
                .filter(|c| c.get("status").and_then(|s| s.as_str()) == Some("WARN"))
                .count();

            let findings_count = json
                .get("quality")
                .and_then(|q| q.get("sarif"))
                .and_then(|s| s.get("findings_count"))
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as usize;

            Some(HistoricalRun {
                timestamp: dir_name.clone(),
                checks_passed,
                checks_failed,
                checks_warned,
                quality_pass,
                findings_count,
            })
        })
        .collect();

    // Reverse to newest-first
    history.reverse();
    history
}

/// PRV-204: Compute per-check flaky scores from historical report.json files.
///
/// Reads the `checks` array from each historical run's report.json and tracks
/// status transitions per check ID across runs. A check is "flaky" if its
/// status flips between runs. Returns only checks with flaky_score > 0,
/// sorted by flaky_score descending.
///
/// Completely defensive: parse errors or missing fields cause individual runs
/// to be skipped. Requires at least 2 historical runs to produce any output.
pub(crate) fn compute_flaky_scores(out_dir: &Path, max_runs: usize) -> Vec<FlakyCheckScore> {
    let parent = match out_dir.parent() {
        Some(p) => p,
        None => return Vec::new(),
    };

    let entries = match fs::read_dir(parent) {
        Ok(e) => e,
        Err(_) => return Vec::new(),
    };

    let mut dirs: Vec<String> = entries
        .filter_map(|e| e.ok())
        .filter(|e| {
            let name = e.file_name();
            let name_str = name.to_string_lossy();
            name_str != "latest"
                && e.path().is_dir()
                && name_str
                    .chars()
                    .next()
                    .map(|c| c.is_ascii_digit())
                    .unwrap_or(false)
        })
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect();

    dirs.sort();
    if dirs.len() > max_runs {
        dirs = dirs.split_off(dirs.len() - max_runs);
    }

    if dirs.len() < 2 {
        return Vec::new();
    }

    // Collect per-check statuses across runs (oldest first = chronological order)
    // Key: check_id, Value: Vec<(check_name, status_str)>
    let mut check_history: std::collections::BTreeMap<String, Vec<(String, String)>> =
        std::collections::BTreeMap::new();

    for dir_name in &dirs {
        let report_path = parent.join(dir_name).join("report.json");
        let content = match fs::read_to_string(&report_path) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let json: serde_json::Value = match serde_json::from_str(&content) {
            Ok(v) => v,
            Err(_) => continue,
        };

        let checks_arr = match json.get("checks").and_then(|c| c.as_array()) {
            Some(arr) => arr,
            None => continue,
        };

        for check in checks_arr {
            let id = match check.get("id").and_then(|v| v.as_str()) {
                Some(s) => s.to_string(),
                None => continue,
            };
            let status = match check.get("status").and_then(|v| v.as_str()) {
                Some(s) => s.to_string(),
                None => continue,
            };
            let name = check
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or(&id)
                .to_string();

            check_history.entry(id).or_default().push((name, status));
        }
    }

    // Compute flaky scores
    let mut scores: Vec<FlakyCheckScore> = check_history
        .into_iter()
        .filter_map(|(check_id, entries)| {
            let total_runs = entries.len();
            if total_runs < 2 {
                return None;
            }

            // Count transitions
            let statuses: Vec<&str> = entries.iter().map(|(_, s)| s.as_str()).collect();
            let transitions = statuses.windows(2).filter(|w| w[0] != w[1]).count();

            if transitions == 0 {
                return None;
            }

            let flaky_score = transitions as f64 / (total_runs - 1) as f64;

            // Last statuses for sparkline (newest first)
            let last_statuses: Vec<String> = entries
                .iter()
                .rev()
                .take(10)
                .map(|(_, s)| s.clone())
                .collect();

            // Use the most recent name for the check
            let check_name = entries.last().map(|(n, _)| n.clone()).unwrap_or_default();

            let confidence = match total_runs {
                0..=3 => "low",
                4..=7 => "medium",
                _ => "high",
            };

            Some(FlakyCheckScore {
                check_id,
                check_name,
                flaky_score,
                total_runs,
                transitions,
                last_statuses,
                confidence,
            })
        })
        .collect();

    // Sort by flaky_score descending (most flaky first)
    scores.sort_by(|a, b| {
        b.flaky_score
            .partial_cmp(&a.flaky_score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    scores
}
