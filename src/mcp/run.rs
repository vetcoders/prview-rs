//! `run_review`: spawn the prview binary to produce a review pack.
//!
//! The MCP layer adds no review logic — it prepares a run directory, spawns
//! `prview` (its own binary) as a subprocess, and reads the resulting pack from
//! storage. quick waits synchronously within a hard 120s budget; deep detaches
//! and is polled later through `verdict`/`state`. A single active run per repo
//! branch is enforced via the `RUNNING.json` liveness marker (R2b).

use crate::mcp::read;
use crate::mcp::types::{ToolError, error_class};
use std::fs::File;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Review depth requested by the caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Profile {
    Quick,
    Deep,
}

impl Profile {
    /// Parse the tool argument; default quick, unknown value is fail-loud.
    pub fn parse(value: Option<&str>) -> Result<Self, ToolError> {
        match value {
            None | Some("quick") => Ok(Profile::Quick),
            Some("deep") => Ok(Profile::Deep),
            Some(other) => Err(ToolError::new(
                error_class::RUN_FAILED,
                format!("unknown profile '{other}'; expected 'quick' or 'deep'"),
            )),
        }
    }

    fn cli_flag(self) -> &'static str {
        match self {
            Profile::Quick => "--quick",
            Profile::Deep => "--deep",
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Profile::Quick => "quick",
            Profile::Deep => "deep",
        }
    }
}

