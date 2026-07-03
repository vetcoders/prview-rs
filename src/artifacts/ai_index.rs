//! AI_INDEX.md generation.

use super::*;

// ── PR_REVIEW.md ────────────────────────────────────────────────────

pub(crate) fn generate_ai_index(
    dir: &Path,
    config: &Config,
    diffs: &[Diff],
    checks: &[CheckResult],
    coverage: &CoverageDelta,
) -> Result<()> {
    use std::fmt::Write;

    let timestamp = chrono::Local::now().to_rfc3339();
    let (target, base, base_sha, target_sha) = if let Some(diff) = diffs.first() {
        (
            diff.target.as_str(),
            diff.base.as_str(),
            diff.base_commit_id.as_str(),
            diff.target_commit_id.as_str(),
        )
    } else {
        (
            config.target.as_deref().unwrap_or("HEAD"),
            config.bases.first().map(|s| s.as_str()).unwrap_or("main"),
            "",
            "",
        )
    };
    let commit_count = diffs.first().map(|d| d.commits.len()).unwrap_or(0);
    let files_changed = diffs.iter().flat_map(|d| &d.files).count();
    let failed_checks = checks.iter().filter(|check| check.is_failure()).count();
    let warned_checks = checks
        .iter()
        .filter(|check| matches!(check.status, CheckStatus::Warnings))
        .count();

    let mut md = String::new();
    writeln!(md, "# AI Review Index\n")?;
    writeln!(md, "- Generated: {}", timestamp)?;
    writeln!(
        md,
        "- PR: {}",
        config
            .pr_number
            .map_or("n/a".to_string(), |n| n.to_string())
    )?;
    writeln!(md, "- Base: `{}`", base)?;
    writeln!(md, "- Head: `{}`", target)?;
    writeln!(md, "- Commits: {}", commit_count)?;
    writeln!(md, "- Files changed: {}", files_changed)?;
    writeln!(
        md,
        "- Reviewed range: `{}`...`{}`",
        short_or_empty(base_sha),
        short_or_empty(target_sha)
    )?;
    writeln!(
        md,
        "- Check summary: {} failure(s), {} warning(s)",
        failed_checks, warned_checks
    )?;
    writeln!(
        md,
        "- Coverage signal: {}/{} changed code files ({}%)",
        coverage.covered_count, coverage.total_source, coverage.pct
    )?;
    let gate_path = Path::new("00_summary/MERGE_GATE.json");
    if dir.join(gate_path).exists()
        && let Ok(raw) = read_to_string_within(dir, gate_path)
        && let Ok(gate) = serde_json::from_str::<serde_json::Value>(&raw)
    {
        let verdict = gate["decision"]["verdict"].as_str().unwrap_or("UNKNOWN");
        let reason = gate["decision"]["decision_reason"]
            .as_str()
            .or_else(|| gate["decision"]["reason"].as_str())
            .unwrap_or("no reason recorded");
        let allow = gate["decision"]["allow_merge"].as_bool();
        writeln!(md, "- Gate verdict: `{}` — {}", verdict, reason)?;
        if let Some(allow) = allow {
            writeln!(md, "- Allow merge: `{}`", allow)?;
        }
    }
    writeln!(md)?;

    writeln!(md, "## Recommended reading order (human-first)\n")?;
    let mut step = 1;
    if dir.join("dashboard.html").exists() {
        writeln!(
            md,
            "{}. `dashboard.html` — interactive human review dashboard.",
            step
        )?;
        step += 1;
    }
    if dir.join("review.html").exists() {
        writeln!(
            md,
            "{}. `review.html` — standard portable human review export.",
            step
        )?;
        step += 1;
    }
    writeln!(
        md,
        "{}. `00_summary/MERGE_GATE.md` — human gate verdict.",
        step
    )?;
    step += 1;
    writeln!(
        md,
        "{}. `00_summary/MERGE_GATE.json` — canonical gate data.",
        step
    )?;
    step += 1;
    if dir.join("00_summary/FAILURES_SUMMARY.md").exists() {
        writeln!(
            md,
            "{}. `00_summary/FAILURES_SUMMARY.md` — failing check details.",
            step
        )?;
        step += 1;
    }
    writeln!(
        md,
        "{}. `PR_REVIEW.md` — review narrative and changed-file map.",
        step
    )?;
    step += 1;
    if dir.join("30_context/INLINE_FINDINGS.sarif").exists() {
        writeln!(
            md,
            "{}. `30_context/INLINE_FINDINGS.sarif` — structured inline findings.",
            step
        )?;
    }
    writeln!(md)?;

    writeln!(md, "## Layout\n")?;
    writeln!(
        md,
        "- `00_summary/` — run, gate, metadata, sanity, manifest."
    )?;
    writeln!(
        md,
        "- `10_diff/` — full patch and per-commit/per-file diffs."
    )?;
    writeln!(
        md,
        "- `20_quality/` — check logs and quality signal reports."
    )?;
    writeln!(md, "- `30_context/` — contextual scanners and SARIF.")?;
    writeln!(md, "- `report.json` — machine-readable aggregate report.")?;
    if dir.join("review.html").exists() {
        writeln!(md, "- `review.html` — standard browser review export.")?;
    }
    if dir.join("dashboard.html").exists() {
        writeln!(md, "- `dashboard.html` — browser review dashboard.")?;
    }
    writeln!(md)?;

    writeln!(md, "## If you need deeper context\n")?;
    writeln!(md, "- `20_quality/full-checks.log` — combined quality log.")?;
    writeln!(md, "- `10_diff/full.patch` — canonical patch text.")?;
    writeln!(
        md,
        "- `10_diff/per-commit-diffs/` — commit-by-commit patches."
    )?;
    if dir.join("00_summary/file-status.txt").exists() {
        writeln!(
            md,
            "- `00_summary/file-status.txt` — changed file status table."
        )?;
    }
    if dir.join("30_context/changed-tests.txt").exists() {
        writeln!(md, "- `30_context/changed-tests.txt` — changed test files.")?;
    }
    writeln!(md)?;

    writeln!(md, "## Packs\n")?;
    if dir.join("artifacts.zip").exists() || config.create_zip {
        writeln!(md, "- `artifacts.zip` — portable artifact pack.")?;
    } else {
        writeln!(md, "- ZIP output was disabled for this run.")?;
    }

    fs::write(dir.join("AI_INDEX.md"), md)?;
    Ok(())
}

pub(crate) fn short_or_empty(sha: &str) -> &str {
    &sha[..sha.len().min(12)]
}
