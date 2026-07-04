//! Storage/pack readers for the MCP adapter.
//!
//! Everything here is a pure disk read over the prview storage tree
//! (`~/.prview/`) or a run's artifact pack. No review logic lives here — the
//! MCP surface only reads truth the core already wrote.

use crate::mcp::types::{ToolError, error_class};
use crate::storage::{RunEntry, RunIndex};
use std::path::{Path, PathBuf};

/// Prefix-tolerant commit comparison (short vs full SHA both directions),
/// mirroring the core's `commit_ids_match` so a run recorded with a 7-char
/// short SHA still matches a HEAD probe using the same length.
pub fn commit_matches(a: &str, b: &str) -> bool {
    !a.is_empty() && !b.is_empty() && (a == b || a.starts_with(b) || b.starts_with(a))
}

fn in_scope(e: &RunEntry, repo: &str, branch_key: &str) -> bool {
    e.repo == repo && e.branch == branch_key
}

/// Newest run (by `created_at`) for repo+branch whose commit matches HEAD.
///
/// This is the R3 contract: a run recorded on commit A never masquerades as
/// fresh once HEAD moves to B. Returns `None` when no run exists for HEAD.
pub fn latest_for_head<'a>(
    index: &'a RunIndex,
    repo: &str,
    branch_key: &str,
    head_short: &str,
) -> Option<&'a RunEntry> {
    index
        .entries()
        .iter()
        .filter(|e| in_scope(e, repo, branch_key) && commit_matches(&e.commit, head_short))
        .max_by(|a, b| a.created_at.cmp(&b.created_at))
}

/// Newest run (by `created_at`) for repo+branch, regardless of commit.
///
/// Informational only; always carries its own commit so a HEAD mismatch is
/// visible rather than hidden.
pub fn latest_any<'a>(index: &'a RunIndex, repo: &str, branch_key: &str) -> Option<&'a RunEntry> {
    index
        .entries()
        .iter()
        .filter(|e| in_scope(e, repo, branch_key))
        .max_by(|a, b| a.created_at.cmp(&b.created_at))
}

/// Validate an absolute `repo` argument and resolve its git top-level.
///
/// The MCP contract requires an ABSOLUTE path: the server MUST NOT rely on its
/// own cwd (`2026-07-01-prview-mcp-v1-design.md`). A relative path is rejected
/// at the boundary — before any `exists()`/git probe — so a review can never
/// silently resolve against wherever the server happens to run. (`invalid_args`
/// would be the precise class here, but the v1 schema has no such class yet;
/// `repo_not_found` is the closest fail-loud contract error. Adding a dedicated
/// class is a future schema evolution.)
///
/// `repo_not_found` when the path is not absolute or does not exist;
/// `not_a_git_repo` when it is not inside a git work tree.
pub fn resolve_repo_root(repo: &str) -> Result<PathBuf, ToolError> {
    let path = PathBuf::from(repo);
    if !path.is_absolute() {
        return Err(ToolError::new(
            error_class::REPO_NOT_FOUND,
            format!("repo path must be absolute: {repo}"),
        ));
    }
    if !path.exists() {
        return Err(ToolError::new(
            error_class::REPO_NOT_FOUND,
            format!("repo path does not exist: {repo}"),
        ));
    }

    let output = crate::git::git_cmd()
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(&path)
        .output()
        .map_err(|e| {
            ToolError::new(
                error_class::NOT_A_GIT_REPO,
                format!("failed to run git in {repo}: {e}"),
            )
        })?;

    if !output.status.success() {
        return Err(ToolError::new(
            error_class::NOT_A_GIT_REPO,
            format!("not a git repository: {repo}"),
        ));
    }

    let root = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if root.is_empty() {
        return Err(ToolError::new(
            error_class::NOT_A_GIT_REPO,
            format!("not a git repository (empty git output): {repo}"),
        ));
    }
    Ok(PathBuf::from(root))
}

/// Read the top-level `bases` array from a run's `MERGE_GATE.json`.
/// Returns an empty vec when the file or field is absent.
pub fn read_bases(run_dir: &Path) -> Vec<String> {
    let gate_path = run_dir.join("00_summary").join("MERGE_GATE.json");
    let Ok(text) = std::fs::read_to_string(&gate_path) else {
        return Vec::new();
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
        return Vec::new();
    };
    value
        .get("bases")
        .and_then(|b| b.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Run lifecycle status (pid-liveness)
// ---------------------------------------------------------------------------

/// Marker written by the MCP layer before a deep run detaches. Lets a later
/// call derive liveness without any in-memory state (design spec section 3).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RunningMarker {
    pub pid: u32,
    pub started_at: String,
    pub profile: String,
    pub commit: String,
    #[serde(default)]
    pub base_used: Vec<String>,
}

/// Deterministic run status. `SANITY.json` present wins (completed) even if a
/// stale `RUNNING.json` marker was left behind; otherwise liveness is derived
/// from the marker's pid — a dead pid is `Stale`, never an eternal `running`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunStatus {
    Completed,
    Running { pid: u32 },
    Stale { pid: u32, started_at: String },
    Failed,
}

const RUNNING_MARKER: &str = "RUNNING.json";