/// Default sync quick budget. 120s comes from 0.4.0 Codescribe/Vista dogfood:
/// the previous 60s budget timed out repeatedly on a medium-large (~411k LOC)
/// repo while keeping quick synchronous remains the approved product contract.
const DEFAULT_QUICK_BUDGET: Duration = Duration::from_secs(120);
const FALLBACK_BASES: &[&str] = &["develop", "main", "master"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BaseSelection {
    pub bases: Vec<String>,
    pub base_fallback: bool,
    pub caveats: Vec<String>,
}

fn short_head(repo: &Path) -> String {
    crate::git::git_cmd()
        .args(["rev-parse", "--short", "HEAD"])
        .current_dir(repo)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

fn quick_budget() -> Duration {
    std::env::var("PRVIEW_MCP_QUICK_BUDGET_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|millis| *millis > 0)
        .map(Duration::from_millis)
        .unwrap_or(DEFAULT_QUICK_BUDGET)
}

/// Allocate a fresh, collision-free run directory under the standard storage
/// layout so a later `verdict(run_id)` scan can find an in-flight deep run.
fn allocate_run_dir(repo_name: &str, branch_key: &str) -> Result<(PathBuf, String), ToolError> {
    let repo_runs_root = crate::config::prview_home().join("runs").join(repo_name);
    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S").to_string();
    allocate_run_dir_in(&repo_runs_root, branch_key, &stamp)
}

/// Exclusive, race-free allocation of a run directory (PR #12 review).
///
/// `create_dir` is atomic and fails with `AlreadyExists` when the leaf already
/// exists, unlike `create_dir_all`, which succeeds silently. That closes the
/// TOCTOU window where two concurrent `run_review` calls pick the same
/// timestamp directory and both write into it, corrupting a single pack — the
/// loser now bumps to a fresh suffixed id instead of clobbering the winner.
fn allocate_run_dir_in(
    repo_runs_root: &Path,
    branch_key: &str,
    stamp: &str,
) -> Result<(PathBuf, String), ToolError> {
    let base = repo_runs_root.join(branch_key);
    std::fs::create_dir_all(&base).map_err(|e| {
        ToolError::new(
            error_class::RUN_FAILED,
            format!("failed to create runs dir {}: {e}", base.display()),
        )
    })?;

    let mut suffix = 2u32;
    let mut run_id = stamp.to_string();
    loop {
        // Spec 4a: run_id must be globally unique within the repo storage, not
        // just within a branch (PR #12 review). Reject an id already taken under
        // ANY branch before claiming it, so verdict/read_artifact by explicit
        // run_id never resolve to a same-second run from a different branch.
        if !run_id_taken_in_repo(repo_runs_root, &run_id) {
            let run_dir = base.join(&run_id);
            match std::fs::create_dir(&run_dir) {
                Ok(()) => return Ok((run_dir, run_id)),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(e) => {
                    return Err(ToolError::new(
                        error_class::RUN_FAILED,
                        format!("failed to create run dir {}: {e}", run_dir.display()),
                    ));
                }
            }
        }
        run_id = format!("{stamp}-{suffix}");
        suffix += 1;
        if suffix > 10_000 {
            return Err(ToolError::new(
                error_class::RUN_FAILED,
                "exhausted run-id suffixes allocating a run directory".to_string(),
            ));
        }
    }
}

/// True when `run_id` already names a directory under any `runs/<repo>/<branch>`
/// subtree — the repo-global uniqueness probe for `allocate_run_dir_in`.
fn run_id_taken_in_repo(repo_runs_root: &Path, run_id: &str) -> bool {
    let Ok(read) = std::fs::read_dir(repo_runs_root) else {
        return false;
    };
    for branch in read.flatten() {
        let bp = branch.path();
        if bp.is_dir() && bp.join(run_id).exists() {
            return true;
        }
    }
    false
}

/// Detect a currently active run on this repo branch (live RUNNING marker).
fn active_run(repo_name: &str, branch_key: &str) -> Option<String> {
    let base = crate::config::prview_home()
        .join("runs")
        .join(repo_name)
        .join(branch_key);
    let read = std::fs::read_dir(&base).ok()?;
    for entry in read.flatten() {
        let dir = entry.path();
        if dir.is_dir()
            && matches!(read::run_status(&dir), read::RunStatus::Running { .. })
            && let Some(id) = dir.file_name().and_then(|n| n.to_str())
        {
            return Some(id.to_string());
        }
    }
    None
}

fn write_marker(run_dir: &Path, marker: &read::RunningMarker) {
    if let Ok(text) = serde_json::to_string_pretty(marker) {
        let _ = std::fs::write(read::running_marker_path(run_dir), text);
    }
}

fn normalize_origin_branch(value: &str) -> Option<String> {
    let trimmed = value.trim();
    let branch = trimmed
        .strip_prefix("refs/remotes/origin/")
        .or_else(|| trimmed.strip_prefix("origin/"))
        .unwrap_or(trimmed);
    if branch.is_empty() || branch == "HEAD" {
        None
    } else {
        Some(branch.to_string())
    }
}

fn origin_head_branch(repo: &Path) -> Option<String> {
    let out = crate::git::git_cmd()
        .args([
            "symbolic-ref",
            "--quiet",
            "--short",
            "refs/remotes/origin/HEAD",
        ])
        .current_dir(repo)
        .output()
        .ok()?;
    if out.status.success() {
        normalize_origin_branch(&String::from_utf8_lossy(&out.stdout))
    } else {
        None
    }
}

fn configured_origin_head(repo: &Path) -> Option<String> {
    let out = crate::git::git_cmd()
        .args(["config", "--get", "remote.origin.HEAD"])
        .current_dir(repo)
        .output()
        .ok()?;
    if out.status.success() {
        normalize_origin_branch(&String::from_utf8_lossy(&out.stdout))
    } else {
        None
    }
}

fn ref_exists(repo: &Path, name: &str) -> bool {
    let refs = if name.starts_with("refs/") {
        vec![name.to_string()]
    } else {
        vec![
            format!("refs/heads/{name}"),
            format!("refs/remotes/origin/{name}"),
        ]
    };

    refs.into_iter().any(|reference| {
        let mut cmd = crate::git::git_cmd();
        cmd.args([
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("{reference}^{{commit}}"),
        ])
        .current_dir(repo)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
        cmd.status().map(|s| s.success()).unwrap_or(false)
    })
}

pub(crate) fn select_bases(repo: &Path, base: Option<&str>) -> BaseSelection {
    if let Some(base) = base {
        return BaseSelection {
            bases: vec![base.to_string()],
            base_fallback: false,
            caveats: Vec::new(),
        };
    }

    if let Some(branch) = origin_head_branch(repo).or_else(|| configured_origin_head(repo)) {
        return BaseSelection {
            bases: vec![branch],
            base_fallback: false,
            caveats: Vec::new(),
        };
    }

    let bases: Vec<String> = FALLBACK_BASES
        .iter()
        .copied()
        .filter(|candidate| ref_exists(repo, candidate))
        .map(str::to_string)
        .collect();
    let bases = if bases.is_empty() {
        FALLBACK_BASES.iter().map(|s| s.to_string()).collect()
    } else {
        bases
    };

    BaseSelection {
        bases,
        base_fallback: true,
        caveats: vec![
            "base_fallback: default branch was not detectable; tried develop/main/master"
                .to_string(),
        ],
    }
}

/// Positional args for the child prview: `[branch, base]` when a base is given
/// (base is positional in the CLI, so target must precede it), else none.
fn positional_args(repo: &Path, selection: &BaseSelection) -> Vec<String> {
    let branch = crate::config::current_branch_name(repo).unwrap_or_else(|| "HEAD".to_string());
    let mut args = vec![branch];
    args.extend(selection.bases.iter().cloned());
    args
}

fn add_base_metadata(body: &mut serde_json::Value, selection: &BaseSelection) {
    body["base_fallback"] = serde_json::json!(selection.base_fallback);
    if selection.base_fallback {
        let mut caveats = body["caveats"].as_array().cloned().unwrap_or_default();
        caveats.extend(
            selection
                .caveats
                .iter()
                .cloned()
                .map(serde_json::Value::String),
        );
        body["caveats"] = serde_json::Value::Array(caveats);
    }
}

fn stdio_files(run_dir: &Path) -> Result<(File, File), ToolError> {
    let out = File::create(run_dir.join("run.log")).map_err(|e| {
        ToolError::new(
            error_class::RUN_FAILED,
            format!("cannot create run.log: {e}"),
        )
    })?;
    let err = File::create(run_dir.join("run.stderr.log")).map_err(|e| {
        ToolError::new(
            error_class::RUN_FAILED,
            format!("cannot create run.stderr.log: {e}"),
        )
    })?;
    Ok((out, err))
}

fn stderr_tail(run_dir: &Path) -> String {
    let text = std::fs::read_to_string(run_dir.join("run.stderr.log")).unwrap_or_default();
    let tail: Vec<&str> = text.lines().rev().take(20).collect();
    tail.into_iter().rev().collect::<Vec<_>>().join("\n")
}

/// Start a review. Returns the ready success body (without `schema_version`,
/// which the tool layer stamps).
pub async fn start(
    repo: &Path,
    base: Option<String>,
    profile: Profile,
) -> Result<serde_json::Value, ToolError> {
    let repo_name = crate::config::repo_name_from_root(repo);
    let branch_key = crate::config::storage_branch_key(repo);

    // R2b: one active run per repo branch.
    if let Some(active_run_id) = active_run(&repo_name, &branch_key) {
        return Err(ToolError::with_extra(
            error_class::STORAGE_LOCKED,
            "another review is already running for this repo branch",
            serde_json::json!({
                "active_run_id": active_run_id,
                "retry_after_ms": 5000,
            }),
        ));
    }

    let (run_dir, run_id) = allocate_run_dir(&repo_name, &branch_key)?;
    let commit = short_head(repo);
    let (out_file, err_file) = stdio_files(&run_dir)?;
    let selection = select_bases(repo, base.as_deref());

    let mut args: Vec<String> = vec![
        "--output-dir".to_string(),
        run_dir.to_string_lossy().to_string(),
        profile.cli_flag().to_string(),
    ];
    args.extend(positional_args(repo, &selection));

    match profile {
        Profile::Quick => {
            let mut cmd = tokio::process::Command::new(std::env::current_exe().map_err(|e| {
                ToolError::new(error_class::RUN_FAILED, format!("current_exe failed: {e}"))
            })?);
            cmd.current_dir(repo)
                .args(&args)
                .stdout(std::process::Stdio::from(out_file))
                .stderr(std::process::Stdio::from(err_file));
            // Shared rails: detached stdin, kill_on_drop, and (unix) own process
            // group so a timeout SIGKILLs the WHOLE tree (prview -> semgrep/cargo
            // grandchildren), not just the wrapper — kill_on_drop/start_kill reap
            // only the direct child (PR #12 review).
            crate::proc::harden(&mut cmd);

            let mut child = cmd.spawn().map_err(|e| {
                ToolError::new(error_class::RUN_FAILED, format!("spawn prview failed: {e}"))
            })?;
            // Capture the pid (also the pgid, since the child leads its group)
            // before the borrow in `child.wait()`; needed to signal the group.
            let child_pid = child.id();

            // Marker with the real child pid: on timeout it is left behind with a
            // now-dead pid, which reads as `stale` — fail-loud, not eternal running.
            write_marker(
                &run_dir,
                &read::RunningMarker {
                    pid: child_pid.unwrap_or(0),
                    started_at: chrono::Local::now().to_rfc3339(),
                    profile: profile.as_str().to_string(),
                    commit: commit.clone(),
                    base_used: selection.bases.clone(),
                },
            );

            let budget = quick_budget();
            match tokio::time::timeout(budget, child.wait()).await {
                Err(_) => {
                    // Kill the whole group first so the check-tool grandchildren
                    // die, then reap the direct child.
                    #[cfg(unix)]
                    if let Some(pid) = child_pid {
                        crate::proc::sigkill_process_group(pid);
                    }
                    let _ = child.start_kill();
                    Err(ToolError::with_extra(
                        error_class::RUN_TIMEOUT,
                        "quick review exceeded the configured budget; retry with profile=deep",
                        serde_json::json!({
                            "run_id": run_id,
                            "base_used": selection.bases,
                            "base_fallback": selection.base_fallback,
                            "caveats": selection.caveats,
                            "retry_hint": {
                                "profile": "deep",
                                "reason": "quick exceeded its synchronous budget"
                            }
                        }),
                    ))
                }
                Ok(Err(e)) => Err(ToolError::new(
                    error_class::RUN_FAILED,
                    format!("failed to wait on prview: {e}"),
                )),
                Ok(Ok(_status)) => {
                    // Success is defined by a finalized pack (SANITY.json, the last
                    // guaranteed finalization artifact), NOT the exit code: prview
                    // exits non-zero on a BLOCK verdict yet the run is a valid
                    // completed review.
                    if read::run_status(&run_dir) != read::RunStatus::Completed {
                        return Err(ToolError::with_extra(
                            error_class::RUN_FAILED,
                            "prview produced no completed pack",
                            serde_json::json!({
                                "run_id": run_id,
                                "stderr_tail": stderr_tail(&run_dir),
                            }),
                        ));
                    }
                    // Completed: the child already registered the run; drop the
                    // marker so status readers see a clean completion.
                    let _ = std::fs::remove_file(read::running_marker_path(&run_dir));
                    let mut body = completed_body(&run_dir, &run_id, &commit);
                    add_base_metadata(&mut body, &selection);
                    Ok(body)
                }
            }
        }
        Profile::Deep => {
            let pid = spawn_detached(repo, &args, out_file, err_file)?;
            write_marker(
                &run_dir,
                &read::RunningMarker {
                    pid,
                    started_at: chrono::Local::now().to_rfc3339(),
                    profile: profile.as_str().to_string(),
                    commit: commit.clone(),
                    base_used: selection.bases.clone(),
                },
            );
            let mut body = serde_json::json!({
                "run_id": run_id,
                "status": "running",
                "commit": commit,
                "base_used": selection.bases,
                "caveats": [],
            });
            add_base_metadata(&mut body, &selection);
            Ok(body)
        }
    }
}

/// Spawn a fully detached deep run (own process group on unix) and return its
/// pid. The MCP server does not wait on it — the handle is dropped so the child
/// keeps running independently.
fn spawn_detached(
    repo: &Path,
    args: &[String],
    out_file: File,
    err_file: File,
) -> Result<u32, ToolError> {
    let mut cmd = std::process::Command::new(std::env::current_exe().map_err(|e| {
        ToolError::new(error_class::RUN_FAILED, format!("current_exe failed: {e}"))
    })?);
    cmd.current_dir(repo)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::from(out_file))
        .stderr(std::process::Stdio::from(err_file));

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }

    let child = cmd.spawn().map_err(|e| {
        ToolError::new(
            error_class::RUN_FAILED,
            format!("spawn detached prview failed: {e}"),
        )
    })?;
    Ok(child.id())
}

/// Build the completed-run response body (quick sync path).
fn completed_body(run_dir: &Path, run_id: &str, commit: &str) -> serde_json::Value {
    let decision = read::read_decision(run_dir);
    let (verdict, merge_rec, allow_merge, base_used, blocking, caveats) = match &decision {
        Ok(d) => (
            d.verdict.clone(),
            d.merge_recommendation.clone(),
            d.allow_merge,
            d.base_used.clone(),
            d.blocking_issues.clone(),
            d.caveats.clone(),
        ),
        Err(_) => (
            "UNKNOWN".to_string(),
            "review_required".to_string(),
            false,
            read::read_bases(run_dir),
            Vec::new(),
            Vec::new(),
        ),
    };

    let (checks_passed, checks_failed, files_changed) = run_stats(run_id, run_dir);

    let mut artifact_paths = serde_json::json!({
        "pack": run_dir.to_string_lossy(),
        "merge_gate": "00_summary/MERGE_GATE.json",
    });
    if run_dir
        .join("30_context")
        .join("INLINE_FINDINGS.sarif")
        .exists()
    {
        artifact_paths["sarif"] = serde_json::json!("30_context/INLINE_FINDINGS.sarif");
    }
    // report.json is written at the pack ROOT (the sanity checker also expects
    // it there), not under 00_summary — so advertise the root path or MCP
    // clients cannot discover the machine-readable report (PR #12 review).
    if run_dir.join("report.json").exists() {
        artifact_paths["report"] = serde_json::json!("report.json");
    }

    serde_json::json!({
        "run_id": run_id,
        "status": "completed",
        "commit": commit,
        "base_used": base_used,
        "verdict": verdict,
        "merge_recommendation": merge_rec,
        "allow_merge": allow_merge,
        "blocking_issues": blocking,
        "caveats": caveats,
        "gates": read::read_gates(run_dir),
        "artifact_paths": artifact_paths,
        "stats": {
            "checks_passed": checks_passed,
            "checks_failed": checks_failed,
            "files_changed": files_changed,
        },
    })
}

/// Pull run stats from the freshly-registered index entry (falls back to zeros).
fn run_stats(run_id: &str, run_dir: &Path) -> (usize, usize, usize) {
    let index = crate::storage::RunIndex::load();
    if let Some(e) = index
        .entries()
        .iter()
        .find(|e| e.id == run_id && e.path == run_dir)
    {
        (e.checks_passed, e.checks_failed, e.files_changed)
    } else {
        (0, 0, 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_parse_defaults_quick_and_rejects_unknown() {
        assert_eq!(Profile::parse(None).unwrap(), Profile::Quick);
        assert_eq!(Profile::parse(Some("quick")).unwrap(), Profile::Quick);
        assert_eq!(Profile::parse(Some("deep")).unwrap(), Profile::Deep);
        let err = Profile::parse(Some("turbo")).unwrap_err();
        assert_eq!(err.class, error_class::RUN_FAILED);
    }

    #[test]
    fn intended_bases_uses_explicit_then_defaults() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        crate::git::git_cmd()
            .args(["init", "-b", "main"])
            .current_dir(repo)
            .output()
            .unwrap();
        crate::git::git_cmd()
            .args(["config", "user.email", "test@test.com"])
            .current_dir(repo)
            .output()
            .unwrap();
        crate::git::git_cmd()
            .args(["config", "user.name", "Test"])
            .current_dir(repo)
            .output()
            .unwrap();
        std::fs::write(repo.join("a.txt"), "hello\n").unwrap();
        crate::git::git_cmd()
            .args(["add", "-A"])
            .current_dir(repo)
            .output()
            .unwrap();
        crate::git::git_cmd()
            .args(["commit", "-m", "init"])
            .current_dir(repo)
            .output()
            .unwrap();
        let explicit = select_bases(repo, Some("dev"));
        assert_eq!(explicit.bases, vec!["dev"]);
        assert!(!explicit.base_fallback);

        let fallback = select_bases(repo, None);
        assert!(fallback.base_fallback);
        assert_eq!(fallback.bases, vec!["main"]);
    }

    #[test]
    fn ref_exists_handles_dash_prefixed_branch_names() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        crate::git::git_cmd()
            .args(["init", "-b", "main"])
            .current_dir(repo)
            .output()
            .unwrap();
        crate::git::git_cmd()
            .args(["config", "user.email", "test@test.com"])
            .current_dir(repo)
            .output()
            .unwrap();
        crate::git::git_cmd()
            .args(["config", "user.name", "Test"])
            .current_dir(repo)
            .output()
            .unwrap();
        std::fs::write(repo.join("a.txt"), "hello\n").unwrap();
        crate::git::git_cmd()
            .args(["add", "-A"])
            .current_dir(repo)
            .output()
            .unwrap();
        crate::git::git_cmd()
            .args(["commit", "-m", "init"])
            .current_dir(repo)
            .output()
            .unwrap();
        crate::git::git_cmd()
            .args(["update-ref", "refs/heads/-dash", "HEAD"])
            .current_dir(repo)
            .output()
            .unwrap();

        assert!(ref_exists(repo, "-dash"));
        assert!(!ref_exists(repo, "-missing"));
    }

    /// PR #12 review: two allocations that collide on the same timestamp within
    /// one branch must not share a directory. Exclusive `create_dir` makes the
    /// second caller take a distinct suffixed id instead of clobbering the pack.
    #[test]
    fn allocate_run_dir_is_exclusive_within_branch() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let stamp = "20260701-120000";
        let (dir1, id1) = allocate_run_dir_in(root, "main", stamp).unwrap();
        let (dir2, id2) = allocate_run_dir_in(root, "main", stamp).unwrap();
        assert_eq!(id1, stamp);
        assert_eq!(id2, "20260701-120000-2");
        assert_ne!(dir1, dir2);
        assert!(dir1.is_dir() && dir2.is_dir());
    }

    /// PR #12 review (spec 4a): a run_id is unique across the whole repo, not
    /// per branch. An id already used on another branch forces a suffix so an
    /// explicit-id lookup never collides between branches.
    #[test]
    fn allocate_run_dir_is_globally_unique_across_branches() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let stamp = "20260701-120000";
        let (existing, existing_id) = allocate_run_dir_in(root, "feature", stamp).unwrap();
        let (fresh, fresh_id) = allocate_run_dir_in(root, "main", stamp).unwrap();
        assert_eq!(existing_id, stamp);
        assert_eq!(fresh_id, "20260701-120000-2");
        assert_ne!(existing_id, fresh_id);
        assert!(existing.is_dir() && fresh.is_dir());
    }

    /// PR #12 review: a completed pack writes report.json at the pack root, so
    /// the run_review response must advertise `report.json`, not the (never
    /// present) `00_summary/report.json`, or clients cannot find the report.
    #[test]
    fn completed_body_advertises_root_report_json() {
        let tmp = tempfile::tempdir().unwrap();
        let run_dir = tmp.path();
        let summary = run_dir.join("00_summary");
        std::fs::create_dir_all(&summary).unwrap();
        std::fs::write(
            summary.join("MERGE_GATE.json"),
            serde_json::to_string(&serde_json::json!({
                "bases": ["main"],
                "decision": {
                    "merge_recommendation": "approve",
                    "verdict": "APPROVE",
                    "allow_merge": true
                }
            }))
            .unwrap(),
        )
        .unwrap();
        // Root-level report.json — where the pack actually writes it.
        std::fs::write(run_dir.join("report.json"), "{}").unwrap();

        let body = completed_body(run_dir, "20260701-120000", "abc1234");
        assert_eq!(
            body["artifact_paths"]["report"],
            serde_json::json!("report.json")
        );
    }

    // The quick-timeout process-group kill uses crate::proc::sigkill_process_group,
    // proven canonically in crate::proc::tests (grandchild reap).
}
