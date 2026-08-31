//! Storage/pack readers for the MCP adapter.
//!
//! Everything here is a pure disk read over the prview storage tree
//! (`~/.prview/`) or a run's artifact pack. No review logic lives here — the
//! MCP surface only reads truth the core already wrote.

use crate::gate::{
    JsonKind, merge_rec_from_rank, rank_from_merge_rec, rank_from_verdict, readable_signal,
    verdict_from_rank,
};
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

/// Deterministic run status. Completion requires both finalized pack bytes and
/// the exact durable index row. Otherwise liveness is derived from the marker's
/// pid — a dead publisher is `Stale`, never a fake completed run.
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
    // SANITY is necessary but not sufficient: it precedes the transactional
    // index/latest commit. A lossy read is acceptable only for this lifecycle
    // hint; every MCP success path separately uses `strict_run_index`.
    let finalized = run_dir.join("00_summary").join("SANITY.json").exists();
    let run_id = run_dir.file_name().and_then(|name| name.to_str());
    let published = RunIndex::load()
        .entries()
        .iter()
        .any(|entry| Some(entry.id.as_str()) == run_id && same_run_path(&entry.path, run_dir));
    if finalized && published {
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
    let mut matches: Vec<&RunEntry> = Vec::new();
    for entry in index
        .entries()
        .iter()
        .filter(|e| e.repo == repo_name && e.id == run_id)
    {
        if !matches
            .iter()
            .any(|existing| same_run_path(&existing.path, &entry.path))
        {
            matches.push(entry);
        }
    }
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

fn same_run_path(left: &Path, right: &Path) -> bool {
    match (left.canonicalize(), right.canonicalize()) {
        (Ok(left), Ok(right)) => left == right,
        _ => left == right,
    }
}

fn strict_run_index() -> Result<RunIndex, ToolError> {
    RunIndex::load_strict().map_err(|error| {
        ToolError::new(
            error_class::STORAGE_CORRUPT,
            format!("failed to read the durable run index: {error:#}"),
        )
    })
}

/// Require the exact run id/path pair that transactionally commits publication.
/// SANITY proves the pack files were finalized; only this row proves that the
/// publication transaction (index + latest) committed as one durable truth.
pub(crate) fn require_published_run(run_dir: &Path, run_id: &str) -> Result<RunEntry, ToolError> {
    let index = strict_run_index()?;
    index
        .entries()
        .iter()
        .find(|entry| entry.id == run_id && same_run_path(&entry.path, run_dir))
        .cloned()
        .ok_or_else(|| {
            ToolError::new(
                error_class::RUN_FAILED,
                format!("run {run_id} finalized its pack but did not commit durable publication"),
            )
        })
}

/// Newest LIVE in-flight run for HEAD (by `started_at`).
///
/// Filters to `RunStatus::Running` (a live pid, no completion marker), exactly
/// what the `state` tool reports as "the current run", so `verdict` and `state`
/// agree. A stale marker or a durably completed run does not qualify here.
fn running_run_for_head(repo_name: &str, branch_key: &str, head: &str) -> Option<ResolvedRun> {
    let base = crate::config::prview_home()
        .join("runs")
        .join(repo_name)
        .join(branch_key);
    let mut best: Option<(String, ResolvedRun)> = None;
    for entry in std::fs::read_dir(&base).ok()?.flatten() {
        let run_dir = entry.path();
        if !run_dir.is_dir() || !matches!(run_status(&run_dir), RunStatus::Running { .. }) {
            continue;
        }
        let Some(marker) = read_running_marker(&run_dir) else {
            continue;
        };
        if !commit_matches(&marker.commit, head) {
            continue;
        }
        let Some(run_id) = run_dir.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let candidate = ResolvedRun {
            run_dir: run_dir.clone(),
            run_id: run_id.to_string(),
            commit: marker.commit.clone(),
        };
        let newer = best
            .as_ref()
            .map(|(started, _)| marker.started_at > *started)
            .unwrap_or(true);
        if newer {
            best = Some((marker.started_at.clone(), candidate));
        }
    }
    best.map(|(_, run)| run)
}

/// Pick between the newest indexed COMPLETED run and the newest LIVE in-flight
/// run for HEAD.
///
/// A live in-flight run wins whenever one exists: it is what `state` reports as
/// the current run, and `verdict` reports it as `in_progress` (so a poller keeps
/// polling) instead of stopping early on a stale completed pack from a prior run
/// on the same HEAD. With no live run, the indexed completed run is returned;
/// `None` only when neither exists.
fn choose_head_run(
    indexed: Option<ResolvedRun>,
    running: Option<ResolvedRun>,
) -> Option<ResolvedRun> {
    running.or(indexed)
}

/// Resolve a run for `verdict`/`findings`/`read_artifact`.
///
/// With `run_id`: look it up in the index (completed runs), else scan storage
/// for an in-flight deep run. Without: the latest run for the current HEAD
/// (R3), preferring a live in-flight run over a stale completed pack. A missing
/// run is a fail-loud `run_not_found` — the agent then calls `run_review`.
pub fn resolve_run(root: &Path, run_id: Option<&str>) -> Result<ResolvedRun, ToolError> {
    let repo_name = crate::config::repo_name_from_root(root);
    let index = strict_run_index()?;

    match run_id {
        Some(id) => {
            validate_run_id(id)?;
            let disk_run = find_run_dir_by_id(&repo_name, id)?;
            if let Some(e) = find_index_entry_by_id(&index, &repo_name, id)? {
                if let Some(ref run_dir) = disk_run
                    && !same_run_path(run_dir, &e.path)
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
            // The index only knows COMPLETED, registered runs; `state` reports the
            // live in-flight run. Prefer a live run for HEAD so a `verdict` poll
            // without a run_id does not stop early on a stale completed pack while a
            // fresh deep run on the same HEAD is still producing its own (A4).
            let indexed = latest_for_head(&index, &repo_name, &branch_key, &state.head).map(|e| {
                ResolvedRun {
                    run_dir: e.path.clone(),
                    run_id: e.id.clone(),
                    commit: e.commit.clone(),
                }
            });
            let running = running_run_for_head(&repo_name, &branch_key, &state.head);
            match choose_head_run(indexed, running) {
                Some(run) => Ok(run),
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
    pub enforcement_disposition: crate::policy::engine::EnforcementDisposition,
    pub blocking_issues: Vec<String>,
    pub caveats: Vec<String>,
    pub base_used: Vec<String>,
    pub normalized: bool,
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
    // A pack whose schema this build does not know cannot be normalized
    // honestly: an unknown MAJOR is fail-loud, a newer MINOR is tolerated but
    // carries a caveat. An absent `schema_version` is the documented pre-2.1
    // read-back surface and is accepted silently.
    let schema_caveat = crate::gate::check_merge_gate_schema_field(value.get("schema_version"))
        .map_err(|e| ToolError::new(error_class::STORAGE_CORRUPT, e.to_string()))?;
    let enforcement_required =
        crate::gate::schema_requires_enforcement_disposition(value.get("schema_version"));

    // A pack with no `schema_version` predates the field and its ROOT is the
    // decision — the same legacy tolerance the CLI reader and the contract keep.
    // Demanding a nested `decision` at every version made the one shape the
    // contract explicitly tolerates come back `storage_corrupt` from this
    // surface while the CLI read it fine.
    let decision = crate::gate::select_decision_object(&value).map_err(|shape| {
        ToolError::new(
            error_class::STORAGE_CORRUPT,
            format!("MERGE_GATE.json {}", shape.describe()),
        )
    })?;

    // A field present with the wrong JSON type is NOT an absent field. Reading
    // it through `as_str()` / `as_bool()` collapsed the two, and "absent" is the
    // one state the adapter accepts in silence — so `merge_recommendation: 7`
    // beside a valid verdict produced a confident passthrough that had quietly
    // ignored a signal. Keep the two apart and name what was ignored.
    let mut unknown_signal_caveats = Vec::new();

    let raw_merge = readable_signal(
        "merge_recommendation",
        decision.get("merge_recommendation"),
        JsonKind::String,
        &mut unknown_signal_caveats,
    )
    .and_then(|v| v.as_str())
    .map(str::to_string);
    let raw_verdict = readable_signal(
        "verdict",
        decision.get("verdict"),
        JsonKind::String,
        &mut unknown_signal_caveats,
    )
    .and_then(|v| v.as_str())
    .map(str::to_string);
    let raw_allow = readable_signal(
        "allow_merge",
        decision.get("allow_merge"),
        JsonKind::Boolean,
        &mut unknown_signal_caveats,
    )
    .and_then(|v| v.as_bool());
    // Read here rather than next to its rank below, so that a mistyped value
    // reaches `mistyped_signal` on the line after this one. A bare `as_bool()`
    // read `quality_pass: "false"` as absent: no caveat, no rank, and the
    // surface answered `approve` while the pack said the quality axis failed.
    let raw_quality_pass = readable_signal(
        "quality_pass",
        decision.get("quality_pass"),
        JsonKind::Boolean,
        &mut unknown_signal_caveats,
    )
    .and_then(|v| v.as_bool());
    // The confidence and blocker axes, mirroring the CLI. All three are read
    // here so a mistyped one reaches `mistyped_signal` below; `blocking_issues`
    // was previously read only at the very end, for passthrough, so a stated
    // blocker never touched the decision this adapter returned.
    let raw_analysis_status = readable_signal(
        "analysis_status",
        decision.get("analysis_status"),
        JsonKind::String,
        &mut unknown_signal_caveats,
    )
    .and_then(|v| v.as_str().map(str::to_string));
    let raw_policy_allow_merge = readable_signal(
        "policy_allow_merge",
        decision.get("policy_allow_merge"),
        JsonKind::Boolean,
        &mut unknown_signal_caveats,
    )
    .and_then(|v| v.as_bool());
    let raw_blocking_issues = readable_signal(
        "blocking_issues",
        decision.get("blocking_issues"),
        JsonKind::Array,
        &mut unknown_signal_caveats,
    )
    .and_then(|v| v.as_array())
    .map(|issues| issues.len());
    let mut enforcement_caveats = Vec::new();
    let stated_enforcement_disposition = crate::gate::read_enforcement_disposition(
        decision.get("enforcement_disposition"),
        enforcement_required,
        &mut enforcement_caveats,
    );
    let mut check_caveats = Vec::new();
    let warning_tally = crate::gate::read_pack_warning_tally(
        value.get("checks"),
        value.get("inline_findings"),
        value.get("policy"),
        decision.get("quality_failure_details"),
        enforcement_required,
        &mut check_caveats,
    );

    // Whether any signal was present and could not be TYPED. Captured before
    // the vocabulary caveats below join the same list, because the two are
    // different failures with the same consequence.
    let mistyped_signal = !unknown_signal_caveats.is_empty();

    let merge_rank = raw_merge.as_deref().and_then(rank_from_merge_rec);
    let verdict_rank = raw_verdict.as_deref().and_then(rank_from_verdict);

    // A present-but-unrecognized signal used to vanish into the `flatten()`
    // below, so the caller saw a confident surface derived from the OTHER
    // signal with no hint that a field had been dropped. Record it instead:
    // the value is still not used to rank, but the reader stops pretending it
    // read the pack cleanly. Legacy `ALLOW`/`HOLD` are recognized vocabulary,
    // so they never land here.
    if let Some(raw) = raw_verdict.as_deref()
        && verdict_rank.is_none()
    {
        unknown_signal_caveats.push(format!(
            "unknown_verdict: MERGE_GATE.json verdict `{raw}` is not in the \
             PASS/CONDITIONAL/BLOCK vocabulary; normalized to BLOCK"
        ));
    }
    // An absent verdict is named too, exactly as the CLI names it: the decision
    // this adapter returns is then the reader's substitution, not the pack's.
    if raw_verdict.is_none() && decision.get("verdict").is_none() {
        unknown_signal_caveats.push(
            "unknown_verdict: MERGE_GATE.json decision carries no `verdict`; normalized to BLOCK"
                .to_string(),
        );
    }
    if let Some(raw) = raw_merge.as_deref()
        && merge_rank.is_none()
    {
        unknown_signal_caveats.push(format!(
            "unknown_merge_recommendation: MERGE_GATE.json merge_recommendation `{raw}` is not in \
             the approve/review_required/block vocabulary; it was ignored when deriving this \
             decision"
        ));
    }
    if let Some(raw) = raw_analysis_status.as_deref()
        && !crate::gate::known_analysis_status(raw)
    {
        unknown_signal_caveats.push(format!(
            "unknown_analysis_status: MERGE_GATE.json analysis_status `{raw}` is not in the \
             complete/degraded/incomplete vocabulary; it was ignored when deriving this decision"
        ));
    }

    // Corrupt means the decision states NOTHING — not that what it states is
    // unrankable. The presence test is the CLI's, field for field: a pack that
    // named a verdict outside the vocabulary, or nothing but `allow_merge`, DID
    // state a decision, and calling it corrupt here while the CLI published a
    // summary for it left the same artifact readable on one surface and broken
    // on the other.
    if !["verdict", "merge_recommendation", "allow_merge"]
        .iter()
        .any(|field| decision.get(*field).is_some())
    {
        return Err(ToolError::new(
            error_class::STORAGE_CORRUPT,
            "MERGE_GATE.json decision states no verdict, merge_recommendation or allow_merge",
        ));
    }

    // allow_merge=false raises conservativeness to at least HOLD; allow=true
    // never lowers it (a permissive flag can't override a block/hold signal).
    let allow_rank = raw_allow.map(|allow| if allow { 1 } else { 2 });
    // `quality_pass: false` says "not a PASS" — the contract permits `PASS` only
    // when quality passes — so it ranks 2, like `allow_merge: false`. `true`
    // states no rank of its own: a quality-clean run is still held at
    // CONDITIONAL by a breaking-change escalation. Absence states nothing
    // either, so a pack written before the field reads exactly as it always did.
    // This is the CLI's rule, mirrored, because a pack this adapter approved
    // while the CLI held it is the same split the reader parity work closed.
    // (Read above, with the other typed signals.)
    let quality_rank = (raw_quality_pass == Some(false)
        || warning_tally.has_new_quality_failure_signal)
        .then_some(2);
    if warning_tally.has_new_quality_failure_signal && raw_quality_pass != Some(false) {
        unknown_signal_caveats.push(
            "quality_failure_inconsistency: typed introduced/mixed/unclassified failure details \
             contradict quality_pass; normalized to false"
                .to_string(),
        );
    }
    // Same rule on the confidence axis: only `degraded`/`incomplete` rule `PASS`
    // out and therefore rank. `complete` is a precondition of `PASS`, not a
    // grant of it, so it stays silent like `quality_pass: true`.
    let analysis_rank = raw_analysis_status
        .as_deref()
        .and_then(crate::gate::rank_from_analysis_status);
    // A stated blocker is a stated BLOCK: `blocking_issues` is non-empty only
    // when a check reached `PolicyConclusion::Blocked`, whose `merge_impact` is
    // `Block`. `policy_allow_merge: false` is the same fact — the emitter writes
    // `policy_allow_merge = blocking_issues.is_empty()`. Neither says anything
    // permissive: "policy did not hard-block" is explicitly NOT `allow_merge`.
    let blocker_rank = (raw_policy_allow_merge == Some(false)
        || raw_blocking_issues.is_some_and(|len| len > 0)
        || warning_tally.has_blocking_signal)
        .then_some(3);
    let check_review_rank = warning_tally.has_review_signal.then_some(2);

    // A verdict this reader had to SUBSTITUTE — absent, outside the vocabulary,
    // or present with the wrong JSON type — governs everything derived beside
    // it, and so does any other signal that could not be typed. This is the
    // CLI's `normalized_to_block` rule, mirrored: a decision derived from a
    // block the reader only partly read is not one either surface may publish
    // as permissive, and the two surfaces answering that differently is how one
    // pack came to be a `PASS` for MCP automation and a `BLOCK` on the CLI.
    let normalized_to_block = mistyped_signal || verdict_rank.is_none();

    let stated_ranks: Vec<u8> = [
        merge_rank,
        verdict_rank,
        allow_rank,
        quality_rank,
        analysis_rank,
        check_review_rank,
        blocker_rank,
    ]
    .into_iter()
    .flatten()
    .collect();
    let final_rank = if normalized_to_block {
        3
    } else {
        stated_ranks.iter().copied().max().unwrap_or(3)
    };

    let allow_merge = final_rank == 1;
    let mut enforcement_disposition = stated_enforcement_disposition.unwrap_or(match final_rank {
        1 => crate::policy::engine::EnforcementDisposition::Clean,
        2 => crate::policy::engine::EnforcementDisposition::ReviewRequired,
        _ => crate::policy::engine::EnforcementDisposition::Block,
    });
    if final_rank == 3 {
        enforcement_disposition.raise_to(crate::policy::engine::EnforcementDisposition::Block);
    } else if raw_quality_pass == Some(false)
        || analysis_rank.is_some()
        || check_review_rank.is_some()
        || blocker_rank.is_some()
        || (final_rank == 2
            && enforcement_disposition == crate::policy::engine::EnforcementDisposition::Clean)
    {
        if check_review_rank.is_some()
            && enforcement_disposition
                < crate::policy::engine::EnforcementDisposition::ReviewRequired
        {
            unknown_signal_caveats.push(
                "check_enforcement_inconsistency: a typed check requires review but the stored \
                 enforcement_disposition was more permissive; normalized to review_required"
                    .to_string(),
            );
        }
        enforcement_disposition
            .raise_to(crate::policy::engine::EnforcementDisposition::ReviewRequired);
    }
    if warning_tally.has_unreadable_signal {
        enforcement_disposition
            .raise_to(crate::policy::engine::EnforcementDisposition::ReviewRequired);
    } else if enforcement_required
        && enforcement_disposition == crate::policy::engine::EnforcementDisposition::WarningsOnly
        && !warning_tally.has_explicit_warnings
    {
        unknown_signal_caveats.push(
            "unproven_warnings_only: MERGE_GATE.json schema 2.3+ states warnings_only without a \
             typed warning fact; normalized to review_required"
                .to_string(),
        );
        enforcement_disposition
            .raise_to(crate::policy::engine::EnforcementDisposition::ReviewRequired);
    } else if warning_tally.has_explicit_warnings
        && enforcement_disposition == crate::policy::engine::EnforcementDisposition::Clean
    {
        if stated_enforcement_disposition
            == Some(crate::policy::engine::EnforcementDisposition::Clean)
        {
            unknown_signal_caveats.push(
                "enforcement_inconsistency: MERGE_GATE.json has typed warning facts beside a clean \
                 enforcement_disposition; warnings-clean enforcement uses the canonical tally"
                    .to_string(),
            );
        }
        enforcement_disposition
            .raise_to(crate::policy::engine::EnforcementDisposition::WarningsOnly);
    }

    // Only the PACK's own axes can be inconsistent with each other; a verdict
    // this reader substituted is already named by its own caveat, and calling
    // the substitution an inconsistency would blame the artifact for the
    // reader's normalization.
    //
    // `allow_merge` raises the rank but is not compared as one: `false` says
    // "not a PASS" — `>= 2`, never 3 — so treating it as an exact rank would
    // call every healthy BLOCK pack inconsistent with itself. It contradicts the
    // decision only when the DERIVED flag disagrees with the stated one.
    let textual_ranks: Vec<u8> = [merge_rank, verdict_rank].into_iter().flatten().collect();
    let signals_disagree =
        !normalized_to_block && textual_ranks.iter().any(|&rank| rank != final_rank);
    let allow_contradicts =
        !normalized_to_block && raw_allow.map(|a| a != allow_merge).unwrap_or(false);
    // An ignored signal is itself a normalization: the returned decision is not
    // a faithful passthrough of what the pack says. A forward schema is the same
    // situation one level up — the pack was written by a build this one does not
    // fully know, so the read is best-effort and the caveat must be backed by the
    // flag consumers actually branch on.
    let normalized = signals_disagree
        || allow_contradicts
        || !unknown_signal_caveats.is_empty()
        || !enforcement_caveats.is_empty()
        || !check_caveats.is_empty()
        || schema_caveat.is_some();

    let mut caveats = schema_caveat.into_iter().collect::<Vec<_>>();
    caveats.append(&mut enforcement_caveats);
    caveats.append(&mut check_caveats);
    caveats.append(&mut unknown_signal_caveats);
    if signals_disagree || allow_contradicts {
        caveats.push(format!(
            "core_inconsistency: original allow_merge={}, merge_recommendation={}, verdict={}, \
             quality_pass={}, analysis_status={}, blocking_issues={}, policy_allow_merge={}",
            raw_allow
                .map(|b| b.to_string())
                .unwrap_or_else(|| "null".to_string()),
            raw_merge.as_deref().unwrap_or("null"),
            raw_verdict.as_deref().unwrap_or("null"),
            raw_quality_pass
                .map(|b| b.to_string())
                .unwrap_or_else(|| "null".to_string()),
            raw_analysis_status.as_deref().unwrap_or("null"),
            raw_blocking_issues
                .map(|len| len.to_string())
                .unwrap_or_else(|| "null".to_string()),
            raw_policy_allow_merge
                .map(|b| b.to_string())
                .unwrap_or_else(|| "null".to_string()),
        ));
    }
    caveats.extend(string_array(decision.get("review_caveats")));

    Ok(NormalizedDecision {
        merge_recommendation: merge_rec_from_rank(final_rank).to_string(),
        allow_merge,
        verdict: verdict_from_rank(final_rank).to_string(),
        enforcement_disposition,
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

/// Read a run's per-gate check summary from `MERGE_GATE.json`. The MCP row is a
/// lossless projection of the policy state needed by an agent; it does not
/// collapse execution, finding, confidence and merge impact into `status`.
/// Empty when the file or `checks` array is absent.
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
        .cloned()
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
    fn read_gates_preserves_policy_axes() {
        let dir = tempfile::tempdir().unwrap();
        let row = serde_json::json!({
            "id": "semgrep_scan",
            "name": "Semgrep scan",
            "status": "warnings",
            "execution_state": "executed",
            "outcome": "findings_warning",
            "class": "INFO",
            "severity": "warn",
            "policy_conclusion": "advisory",
            "blocking": false,
            "merge_impact": "review_required",
            "confidence_impact": "degraded",
            "duration_secs": 1.25,
            "cached": false,
            "reason": "partial parse",
            "evidence": "20_quality/semgrep_scan.result.json",
            "log": "20_quality/semgrep_scan.log"
        });
        write_gate(
            dir.path(),
            &serde_json::json!({
                "checks": [row.clone()]
            }),
        );

        let gates = read_gates(dir.path());
        assert_eq!(gates, vec![row]);
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
    fn unknown_verdict_is_reported_as_normalized_with_caveat() {
        // An unrecognized verdict used to vanish into the rank `flatten()`: the
        // decision was derived from `merge_recommendation` alone and returned as
        // a clean passthrough, with nothing telling the caller a field had been
        // dropped. It must surface as an explicit `unknown_verdict` caveat — and
        // it must also govern the decision published beside it. Deriving a PASS
        // from the surviving `approve` was this adapter reading one pack as an
        // approval while the CLI, on the same bytes, substituted BLOCK.
        let dir = tempfile::tempdir().unwrap();
        write_gate(
            dir.path(),
            &serde_json::json!({
                "bases": ["main"],
                "decision": {
                    "merge_recommendation": "approve",
                    "verdict": "MAYBE",
                    "allow_merge": true
                }
            }),
        );
        let d = read_decision(dir.path()).unwrap();
        assert_eq!(
            d.verdict, "BLOCK",
            "a substituted verdict governs every axis derived beside it"
        );
        assert_eq!(d.merge_recommendation, "block");
        assert!(
            !d.allow_merge,
            "the pack's `allow_merge: true` does not stand"
        );
        assert!(d.normalized, "an ignored signal is a normalization");
        let caveat = d
            .caveats
            .iter()
            .find(|c| c.starts_with("unknown_verdict:"))
            .expect("unknown_verdict caveat present");
        assert!(caveat.contains("MAYBE"), "{caveat}");
    }

    #[test]
    fn unknown_merge_recommendation_is_reported_with_caveat() {
        let dir = tempfile::tempdir().unwrap();
        write_gate(
            dir.path(),
            &serde_json::json!({
                "bases": ["main"],
                "decision": {
                    "merge_recommendation": "probably_fine",
                    "verdict": "BLOCK",
                    "allow_merge": false
                }
            }),
        );
        let d = read_decision(dir.path()).unwrap();
        assert_eq!(d.verdict, "BLOCK");
        assert!(d.normalized);
        assert!(
            d.caveats
                .iter()
                .any(|c| c.starts_with("unknown_merge_recommendation:")),
            "caveats: {:?}",
            d.caveats
        );
    }

    #[test]
    fn legacy_verdict_synonyms_raise_no_unknown_verdict_caveat() {
        // The documented pre-2.1 tolerance is a safety net, not a hole: ALLOW and
        // HOLD are recognized vocabulary and must never be reported as unknown.
        for verdict in ["ALLOW", "HOLD"] {
            let dir = tempfile::tempdir().unwrap();
            write_gate(
                dir.path(),
                &serde_json::json!({
                    "bases": ["main"],
                    "decision": { "verdict": verdict, "allow_merge": verdict == "ALLOW" }
                }),
            );
            let d = read_decision(dir.path()).unwrap();
            assert!(
                !d.caveats.iter().any(|c| c.starts_with("unknown_verdict:")),
                "legacy `{verdict}` must stay tolerated: {:?}",
                d.caveats
            );
        }
    }

    #[test]
    fn unknown_schema_major_fails_loud() {
        let dir = tempfile::tempdir().unwrap();
        write_gate(
            dir.path(),
            &serde_json::json!({
                "schema_version": "9.0",
                "bases": ["main"],
                "decision": { "verdict": "PASS", "allow_merge": true }
            }),
        );
        let err = read_decision(dir.path()).expect_err("unknown major must fail loud");
        assert_eq!(err.class, error_class::STORAGE_CORRUPT);
        assert!(err.message.contains("9.0"), "{}", err.message);
    }

    #[test]
    fn newer_schema_minor_is_tolerated_with_caveat() {
        let dir = tempfile::tempdir().unwrap();
        write_gate(
            dir.path(),
            &serde_json::json!({
                "schema_version": "2.9",
                "bases": ["main"],
                "decision": { "verdict": "PASS", "merge_recommendation": "approve", "allow_merge": true }
            }),
        );
        let d = read_decision(dir.path()).expect("newer minor is readable");
        assert_eq!(d.verdict, "PASS");
        assert!(
            d.caveats
                .iter()
                .any(|c| c.starts_with("schema_forward_compat:")),
            "caveats: {:?}",
            d.caveats
        );
    }

    #[test]
    fn forward_schema_read_is_marked_normalized() {
        // docs/mcp.md: "Anything the adapter could not read is named rather than
        // dropped, and every such case sets `normalized: true`" — and it lists
        // `schema_forward_compat:` among them. A caveat next to
        // `normalized: false` tells the client the decision was passed through
        // unchanged while simultaneously admitting fields were ignored.
        let dir = tempfile::tempdir().unwrap();
        write_gate(
            dir.path(),
            &serde_json::json!({
                "schema_version": "2.9",
                "bases": ["main"],
                "decision": { "verdict": "PASS", "merge_recommendation": "approve", "allow_merge": true }
            }),
        );
        let d = read_decision(dir.path()).expect("newer minor is readable");
        assert!(
            d.caveats
                .iter()
                .any(|c| c.starts_with("schema_forward_compat:")),
            "caveats: {:?}",
            d.caveats
        );
        assert!(
            d.normalized,
            "a forward-schema read ignored unknown fields; that is a normalization"
        );
    }

    #[test]
    fn non_string_schema_version_is_storage_corrupt() {
        // Same defect as the CLI reader: `as_str()` turned a present-but-
        // wrongly-typed field into "absent", which is the silently-accepted
        // legacy path.
        for bad in [
            serde_json::json!(2.1),
            serde_json::json!(null),
            serde_json::json!({ "major": 2 }),
        ] {
            let dir = tempfile::tempdir().unwrap();
            write_gate(
                dir.path(),
                &serde_json::json!({
                    "schema_version": bad,
                    "bases": ["main"],
                    "decision": { "verdict": "PASS", "allow_merge": true }
                }),
            );
            let err = read_decision(dir.path()).expect_err("non-string schema must fail loud");
            assert_eq!(err.class, error_class::STORAGE_CORRUPT);
            assert!(
                err.message.contains("schema_version"),
                "{} for {bad}",
                err.message
            );
        }
    }

    #[test]
    fn a_legacy_root_shaped_pack_is_read_not_called_corrupt() {
        // A pack with no `schema_version` predates the field, and reading its
        // ROOT as the decision is the documented legacy read-back surface — the
        // CLI reader and `docs/contracts/merge_gate.md` both keep it. This
        // adapter demanded a nested `decision` object at every version, so the
        // one pack shape the contract explicitly tolerates came back
        // `storage_corrupt`.
        let dir = tempfile::tempdir().unwrap();
        write_gate(
            dir.path(),
            &serde_json::json!({ "verdict": "ALLOW", "allow_merge": true }),
        );

        let d = read_decision(dir.path()).expect("a legacy root-shaped pack is readable");
        assert_eq!(d.verdict, "PASS");
        assert!(d.allow_merge, "{d:?}");
    }

    #[test]
    fn a_non_object_gate_root_is_corrupt_on_both_readers() {
        // The legacy root tolerance covers a pack whose decision fields sit at
        // the root — not a pack that is an array, a scalar or `null`. Those
        // carry no fields to read, and the two readers must agree they are
        // corrupt rather than one of them inventing a normalized BLOCK.
        for root in ["[1,2,3]", "\"BLOCK\"", "null", "7"] {
            let dir = tempfile::tempdir().unwrap();
            std::fs::create_dir_all(dir.path().join("00_summary")).unwrap();
            std::fs::write(dir.path().join("00_summary/MERGE_GATE.json"), root).unwrap();

            let err = read_decision(dir.path()).expect_err("a non-object gate root is corrupt");
            assert_eq!(err.class, error_class::STORAGE_CORRUPT);
            assert!(err.message.contains("not a JSON object"), "{}", err.message);
        }
    }

    #[test]
    fn a_versioned_pack_without_a_decision_object_stays_corrupt() {
        // The other half of the same rule: once a pack names its schema, the
        // object that schema is built around is mandatory. Reading the root
        // there would publish a verdict nothing in the pack stated.
        for decision in [None, Some(serde_json::json!("PASS"))] {
            let dir = tempfile::tempdir().unwrap();
            let mut gate = serde_json::json!({
                "schema_version": "2.2",
                "verdict": "ALLOW",
                "allow_merge": true
            });
            if let Some(decision) = decision.clone() {
                gate["decision"] = decision;
            }
            write_gate(dir.path(), &gate);

            let err = read_decision(dir.path())
                .expect_err("a versioned pack with no decision object is corrupt");
            assert_eq!(err.class, error_class::STORAGE_CORRUPT);
            assert!(err.message.contains("decision"), "{}", err.message);
        }
    }

    #[test]
    fn wrongly_typed_decision_signal_is_reported_not_dropped() {
        // `verdict: "PASS"` with `merge_recommendation: 7`: `as_str()` mapped the
        // malformed field onto "absent", so the unknown-signal branch never saw
        // it and the adapter returned a clean `normalized: false` decision
        // derived from the surviving signal — a pack field ignored in silence.
        let dir = tempfile::tempdir().unwrap();
        write_gate(
            dir.path(),
            &serde_json::json!({
                "bases": ["main"],
                "decision": { "verdict": "PASS", "merge_recommendation": 7, "allow_merge": true }
            }),
        );

        let d = read_decision(dir.path()).expect("one readable signal keeps the pack readable");
        assert!(
            d.caveats
                .iter()
                .any(|c| c.starts_with("unreadable_merge_recommendation:")),
            "the ignored field must be named: {:?}",
            d.caveats
        );
        assert!(
            d.normalized,
            "a decision that ignored a field is not a passthrough"
        );
    }

    #[test]
    fn wrongly_typed_allow_merge_is_reported_not_dropped() {
        // Same defect on the third signal: a non-boolean `allow_merge` was
        // dropped by `as_bool()` and the conservativeness it should have raised
        // simply disappeared.
        let dir = tempfile::tempdir().unwrap();
        write_gate(
            dir.path(),
            &serde_json::json!({
                "bases": ["main"],
                "decision": {
                    "verdict": "PASS",
                    "merge_recommendation": "approve",
                    "allow_merge": "false"
                }
            }),
        );

        let d = read_decision(dir.path()).expect("both ranked signals are readable");
        assert!(
            d.caveats
                .iter()
                .any(|c| c.starts_with("unreadable_allow_merge:")),
            "the ignored field must be named: {:?}",
            d.caveats
        );
        assert!(d.normalized);
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
        let home = tempfile::tempdir().unwrap();
        let _home = crate::config::override_test_prview_home(home.path().to_path_buf());
        let dir = tempfile::tempdir().unwrap();
        // Both a lingering live marker AND a finalized, durably indexed pack:
        // publication wins.
        write_marker(dir.path(), std::process::id());
        let summary = dir.path().join("00_summary");
        std::fs::create_dir_all(&summary).unwrap();
        std::fs::write(summary.join("RUN.json"), "{}").unwrap();
        std::fs::write(summary.join("SANITY.json"), "{}").unwrap();
        let run_id = dir.path().file_name().unwrap().to_str().unwrap();
        let mut published = entry(run_id, "abc1234", "2026-07-01T12:00:00Z");
        published.path = dir.path().to_path_buf();
        std::fs::write(
            home.path().join("index.jsonl"),
            format!("{}\n", serde_json::to_string(&published).unwrap()),
        )
        .unwrap();
        assert_eq!(run_status(dir.path()), RunStatus::Completed);
    }

    #[test]
    fn finalized_but_unindexed_pack_is_never_completed() {
        let home = tempfile::tempdir().unwrap();
        let _home = crate::config::override_test_prview_home(home.path().to_path_buf());
        let dir = tempfile::tempdir().unwrap();
        let summary = dir.path().join("00_summary");
        std::fs::create_dir_all(&summary).unwrap();
        std::fs::write(summary.join("SANITY.json"), "{}").unwrap();

        write_marker(dir.path(), std::process::id());
        assert_eq!(
            run_status(dir.path()),
            RunStatus::Running {
                pid: std::process::id()
            }
        );
        assert_eq!(
            require_published_run(dir.path(), "unpublished")
                .unwrap_err()
                .class,
            error_class::RUN_FAILED
        );

        std::fs::remove_file(running_marker_path(dir.path())).unwrap();
        write_marker(dir.path(), 2_147_483_646);
        assert!(matches!(run_status(dir.path()), RunStatus::Stale { .. }));
    }

    #[test]
    fn corrupt_index_is_fail_loud_for_mcp_publication_truth() {
        let home = tempfile::tempdir().unwrap();
        let _home = crate::config::override_test_prview_home(home.path().to_path_buf());
        std::fs::write(home.path().join("index.jsonl"), "{ not valid json\n").unwrap();
        let run_dir = home.path().join("runs/demo/main/run");
        std::fs::create_dir_all(&run_dir).unwrap();

        assert_eq!(
            require_published_run(&run_dir, "run").unwrap_err().class,
            error_class::STORAGE_CORRUPT
        );
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

    /// mcp-5 delivery-verifier (b): a `RUNNING.json` with pid 0 (the unknown-pid
    /// sentinel) must never read as an immortal `Running`. `kill(0, 0)` targets
    /// the caller's whole process group and always succeeds, so pid 0 has to be
    /// special-cased as dead → the marker is `Stale`, not eternally alive.
    #[test]
    fn run_status_pid_zero_is_stale_not_running() {
        let dir = tempfile::tempdir().unwrap();
        write_marker(dir.path(), 0);
        match run_status(dir.path()) {
            RunStatus::Stale { pid, .. } => assert_eq!(pid, 0),
            other => panic!("pid 0 must be Stale, never Running; got {other:?}"),
        }
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
    fn find_index_entry_by_id_dedupes_same_run_path() {
        let id = "20260101-120000";
        let first = entry(id, "aaaa111", "2026-01-01T00:00:00Z");
        let duplicate = first.clone();
        let entries = vec![first, duplicate];
        let (_tmp, index) = index_from(&entries);

        let found = find_index_entry_by_id(&index, "demo", id).unwrap().unwrap();

        assert_eq!(found.id, id);
        assert_eq!(found.path, PathBuf::from(format!("/tmp/demo/main/{id}")));
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

    #[cfg(unix)]
    #[test]
    fn same_run_path_accepts_symlinked_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let real_root = dir.path().join("real-root");
        let run = real_root.join("demo/main/20260101-120000");
        std::fs::create_dir_all(&run).unwrap();
        let symlink_root = dir.path().join("symlink-root");
        std::os::unix::fs::symlink(&real_root, &symlink_root).unwrap();

        assert!(same_run_path(
            &symlink_root.join("demo/main/20260101-120000"),
            &run
        ));
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
    fn choose_head_run_prefers_live_in_flight_over_indexed() {
        let indexed = ResolvedRun {
            run_dir: PathBuf::from("/runs/completed"),
            run_id: "completed".to_string(),
            commit: "aaaa111".to_string(),
        };
        let running = ResolvedRun {
            run_dir: PathBuf::from("/runs/in-flight"),
            run_id: "in-flight".to_string(),
            commit: "aaaa111".to_string(),
        };

        // A fresh in-flight run on the same HEAD wins over the stale completed
        // pack — verdict then reports in_progress instead of stopping the poller.
        let chosen = choose_head_run(Some(indexed.clone()), Some(running.clone())).unwrap();
        assert_eq!(chosen.run_id, "in-flight");

        // With no live run, the indexed completed run is the answer.
        let only_completed = choose_head_run(Some(indexed.clone()), None).unwrap();
        assert_eq!(only_completed.run_id, "completed");

        // With only a live run, it is returned.
        let only_running = choose_head_run(None, Some(running)).unwrap();
        assert_eq!(only_running.run_id, "in-flight");

        // Neither → None; an unindexed finalized directory is not completion.
        assert!(choose_head_run(None, None).is_none());
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