/// Path to a run's `RUNNING.json` MCP marker (top-level, not part of the pack).
pub fn running_marker_path(run_dir: &Path) -> PathBuf {
    run_dir.join(RUNNING_MARKER)
}

/// Read the `RUNNING.json` marker, if present and parseable.
pub fn read_running_marker(run_dir: &Path) -> Option<RunningMarker> {
    let text = std::fs::read_to_string(running_marker_path(run_dir)).ok()?;
    serde_json::from_str(&text).ok()
}

/// Derive the deterministic lifecycle status of a run directory.
pub fn run_status(run_dir: &Path) -> RunStatus {
    // SANITY.json is the completion truth: it is the last GUARANTEED
    // finalization artifact. Pack generation writes RUN.json FIRST, then
    // MANIFEST.json, then SANITY.json, then the OPTIONAL artifacts.zip. Keying
    // completion on RUN.json let a deep run's detached writer be caught
    // mid-finalization — a poller would see `completed` while MANIFEST/SANITY
    // (and any zip/symlink/index registration) were still being written, so a
    // client could consume a partial pack. SANITY.json present proves RUN.json
    // + MANIFEST.json + SANITY.json are all on disk (PR #12 review). It wins
    // over any lingering marker.
    if run_dir.join("00_summary").join("SANITY.json").exists() {
        return RunStatus::Completed;
    }

    match read_running_marker(run_dir) {
        Some(marker) => {
            if crate::storage::is_process_alive(marker.pid) {
                RunStatus::Running { pid: marker.pid }
            } else {
                RunStatus::Stale {
                    pid: marker.pid,
                    started_at: marker.started_at,
                }
            }
        }
        // No completion, no live/parseable marker: the run failed or never
        // produced a pack.
        None => RunStatus::Failed,
    }
}

// ---------------------------------------------------------------------------
// Run resolution (run_id -> run directory)
// ---------------------------------------------------------------------------

/// A run resolved to its on-disk directory plus identity.
#[derive(Debug, Clone)]
pub struct ResolvedRun {
    pub run_dir: PathBuf,
    pub run_id: String,
    pub commit: String,
}

/// Reject run ids that could escape the storage tree when joined into a path.
fn validate_run_id(run_id: &str) -> Result<(), ToolError> {
    let safe = !run_id.is_empty()
        && run_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    if safe && run_id != "." && run_id != ".." {
        Ok(())
    } else {
        Err(ToolError::new(
            error_class::RUN_NOT_FOUND,
            format!("invalid run_id: {run_id}"),
        ))
    }
}

fn ambiguous_run_id_error(repo_name: &str, run_id: &str, paths: &[PathBuf]) -> ToolError {
    ToolError::with_extra(
        error_class::STORAGE_CORRUPT,
        format!("ambiguous run_id {run_id} for {repo_name}; multiple runs match"),
        serde_json::json!({
            "run_id": run_id,
            "matches": paths
                .iter()
                .map(|path| path.to_string_lossy().to_string())
                .collect::<Vec<_>>(),
        }),
    )
}

fn find_index_entry_by_id<'a>(
    index: &'a RunIndex,
    repo_name: &str,
    run_id: &str,
) -> Result<Option<&'a RunEntry>, ToolError> {
    let matches: Vec<&RunEntry> = index
        .entries()
        .iter()
        .filter(|e| e.repo == repo_name && e.id == run_id)
        .collect();
    if matches.len() > 1 {
        let paths: Vec<PathBuf> = matches.iter().map(|entry| entry.path.clone()).collect();
        return Err(ambiguous_run_id_error(repo_name, run_id, &paths));
    }
    Ok(matches.into_iter().next())
}

/// Scan `runs/<repo>/*/<run_id>` for a run directory not (yet) in the index —
/// e.g. a deep run still in flight, registered only on completion.
fn find_run_dir_by_id(repo_name: &str, run_id: &str) -> Result<Option<PathBuf>, ToolError> {
    let base = crate::config::prview_home().join("runs").join(repo_name);
    find_run_dir_by_id_in(&base, repo_name, run_id)
}

fn find_run_dir_by_id_in(
    base: &Path,
    repo_name: &str,
    run_id: &str,
) -> Result<Option<PathBuf>, ToolError> {
    let read = match std::fs::read_dir(base) {
        Ok(read) => read,
        Err(_) => return Ok(None),
    };
    let mut matches = Vec::new();
    for branch in read.flatten() {
        if !branch.path().is_dir() {
            continue;
        }
        let candidate = branch.path().join(run_id);
        if candidate.is_dir() {
            matches.push(candidate);
        }
    }
    if matches.len() > 1 {
        return Err(ambiguous_run_id_error(repo_name, run_id, &matches));
    }
    Ok(matches.into_iter().next())
}

/// Resolve a run for `verdict`/`findings`/`read_artifact`.
///
/// With `run_id`: look it up in the index (completed runs), else scan storage
/// for an in-flight deep run. Without: the latest run for the current HEAD
/// (R3). A missing run is a fail-loud `run_not_found` — the agent then calls
/// `run_review`.
pub fn resolve_run(root: &Path, run_id: Option<&str>) -> Result<ResolvedRun, ToolError> {
    let repo_name = crate::config::repo_name_from_root(root);
    let index = RunIndex::load();

    match run_id {
        Some(id) => {
            validate_run_id(id)?;
            let disk_run = find_run_dir_by_id(&repo_name, id)?;
            if let Some(e) = find_index_entry_by_id(&index, &repo_name, id)? {
                if let Some(ref run_dir) = disk_run
                    && *run_dir != e.path
                {
                    return Err(ambiguous_run_id_error(
                        &repo_name,
                        id,
                        &[e.path.clone(), run_dir.clone()],
                    ));
                }
                return Ok(ResolvedRun {
                    run_dir: e.path.clone(),
                    run_id: id.to_string(),
                    commit: e.commit.clone(),
                });
            }
            match disk_run {
                Some(run_dir) => {
                    let commit = read_running_marker(&run_dir)
                        .map(|m| m.commit)
                        .unwrap_or_default();
                    Ok(ResolvedRun {
                        run_dir,
                        run_id: id.to_string(),
                        commit,
                    })
                }
                None => Err(ToolError::new(
                    error_class::RUN_NOT_FOUND,
                    format!("no run with id {id} for {repo_name}"),
                )),
            }
        }
        None => {
            let state = crate::state::collect_state(
                root,
                &crate::state::StateOpts {
                    fast: true,
                    json: true,
                    hot: false,
                },
            )
            .map_err(|e| {
                ToolError::new(
                    error_class::NOT_A_GIT_REPO,
                    format!("failed to read repo state: {e}"),
                )
            })?;
            // Key by the same storage key the write path uses so a detached
            // HEAD (display `HEAD (detached)`, stored under `HEAD`) resolves
            // instead of missing its own just-completed run (PR #12 review).
            let branch_key = crate::config::storage_branch_key(root);
            match latest_for_head(&index, &repo_name, &branch_key, &state.head) {
                Some(e) => Ok(ResolvedRun {
                    run_dir: e.path.clone(),
                    run_id: e.id.clone(),
                    commit: e.commit.clone(),
                }),
                None => Err(ToolError::new(
                    error_class::RUN_NOT_FOUND,
                    "no run for current HEAD; call run_review",
                )),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Findings (SARIF) + artifact body reads
// ---------------------------------------------------------------------------

const SARIF_REL: &str = "30_context/INLINE_FINDINGS.sarif";

/// A single structured finding lifted from the run's inline SARIF.
#[derive(Debug, Clone)]
pub struct FindingItem {
    pub file: String,
    pub line: u64,
    pub severity: String,
    pub rule: String,
    pub message: String,
}

/// Read all inline findings for a run in a deterministic order (file, line,
/// rule). A missing SARIF file is an honest empty set (no findings), not an
/// error — prview only writes the file when there are findings.
pub fn read_findings(run_dir: &Path) -> Vec<FindingItem> {
    let sarif_path = run_dir.join("30_context").join("INLINE_FINDINGS.sarif");
    let Ok(text) = std::fs::read_to_string(&sarif_path) else {
        return Vec::new();
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
        return Vec::new();
    };

    let mut items = Vec::new();
    let runs = value.get("runs").and_then(|r| r.as_array());
    for run in runs.into_iter().flatten() {
        let results = run.get("results").and_then(|r| r.as_array());
        for result in results.into_iter().flatten() {
            let rule = result
                .get("ruleId")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let severity = result
                .get("level")
                .and_then(|v| v.as_str())
                .unwrap_or("warning")
                .to_string();
            let message = result
                .get("message")
                .and_then(|m| m.get("text"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let physical = result
                .get("locations")
                .and_then(|l| l.as_array())
                .and_then(|arr| arr.first())
                .and_then(|loc| loc.get("physicalLocation"));
            let file = physical
                .and_then(|p| p.get("artifactLocation"))
                .and_then(|a| a.get("uri"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let line = physical
                .and_then(|p| p.get("region"))
                .and_then(|r| r.get("startLine"))
                .and_then(|v| v.as_u64())
                .unwrap_or(0);

            items.push(FindingItem {
                file,
                line,
                severity,
                rule,
                message,
            });
        }
    }

    items.sort_by(|a, b| {
        a.file
            .cmp(&b.file)
            .then(a.line.cmp(&b.line))
            .then(a.rule.cmp(&b.rule))
    });
    items
}

/// Pack-relative SARIF path, used as `artifact_ref` on findings.
pub fn sarif_ref() -> &'static str {
    SARIF_REL
}

/// Resolve a pack-relative artifact path, guaranteeing it stays inside the run
/// directory even through symlinks (R5). Any escape or missing file collapses
/// to `artifact_missing` — never revealing what exists outside the run.
pub fn resolve_artifact_path(run_dir: &Path, artifact: &str) -> Result<PathBuf, ToolError> {
    crate::paths::resolve_existing_path_within(run_dir, Path::new(artifact)).map_err(|_| {
        ToolError::new(
            error_class::ARTIFACT_MISSING,
            format!("artifact not found within run: {artifact}"),
        )
    })
}

// ---------------------------------------------------------------------------
// Decision normalization (R1 adapter)
// ---------------------------------------------------------------------------

/// A coherent decision surface derived from the core's `MERGE_GATE.json`.
///
/// The MCP layer is a contract ADAPTER, not a passive proxy: when the core
/// emits contradictory signals (e.g. `allow_merge: true` alongside a `block`
/// recommendation), the most conservative signal wins and `allow_merge` is
/// always DERIVED from the final recommendation. Any correction sets
/// `normalized` and records the originals in `caveats` (`core_inconsistency`).
/// When the core is self-consistent this is a pure passthrough.
#[derive(Debug, Clone)]
pub struct NormalizedDecision {
    pub merge_recommendation: String,
    pub allow_merge: bool,
    pub verdict: String,
    pub blocking_issues: Vec<String>,
    pub caveats: Vec<String>,
    pub base_used: Vec<String>,
    pub normalized: bool,
}

/// Conservativeness rank: BLOCK(3) > HOLD/review_required(2) > APPROVE/PASS(1).
fn rank_from_merge_rec(s: &str) -> Option<u8> {
    match s.to_ascii_lowercase().as_str() {
        "block" => Some(3),
        "review_required" | "hold" => Some(2),
        "approve" => Some(1),
        _ => None,
    }
}

fn rank_from_verdict(s: &str) -> Option<u8> {
    match s.to_ascii_uppercase().as_str() {
        "BLOCK" => Some(3),
        // `CONDITIONAL` is the unified core vocabulary (PV-03/04); `HOLD` is the
        // retired legacy synonym, still recognized so the adapter stays a safe
        // read-back net for pre-2.1 runs on disk.
        "CONDITIONAL" | "HOLD" => Some(2),
        // `ALLOW` is the retired pre-2.1 verdict synonym for a clean pass (folded
        // to `PASS` on the CLI `--json` surface in `output::read_merge_gate_summary`).
        // The adapter recognizes it for the same reason it recognizes `HOLD`:
        // a legacy gate on disk must still normalize instead of failing loud.
        "PASS" | "APPROVE" | "ALLOW" => Some(1),
        _ => None,
    }
}

fn merge_rec_from_rank(rank: u8) -> &'static str {
    match rank {
        3 => "block",
        2 => "review_required",
        _ => "approve",
    }
}

fn verdict_from_rank(rank: u8) -> &'static str {
    match rank {
        3 => "BLOCK",
        2 => "CONDITIONAL",
        _ => "PASS",
    }
}

fn string_array(value: Option<&serde_json::Value>) -> Vec<String> {
    value
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// Read and normalize a run's merge decision (R1). Missing/invalid
/// `MERGE_GATE.json` is a fail-loud `storage_corrupt`, never a silent default.
pub fn read_decision(run_dir: &Path) -> Result<NormalizedDecision, ToolError> {
    let gate_path = run_dir.join("00_summary").join("MERGE_GATE.json");
    let text = std::fs::read_to_string(&gate_path).map_err(|_| {
        ToolError::new(
            error_class::STORAGE_CORRUPT,
            format!("MERGE_GATE.json not found: {}", gate_path.display()),
        )
    })?;
    let value: serde_json::Value = serde_json::from_str(&text).map_err(|e| {
        ToolError::new(
            error_class::STORAGE_CORRUPT,
            format!("MERGE_GATE.json is not valid JSON: {e}"),
        )
    })?;
    let decision = value.get("decision").ok_or_else(|| {
        ToolError::new(
            error_class::STORAGE_CORRUPT,
            "MERGE_GATE.json missing `decision` object",
        )
    })?;

    let raw_merge = decision
        .get("merge_recommendation")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let raw_verdict = decision
        .get("verdict")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let raw_allow = decision.get("allow_merge").and_then(|v| v.as_bool());

    let merge_rank = raw_merge.as_deref().and_then(rank_from_merge_rec);
    let verdict_rank = raw_verdict.as_deref().and_then(rank_from_verdict);

    // Need at least one decision signal to build a truthful surface.
    if merge_rank.is_none() && verdict_rank.is_none() {
        return Err(ToolError::new(
            error_class::STORAGE_CORRUPT,
            "MERGE_GATE.json decision has no recognizable merge_recommendation or verdict",
        ));
    }

    // allow_merge=false raises conservativeness to at least HOLD; allow=true
    // never lowers it (a permissive flag can't override a block/hold signal).
    let allow_rank = raw_allow.map(|allow| if allow { 1 } else { 2 });

    let final_rank = [merge_rank, verdict_rank, allow_rank]
        .into_iter()
        .flatten()
        .max()
        .unwrap_or(2);

    let allow_merge = final_rank == 1;

    // Inconsistent iff the raw signals disagree on conservativeness, or the raw
    // allow_merge flag contradicts the final (derived) recommendation.
    let signal_ranks: Vec<u8> = [merge_rank, verdict_rank].into_iter().flatten().collect();
    let signals_disagree = signal_ranks.iter().any(|&r| r != final_rank);
    let allow_contradicts = raw_allow.map(|a| a != allow_merge).unwrap_or(false);
    let normalized = signals_disagree || allow_contradicts;

    let mut caveats = Vec::new();
    if normalized {
        caveats.push(format!(
            "core_inconsistency: original allow_merge={}, merge_recommendation={}, verdict={}",
            raw_allow
                .map(|b| b.to_string())
                .unwrap_or_else(|| "null".to_string()),
            raw_merge.as_deref().unwrap_or("null"),
            raw_verdict.as_deref().unwrap_or("null"),
        ));
    }
    caveats.extend(string_array(decision.get("review_caveats")));

    Ok(NormalizedDecision {
        merge_recommendation: merge_rec_from_rank(final_rank).to_string(),
        allow_merge,
        verdict: verdict_from_rank(final_rank).to_string(),
        blocking_issues: string_array(decision.get("blocking_issues")),
        caveats,
        base_used: string_array(value.get("bases")),
        normalized,
    })
}

/// Read the top-level `generated_at` timestamp from a run's `MERGE_GATE.json`.
pub fn read_generated_at(run_dir: &Path) -> Option<String> {
    let gate_path = run_dir.join("00_summary").join("MERGE_GATE.json");
    let text = std::fs::read_to_string(&gate_path).ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    value
        .get("generated_at")
        .and_then(|v| v.as_str())
        .map(str::to_string)
}

/// Read a run's per-gate check summary (`{id, status, reason, evidence}`) from
/// `MERGE_GATE.json`. Empty when the file or `checks` array is absent.
pub fn read_gates(run_dir: &Path) -> Vec<serde_json::Value> {
    let gate_path = run_dir.join("00_summary").join("MERGE_GATE.json");
    let Ok(text) = std::fs::read_to_string(&gate_path) else {
        return Vec::new();
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
        return Vec::new();
    };
    value
        .get("checks")
        .and_then(|c| c.as_array())
        .map(|arr| {
            arr.iter()
                .map(|g| {
                    serde_json::json!({
                        "id": g.get("id").cloned().unwrap_or(serde_json::Value::Null),
                        "status": g.get("status").cloned().unwrap_or(serde_json::Value::Null),
                        "reason": g.get("reason").cloned().unwrap_or(serde_json::Value::Null),
                        "evidence": g.get("evidence").cloned().unwrap_or(serde_json::Value::Null),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::RunEntry;
    use std::io::Write;

    fn write_gate(run_dir: &Path, gate: &serde_json::Value) {
        let summary = run_dir.join("00_summary");
        std::fs::create_dir_all(&summary).unwrap();
        std::fs::write(
            summary.join("MERGE_GATE.json"),
            serde_json::to_string_pretty(gate).unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn consistent_block_is_passthrough() {
        let dir = tempfile::tempdir().unwrap();
        write_gate(
            dir.path(),
            &serde_json::json!({
                "bases": ["develop", "main"],
                "decision": {
                    "merge_recommendation": "block",
                    "verdict": "BLOCK",
                    "allow_merge": false,
                    "blocking_issues": ["clippy failed"],
                    "review_caveats": ["High-risk surface"]
                }
            }),
        );
        let d = read_decision(dir.path()).unwrap();
        assert_eq!(d.merge_recommendation, "block");
        assert_eq!(d.verdict, "BLOCK");
        assert!(!d.allow_merge);
        assert!(!d.normalized);
        assert!(!d.caveats.iter().any(|c| c.contains("core_inconsistency")));
        assert_eq!(d.base_used, vec!["develop", "main"]);
        assert_eq!(d.blocking_issues, vec!["clippy failed"]);
    }

    #[test]
    fn allow_true_with_block_is_normalized_conservative() {
        let dir = tempfile::tempdir().unwrap();
        write_gate(
            dir.path(),
            &serde_json::json!({
                "bases": ["main"],
                "decision": {
                    "merge_recommendation": "block",
                    "verdict": "BLOCK",
                    "allow_merge": true,
                    "review_caveats": []
                }
            }),
        );
        let d = read_decision(dir.path()).unwrap();
        assert_eq!(d.merge_recommendation, "block");
        assert!(!d.allow_merge, "allow_merge must be derived, not passed");
        assert!(d.normalized);
        let caveat = d
            .caveats
            .iter()
            .find(|c| c.contains("core_inconsistency"))
            .expect("core_inconsistency caveat present");
        assert!(caveat.contains("original allow_merge=true"));
        assert!(caveat.contains("merge_recommendation=block"));
    }

    #[test]
    fn legacy_hold_verdict_with_allow_true_normalizes_to_conditional() {
        // A pre-2.1 core could emit `HOLD` (review-required) beside
        // `allow_merge: true`. The adapter recognizes the legacy token, lifts to
        // the conservative rank, and re-emits the unified `CONDITIONAL`.
        let dir = tempfile::tempdir().unwrap();
        write_gate(
            dir.path(),
            &serde_json::json!({
                "bases": [],
                "decision": {
                    "merge_recommendation": "approve",
                    "verdict": "HOLD",
                    "allow_merge": true
                }
            }),
        );
        let d = read_decision(dir.path()).unwrap();
        assert_eq!(d.verdict, "CONDITIONAL");
        assert_eq!(d.merge_recommendation, "review_required");
        assert!(!d.allow_merge);
        assert!(d.normalized);
    }

    #[test]
    fn legacy_allow_verdict_normalizes_to_pass() {
        // A pre-2.1 core could emit the retired `ALLOW` verdict synonym for a
        // clean pass. With no `merge_recommendation` present it is the sole
        // decision signal, so before ALLOW was recognized `read_decision` failed
        // loud (`storage_corrupt`). The adapter must instead fold it onto the
        // unified `PASS`, matching `output::read_merge_gate_summary`.
        let dir = tempfile::tempdir().unwrap();
        write_gate(
            dir.path(),
            &serde_json::json!({
                "bases": ["main"],
                "decision": {
                    "verdict": "ALLOW",
                    "allow_merge": true
                }
            }),
        );
        let d = read_decision(dir.path()).unwrap();
        assert_eq!(d.verdict, "PASS");
        assert_eq!(d.merge_recommendation, "approve");
        assert!(d.allow_merge, "legacy ALLOW is a clean pass");
        assert!(
            !d.normalized,
            "ALLOW+allow_merge:true is self-consistent, no core_inconsistency"
        );
    }

    #[test]
    fn healthy_conditional_core_is_passthrough_no_inconsistency() {
        // A self-consistent post-PV-03/04 core (CONDITIONAL verdict +
        // review_required recommendation + derived allow_merge:false) must be a
        // pure passthrough: the adapter is a safety net, a no-op on a healthy
        // core (never a false `core_inconsistency`).
        let dir = tempfile::tempdir().unwrap();
        write_gate(
            dir.path(),
            &serde_json::json!({
                "bases": ["main"],
                "decision": {
                    "merge_recommendation": "review_required",
                    "verdict": "CONDITIONAL",
                    "allow_merge": false,
                    "review_caveats": ["3 inline findings"]
                }
            }),
        );
        let d = read_decision(dir.path()).unwrap();
        assert_eq!(d.verdict, "CONDITIONAL");
        assert_eq!(d.merge_recommendation, "review_required");
        assert!(!d.allow_merge);
        assert!(!d.normalized, "healthy core must not be normalized");
        assert!(
            !d.caveats.iter().any(|c| c.contains("core_inconsistency")),
            "no false core_inconsistency on a healthy core"
        );
    }

    #[test]
    fn healthy_pass_core_is_passthrough_no_inconsistency() {
        // The clean-PASS equivalent: approve + PASS + allow_merge:true is
        // self-consistent and must pass through untouched.
        let dir = tempfile::tempdir().unwrap();
        write_gate(
            dir.path(),
            &serde_json::json!({
                "bases": ["main"],
                "decision": {
                    "merge_recommendation": "approve",
                    "verdict": "PASS",
                    "allow_merge": true
                }
            }),
        );
        let d = read_decision(dir.path()).unwrap();
        assert_eq!(d.verdict, "PASS");
        assert_eq!(d.merge_recommendation, "approve");
        assert!(d.allow_merge);
        assert!(!d.normalized);
        assert!(!d.caveats.iter().any(|c| c.contains("core_inconsistency")));
    }

    #[test]
    fn missing_merge_gate_is_storage_corrupt() {
        let dir = tempfile::tempdir().unwrap();
        let err = read_decision(dir.path()).unwrap_err();
        assert_eq!(err.class, error_class::STORAGE_CORRUPT);
    }

    fn write_marker(run_dir: &Path, pid: u32) {
        let marker = RunningMarker {
            pid,
            started_at: "2026-07-01T12:00:00Z".to_string(),
            profile: "deep".to_string(),
            commit: "abc1234".to_string(),
            base_used: vec!["main".to_string()],
        };
        std::fs::write(
            running_marker_path(run_dir),
            serde_json::to_string(&marker).unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn run_status_completed_wins_over_marker() {
        let dir = tempfile::tempdir().unwrap();
        // Both a lingering live marker AND a finalized pack (SANITY.json):
        // completion wins.
        write_marker(dir.path(), std::process::id());
        let summary = dir.path().join("00_summary");
        std::fs::create_dir_all(&summary).unwrap();
        std::fs::write(summary.join("RUN.json"), "{}").unwrap();
        std::fs::write(summary.join("SANITY.json"), "{}").unwrap();
        assert_eq!(run_status(dir.path()), RunStatus::Completed);
    }

    #[test]
    fn run_status_not_completed_while_pack_still_finalizing() {
        let dir = tempfile::tempdir().unwrap();
        // RUN.json is written FIRST during finalization; MANIFEST.json and
        // SANITY.json follow. RUN.json alone must NOT read as completed while
        // the writer is still finalizing the pack (PR #12 review).
        let summary = dir.path().join("00_summary");
        std::fs::create_dir_all(&summary).unwrap();
        std::fs::write(summary.join("RUN.json"), "{}").unwrap();

        // Writer still alive: the run is running, not completed.
        write_marker(dir.path(), std::process::id());
        assert_eq!(
            run_status(dir.path()),
            RunStatus::Running {
                pid: std::process::id()
            },
        );

        // Writer died mid-finalization (dead pid): stale, never a fake
        // completion that would expose a partial pack.
        std::fs::remove_file(running_marker_path(dir.path())).unwrap();
        write_marker(dir.path(), 2_147_483_646);
        match run_status(dir.path()) {
            RunStatus::Stale { pid, .. } => assert_eq!(pid, 2_147_483_646),
            other => panic!("expected Stale for a partial pack, got {other:?}"),
        }
    }

    #[test]
    fn run_status_running_for_live_pid() {
        let dir = tempfile::tempdir().unwrap();
        write_marker(dir.path(), std::process::id());
        assert_eq!(
            run_status(dir.path()),
            RunStatus::Running {
                pid: std::process::id()
            }
        );
    }

    #[test]
    fn run_status_stale_for_dead_pid() {
        let dir = tempfile::tempdir().unwrap();
        // pid 0x7FFF_FFFF is effectively never a live process.
        write_marker(dir.path(), 2_147_483_646);
        match run_status(dir.path()) {
            RunStatus::Stale { pid, started_at } => {
                assert_eq!(pid, 2_147_483_646);
                assert!(!started_at.is_empty());
            }
            other => panic!("expected Stale, got {other:?}"),
        }
    }

    #[test]
    fn run_status_failed_for_empty_dir() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(run_status(dir.path()), RunStatus::Failed);
    }

    #[test]
    fn validate_run_id_rejects_traversal() {
        assert!(validate_run_id("20260101-120000").is_ok());
        assert!(validate_run_id("../escape").is_err());
        assert!(validate_run_id("a/b").is_err());
        assert!(validate_run_id("..").is_err());
        assert!(validate_run_id("").is_err());
    }

    #[test]
    fn find_run_dir_by_id_accepts_legacy_timestamp_id() {
        let tmp = tempfile::tempdir().unwrap();
        let run_id = "20260101-120000";
        let run_dir = tmp.path().join("main").join(run_id);
        std::fs::create_dir_all(&run_dir).unwrap();

        let resolved = find_run_dir_by_id_in(tmp.path(), "demo", run_id)
            .unwrap()
            .unwrap();

        assert_eq!(resolved, run_dir);
    }

    #[test]
    fn find_run_dir_by_id_fails_loud_on_cross_branch_ambiguity() {
        let tmp = tempfile::tempdir().unwrap();
        let run_id = "20260101-120000";
        std::fs::create_dir_all(tmp.path().join("main").join(run_id)).unwrap();
        std::fs::create_dir_all(tmp.path().join("feature").join(run_id)).unwrap();

        let err = find_run_dir_by_id_in(tmp.path(), "demo", run_id).unwrap_err();

        assert_eq!(err.class, error_class::STORAGE_CORRUPT);
        assert!(err.message.contains("ambiguous run_id"));
        assert_eq!(err.extra["run_id"], run_id);
        assert_eq!(err.extra["matches"].as_array().unwrap().len(), 2);
    }

    fn write_sarif(run_dir: &Path, sarif: &serde_json::Value) {
        let ctx = run_dir.join("30_context");
        std::fs::create_dir_all(&ctx).unwrap();
        std::fs::write(
            ctx.join("INLINE_FINDINGS.sarif"),
            serde_json::to_string_pretty(sarif).unwrap(),
        )
        .unwrap();
    }

    fn sarif_result(uri: &str, line: u64, level: &str, rule: &str) -> serde_json::Value {
        serde_json::json!({
            "ruleId": rule,
            "level": level,
            "message": { "text": format!("{rule} at {uri}:{line}") },
            "locations": [{
                "physicalLocation": {
                    "artifactLocation": { "uri": uri },
                    "region": { "startLine": line }
                }
            }]
        })
    }

    #[test]
    fn read_findings_parses_and_sorts() {
        let dir = tempfile::tempdir().unwrap();
        write_sarif(
            dir.path(),
            &serde_json::json!({
                "version": "2.1.0",
                "runs": [{
                    "results": [
                        sarif_result("src/b.rs", 10, "warning", "w1"),
                        sarif_result("src/a.rs", 5, "error", "e1"),
                        sarif_result("src/a.rs", 2, "note", "n1"),
                    ]
                }]
            }),
        );
        let items = read_findings(dir.path());
        assert_eq!(items.len(), 3);
        // Sorted by (file, line, rule): a.rs:2, a.rs:5, b.rs:10.
        assert_eq!(items[0].file, "src/a.rs");
        assert_eq!(items[0].line, 2);
        assert_eq!(items[1].line, 5);
        assert_eq!(items[2].file, "src/b.rs");
        assert_eq!(items[1].severity, "error");
    }

    #[test]
    fn read_findings_missing_sarif_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        assert!(read_findings(dir.path()).is_empty());
    }

    #[test]
    fn resolve_artifact_path_guards_traversal() {
        let dir = tempfile::tempdir().unwrap();
        let summary = dir.path().join("00_summary");
        std::fs::create_dir_all(&summary).unwrap();
        std::fs::write(summary.join("MERGE_GATE.json"), "{}").unwrap();

        // Legit pack-relative path resolves.
        assert!(resolve_artifact_path(dir.path(), "00_summary/MERGE_GATE.json").is_ok());
        // Parent traversal, absolute path → artifact_missing, no read.
        assert_eq!(
            resolve_artifact_path(dir.path(), "../../../etc/passwd")
                .unwrap_err()
                .class,
            error_class::ARTIFACT_MISSING
        );
        assert_eq!(
            resolve_artifact_path(dir.path(), "/etc/passwd")
                .unwrap_err()
                .class,
            error_class::ARTIFACT_MISSING
        );
    }

    #[cfg(unix)]
    #[test]
    fn resolve_artifact_path_rejects_symlink_escape() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let secret = outside.path().join("secret.txt");
        std::fs::write(&secret, "top secret").unwrap();
        // A symlink inside the run dir pointing outside must not be readable.
        std::os::unix::fs::symlink(&secret, dir.path().join("escape")).unwrap();
        assert_eq!(
            resolve_artifact_path(dir.path(), "escape")
                .unwrap_err()
                .class,
            error_class::ARTIFACT_MISSING
        );
    }

    fn entry(id: &str, commit: &str, created_at: &str) -> RunEntry {
        RunEntry {
            id: id.to_string(),
            repo: "demo".to_string(),
            branch: "main".to_string(),
            commit: commit.to_string(),
            path: PathBuf::from(format!("/tmp/demo/main/{id}")),
            created_at: created_at.to_string(),
            quality_pass: true,
            merge_status: "ALLOW".to_string(),
            policy_mode: "warn".to_string(),
            checks_passed: 1,
            checks_failed: 0,
            files_changed: 1,
            size_bytes: 0,
            has_dashboard: false,
        }
    }

    fn index_from(entries: &[RunEntry]) -> (tempfile::TempDir, RunIndex) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.jsonl");
        let mut f = std::fs::File::create(&path).unwrap();
        for e in entries {
            writeln!(f, "{}", serde_json::to_string(e).unwrap()).unwrap();
        }
        f.flush().unwrap();
        let index = RunIndex::load_from(&path);
        (dir, index)
    }

    #[test]
    fn find_index_entry_by_id_accepts_legacy_timestamp_id() {
        let id = "20260101-120000";
        let entries = vec![entry(id, "aaaa111", "2026-01-01T00:00:00Z")];
        let (_tmp, index) = index_from(&entries);

        let found = find_index_entry_by_id(&index, "demo", id).unwrap().unwrap();

        assert_eq!(found.id, id);
    }

    #[test]
    fn find_index_entry_by_id_fails_loud_on_duplicate_ids() {
        let id = "20260101-120000";
        let mut feature = entry(id, "bbbb222", "2026-01-01T00:00:01Z");
        feature.branch = "feature".to_string();
        feature.path = PathBuf::from(format!("/tmp/demo/feature/{id}"));
        let entries = vec![entry(id, "aaaa111", "2026-01-01T00:00:00Z"), feature];
        let (_tmp, index) = index_from(&entries);

        let err = find_index_entry_by_id(&index, "demo", id).unwrap_err();

        assert_eq!(err.class, error_class::STORAGE_CORRUPT);
        assert!(err.message.contains("ambiguous run_id"));
        assert_eq!(err.extra["matches"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn latest_for_head_filters_by_commit() {
        // commit "aaaa" older, "aaaa" newer, "bbbb" newest overall.
        let entries = vec![
            entry("20260101-000001", "aaaa111", "2026-01-01T00:00:01Z"),
            entry("20260101-000002", "aaaa111", "2026-01-01T00:00:02Z"),
            entry("20260101-000003", "bbbb222", "2026-01-01T00:00:03Z"),
        ];
        let (_tmp, index) = index_from(&entries);

        // HEAD = aaaa111 → newer aaaa entry, not bbbb.
        let head = latest_for_head(&index, "demo", "main", "aaaa111").unwrap();
        assert_eq!(head.id, "20260101-000002");

        // HEAD = cccc333 (no run) → None; latest_any still returns bbbb.
        assert!(latest_for_head(&index, "demo", "main", "cccc333").is_none());
        let any = latest_any(&index, "demo", "main").unwrap();
        assert_eq!(any.id, "20260101-000003");
    }

    #[test]
    fn scope_filters_repo_and_branch() {
        let mut other = entry("20260101-000009", "aaaa111", "2026-01-01T00:00:09Z");
        other.repo = "elsewhere".to_string();
        let entries = vec![
            entry("20260101-000001", "aaaa111", "2026-01-01T00:00:01Z"),
            other,
        ];
        let (_tmp, index) = index_from(&entries);
        assert!(latest_any(&index, "elsewhere", "main").is_some());
        assert_eq!(
            latest_for_head(&index, "demo", "main", "aaaa111")
                .unwrap()
                .id,
            "20260101-000001"
        );
        assert!(latest_for_head(&index, "demo", "other-branch", "aaaa111").is_none());
    }

    #[test]
    fn commit_matches_is_prefix_tolerant() {
        assert!(commit_matches("aaaa111", "aaaa111"));
        assert!(commit_matches("aaaa111", "aaaa111abcdef"));
        assert!(commit_matches("aaaa111abcdef", "aaaa111"));
        assert!(!commit_matches("aaaa111", "bbbb222"));
        assert!(!commit_matches("", "aaaa111"));
    }
}
