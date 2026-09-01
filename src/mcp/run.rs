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
use std::sync::{Arc, Mutex};
use std::time::Duration;

type OwnedDetachedChild = Box<dyn process_wrap::std::ChildWrapper>;

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
fn allocate_run_dir(
    repo_name: &str,
    branch_key: &str,
    commit: &str,
) -> Result<(PathBuf, String), ToolError> {
    crate::config::allocate_run_dir(repo_name, branch_key, commit).map_err(|e| {
        ToolError::new(
            error_class::RUN_FAILED,
            format!("failed to allocate run dir: {e}"),
        )
    })
}

/// Detect a currently active run on this repo branch (live RUNNING marker).
/// Path to the per-branch activation lock that serializes concurrent `start`s.
/// A file (not a directory), so it is ignored by the run-dir scans in
/// `active_run`/`rebuild`, and lives alongside the branch's run directories.
fn branch_activation_lock_path(repo_name: &str, branch_key: &str) -> PathBuf {
    crate::config::prview_home()
        .join("runs")
        .join(repo_name)
        .join(branch_key)
        .join(".active.lock")
}

/// Build the R2b `storage_locked` error, surfacing the active run id when known.
fn locked(active_run_id: Option<&str>) -> ToolError {
    ToolError::with_extra(
        error_class::STORAGE_LOCKED,
        "another review is already running for this repo branch",
        serde_json::json!({
            "active_run_id": active_run_id,
            "retry_after_ms": 5000,
        }),
    )
}

fn active_run(repo_name: &str, branch_key: &str) -> Option<String> {
    let base = crate::config::prview_home()
        .join("runs")
        .join(repo_name)
        .join(branch_key);
    let read = std::fs::read_dir(&base).ok()?;
    for entry in read.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        // `latest` is a symlink to a completed run, never an activation slot.
        if !file_type.is_dir() {
            continue;
        }
        let dir = entry.path();
        if read::read_running_marker(&dir).is_none() {
            continue;
        }
        // A marker is necessary for Running. Avoid probing the global index
        // for markerless history, while retaining the status check so durable
        // publication still beats a lingering marker.
        if matches!(read::run_status(&dir), read::RunStatus::Running { .. })
            && let Some(id) = dir.file_name().and_then(|n| n.to_str())
        {
            return Some(id.to_string());
        }
    }
    None
}

fn running_marker(
    pid: u32,
    profile: Profile,
    commit: String,
    base_used: Vec<String>,
) -> Result<read::RunningMarker, ToolError> {
    let process_birth_id = crate::storage::process_birth_identity(pid).map_err(|error| {
        ToolError::new(
            error_class::RUN_FAILED,
            format!("failed to capture spawned process identity: {error}"),
        )
    })?;
    Ok(read::RunningMarker {
        schema_version: read::RUNNING_MARKER_SCHEMA_VERSION,
        pid,
        process_birth_id: Some(process_birth_id),
        started_at: chrono::Local::now().to_rfc3339(),
        profile: profile.as_str().to_string(),
        commit,
        base_used,
    })
}

fn write_marker(run_dir: &Path, marker: &read::RunningMarker) -> Result<(), ToolError> {
    let text = serde_json::to_string_pretty(marker).map_err(|error| {
        ToolError::new(
            error_class::RUN_FAILED,
            format!("failed to serialize MCP running marker: {error}"),
        )
    })?;
    std::fs::write(read::running_marker_path(run_dir), text).map_err(|error| {
        ToolError::new(
            error_class::RUN_FAILED,
            format!("failed to write MCP running marker: {error}"),
        )
    })
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

fn remote_ref_exists(repo: &Path, branch: &str) -> bool {
    ref_exists(repo, &format!("refs/remotes/origin/{branch}"))
}

pub(crate) fn select_bases(repo: &Path, base: Option<&str>) -> BaseSelection {
    if let Some(base) = base {
        return BaseSelection {
            bases: vec![base.to_string()],
            base_fallback: false,
            caveats: Vec::new(),
        };
    }

    let mut caveats = Vec::new();
    if let Some(branch) = origin_head_branch(repo).or_else(|| configured_origin_head(repo)) {
        if remote_ref_exists(repo, &branch) {
            return BaseSelection {
                bases: vec![format!("origin/{branch}")],
                base_fallback: false,
                caveats: Vec::new(),
            };
        }
        caveats.push(format!(
            "base_fallback: detected default branch 'origin/{branch}' does not exist remotely; tried develop/main/master"
        ));
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
        caveats: if caveats.is_empty() {
            vec![
                "base_fallback: default branch was not detectable; tried develop/main/master"
                    .to_string(),
            ]
        } else {
            caveats
        },
    }
}

/// Positional args for the child prview. The leading `--` terminates options so
/// branch names like `-dash` are always parsed as TARGET, never as flags.
fn positional_args(repo: &Path, selection: &BaseSelection) -> Vec<String> {
    let branch = crate::config::current_branch_name(repo).unwrap_or_else(|| "HEAD".to_string());
    let mut args = vec!["--".to_string(), branch];
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

fn reserve_output_dir(run_dir: &Path, run_id: &str) -> Result<String, ToolError> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let nonce = format!("{run_id}:{}:{now}", std::process::id());
    crate::artifacts::reserve_mcp_output_dir(run_dir, &nonce).map_err(|error| {
        ToolError::new(
            error_class::RUN_FAILED,
            format!("failed to reserve MCP pack path: {error:#}"),
        )
    })?;
    Ok(nonce)
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

    // mcp-3/TOCTOU: the R2b "one active run" rule was a check-then-act — two
    // concurrent starts could both see no active run and both proceed. Serialize
    // activation for this repo branch behind the storage layer's OS file lock.
    // The handle is released by the kernel even if the owner process dies. Held for
    // the whole quick run and until a deep run's marker is on disk, so the
    // window between "check" and "marker visible to `active_run`" is closed.
    let _activation = match crate::storage::acquire_lock_at(&branch_activation_lock_path(
        &repo_name,
        &branch_key,
    )) {
        Ok(guard) => guard,
        // Another activation is in flight; surface the current run id if its
        // marker already landed, else a bare storage_locked.
        Err(_) => return Err(locked(active_run(&repo_name, &branch_key).as_deref())),
    };

    // R2b: one active run per repo branch (now race-free under the lock above).
    if let Some(active_run_id) = active_run(&repo_name, &branch_key) {
        return Err(locked(Some(&active_run_id)));
    }

    let commit = crate::config::short_head(repo);
    let (run_dir, run_id) = allocate_run_dir(&repo_name, &branch_key, &commit)?;
    let output_reservation = reserve_output_dir(&run_dir, &run_id)?;
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
                .env(
                    crate::artifacts::MCP_OUTPUT_RESERVATION_ENV,
                    &output_reservation,
                )
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

            // Marker setup is part of child ownership. If PID identity capture
            // or durable publication fails, terminate the tree and reap the
            // direct root before failing the RPC; no untracked child escapes.
            let marker = match child_pid
                .ok_or_else(|| {
                    ToolError::new(error_class::RUN_FAILED, "spawned prview has no process id")
                })
                .and_then(|pid| {
                    running_marker(pid, profile, commit.clone(), selection.bases.clone())
                }) {
                Ok(marker) => marker,
                Err(error) => {
                    let _ = crate::proc::terminate_and_reap_tokio_child(
                        &mut child,
                        child_pid,
                        Duration::from_secs(5),
                    )
                    .await;
                    return Err(error);
                }
            };
            if let Err(error) = write_marker(&run_dir, &marker) {
                let _ = crate::proc::terminate_and_reap_tokio_child(
                    &mut child,
                    child_pid,
                    Duration::from_secs(5),
                )
                .await;
                return Err(error);
            }

            let budget = quick_budget();
            match tokio::time::timeout(budget, child.wait()).await {
                Err(_) => {
                    // Kill the whole group first so the check-tool grandchildren
                    // die, then reap the direct child.
                    let reaped = crate::proc::terminate_and_reap_tokio_child(
                        &mut child,
                        child_pid,
                        Duration::from_secs(5),
                    )
                    .await;
                    if !reaped {
                        eprintln!(
                            "prview MCP: quick timeout could not confirm process-tree termination and direct-root reap"
                        );
                    }
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
                Ok(Ok(status)) => {
                    // A BLOCK verdict may exit non-zero and still be a valid
                    // review. The success oracle is therefore not exit zero,
                    // but it must be stronger than SANITY: exact durable index
                    // publication distinguishes a valid BLOCK from an execution
                    // failure after pack finalization.
                    if read::run_status(&run_dir) != read::RunStatus::Completed {
                        return Err(ToolError::with_extra(
                            error_class::RUN_FAILED,
                            "prview produced no durably published pack",
                            serde_json::json!({
                                "run_id": run_id,
                                "exit_code": status.code(),
                                "stderr_tail": stderr_tail(&run_dir),
                            }),
                        ));
                    }
                    read::require_published_run(&run_dir, &run_id)?;
                    // Completed: the child already registered the run; drop the
                    // marker so status readers see a clean completion.
                    let _ = std::fs::remove_file(read::running_marker_path(&run_dir));
                    let mut body = completed_body(&run_dir, &run_id, &commit)?;
                    add_base_metadata(&mut body, &selection);
                    Ok(body)
                }
            }
        }
        Profile::Deep => {
            let child = spawn_detached(repo, &args, &output_reservation, out_file, err_file)?;
            activate_detached_child(
                &run_dir,
                child,
                profile,
                commit.clone(),
                selection.bases.clone(),
            )?;
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

/// Spawn a deep run in its own process group on Unix. The caller still owns the
/// returned handle and must either install the detached reaper or terminate and
/// reap it before returning.
fn spawn_detached(
    repo: &Path,
    args: &[String],
    output_reservation: &str,
    out_file: File,
    err_file: File,
) -> Result<OwnedDetachedChild, ToolError> {
    let mut cmd = std::process::Command::new(std::env::current_exe().map_err(|e| {
        ToolError::new(error_class::RUN_FAILED, format!("current_exe failed: {e}"))
    })?);
    cmd.current_dir(repo)
        .args(args)
        .env(
            crate::artifacts::MCP_OUTPUT_RESERVATION_ENV,
            output_reservation,
        )
        .stdout(std::process::Stdio::from(out_file))
        .stderr(std::process::Stdio::from(err_file));
    crate::proc::harden_std(&mut cmd);

    // Windows uses the same durable Job Object ownership as the other sync
    // process paths, so descendants remain owned even if the direct root exits.
    crate::proc::spawn_owned_std_child(cmd).map_err(|e| {
        ToolError::new(
            error_class::RUN_FAILED,
            format!("spawn detached prview failed: {e}"),
        )
    })
}

/// Terminate the complete detached tree and reap its direct root. A child that
/// already exited is also explicitly waited, so immediate failure cannot leave
/// a zombie in the long-lived MCP server.
fn terminate_and_reap_detached_child(child: &mut OwnedDetachedChild) -> bool {
    crate::proc::terminate_and_reap_owned_std_child(child.as_mut())
}

/// Transfer a child into one detached waiter thread without losing ownership
/// if thread creation fails. The parent retains the same shared slot until the
/// builder succeeds; only the running reaper may take the child afterwards.
fn install_detached_reaper(
    child: OwnedDetachedChild,
    run_dir: PathBuf,
    publication_index: PathBuf,
) -> Result<(), (std::io::Error, OwnedDetachedChild)> {
    let owned = Arc::new(Mutex::new(Some(child)));
    let reaper_owned = Arc::clone(&owned);
    let spawn = std::thread::Builder::new()
        .name("prview-mcp-deep-reaper".to_string())
        .spawn(move || {
            let child = reaper_owned
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .take();
            let Some(mut child) = child else {
                return;
            };
            #[cfg(unix)]
            let pid = child.id();
            let _ = child.wait();
            // On Unix the direct root may exit while a background descendant
            // remains in the process group created by `harden_std`. Reaping
            // the root is not tree ownership: close the residual group before
            // declaring the marker non-blocking. Windows' wrapper wait owns the
            // complete Job Object lifecycle.
            #[cfg(unix)]
            let _ = crate::proc::sigkill_process_group(pid);
            // Successful publication is the only state that may discard the
            // marker. Failed children retain a stale diagnostic marker, but no
            // zombie and no active-run lockout.
            if read::run_status_with_index(&run_dir, &publication_index)
                == read::RunStatus::Completed
            {
                let _ = std::fs::remove_file(read::running_marker_path(&run_dir));
            }
        });

    match spawn {
        Ok(handle) => {
            drop(handle);
            Ok(())
        }
        Err(error) => {
            let child = owned
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .take()
                .expect("reaper child remains owned when thread spawn fails");
            Err((error, child))
        }
    }
}

/// Publish the v2 identity marker and install the waiter that owns the child
/// for the remainder of its lifetime. Any setup failure is fail-closed: the
/// child tree is terminated and the direct root reaped before the RPC fails.
fn activate_detached_child(
    run_dir: &Path,
    mut child: OwnedDetachedChild,
    profile: Profile,
    commit: String,
    base_used: Vec<String>,
) -> Result<u32, ToolError> {
    // Capture the publication dependency on the caller thread. Test homes are
    // deliberately thread-local, so a reaper that re-resolves PRVIEW_HOME on
    // its own thread could otherwise inspect the operator's real index.
    let publication_index = crate::config::prview_home().join("index.jsonl");
    let pid = child.id();
    let marker = match running_marker(pid, profile, commit, base_used) {
        Ok(marker) => marker,
        Err(error) => {
            let _ = terminate_and_reap_detached_child(&mut child);
            return Err(error);
        }
    };
    if let Err(error) = write_marker(run_dir, &marker) {
        let _ = terminate_and_reap_detached_child(&mut child);
        return Err(error);
    }
    if let Err((error, mut child)) =
        install_detached_reaper(child, run_dir.to_path_buf(), publication_index)
    {
        let reaped = terminate_and_reap_detached_child(&mut child);
        let _ = std::fs::remove_file(read::running_marker_path(run_dir));
        let detail = if reaped {
            String::new()
        } else {
            "; process-tree termination/direct-root reap could not be confirmed".to_string()
        };
        return Err(ToolError::new(
            error_class::RUN_FAILED,
            format!("failed to start detached prview reaper: {error}{detail}"),
        ));
    }
    Ok(pid)
}

/// Build the completed-run response body (quick sync path).
///
/// A completed pack with an unreadable/corrupt `MERGE_GATE.json` is a fail-loud
/// `storage_corrupt` — the SAME contract the `verdict` tool honours on the
/// identical state (mod.rs `verdict` returns `read_decision`'s error). The old
/// path silently substituted `verdict=UNKNOWN, blocking=[], caveats=[]` and
/// returned a `status=completed` success, an "empty success" the MCP contract
/// (types.rs) forbids and a signal the `verdict` tool would reject.
fn completed_body(
    run_dir: &Path,
    run_id: &str,
    commit: &str,
) -> Result<serde_json::Value, ToolError> {
    let d = read::read_decision(run_dir)?;
    let (verdict, merge_rec, allow_merge, enforcement_disposition, base_used, blocking, caveats) = (
        d.verdict.clone(),
        d.merge_recommendation.clone(),
        d.allow_merge,
        d.enforcement_disposition,
        d.base_used.clone(),
        d.blocking_issues.clone(),
        d.caveats.clone(),
    );

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

    Ok(serde_json::json!({
        "run_id": run_id,
        "status": "completed",
        "commit": commit,
        "base_used": base_used,
        "verdict": verdict,
        "merge_recommendation": merge_rec,
        "allow_merge": allow_merge,
        "enforcement_disposition": enforcement_disposition,
        "blocking_issues": blocking,
        "caveats": caveats,
        "gates": read::read_gates(run_dir),
        "artifact_paths": artifact_paths,
        "stats": {
            "checks_passed": checks_passed,
            "checks_failed": checks_failed,
            "files_changed": files_changed,
        },
    }))
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

    const REAPER_FIXTURE_ENV: &str = "PRVIEW_MCP_REAPER_TEST_SIGNAL";
    #[cfg(unix)]
    const REAPER_GRANDCHILD_PID_ENV: &str = "PRVIEW_MCP_REAPER_TEST_GRANDCHILD_PID";

    /// Subprocess-only fixture. The parent test launches this exact test under
    /// the test binary, then releases it through a file barrier so marker and
    /// reaper setup are deterministic before the child exits immediately.
    #[test]
    fn detached_reaper_child_fixture() {
        let Some(signal) = std::env::var_os(REAPER_FIXTURE_ENV) else {
            return;
        };
        let signal = PathBuf::from(signal);
        #[cfg(unix)]
        if let Some(pidfile) = std::env::var_os(REAPER_GRANDCHILD_PID_ENV) {
            let status = std::process::Command::new("sh")
                .args([
                    "-c",
                    "sleep 30 & echo $! > \"$PRVIEW_MCP_REAPER_TEST_GRANDCHILD_PID\"",
                ])
                .env(REAPER_GRANDCHILD_PID_ENV, &pidfile)
                .status()
                .expect("spawn background-grandchild fixture");
            assert!(status.success());
        }
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while !signal.exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        if signal.exists() {
            std::process::exit(17);
        }
        std::process::exit(18);
    }

    fn spawn_reaper_fixture(signal: &Path) -> OwnedDetachedChild {
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "mcp::run::tests::detached_reaper_child_fixture",
                "--nocapture",
            ])
            .env(REAPER_FIXTURE_ENV, signal)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        crate::proc::harden_std(&mut command);
        crate::proc::spawn_owned_std_child(command).expect("spawn detached reaper fixture")
    }

    #[cfg(unix)]
    fn spawn_reaper_fixture_with_grandchild(signal: &Path, pidfile: &Path) -> OwnedDetachedChild {
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "mcp::run::tests::detached_reaper_child_fixture",
                "--nocapture",
            ])
            .env(REAPER_FIXTURE_ENV, signal)
            .env(REAPER_GRANDCHILD_PID_ENV, pidfile)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        crate::proc::harden_std(&mut command);
        crate::proc::spawn_owned_std_child(command)
            .expect("spawn detached reaper fixture with grandchild")
    }

    fn wait_until_process_is_reaped(pid: u32) {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while crate::storage::is_process_alive(pid) && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            !crate::storage::is_process_alive(pid),
            "detached direct child {pid} was not reaped"
        );
    }

    #[test]
    fn profile_parse_defaults_quick_and_rejects_unknown() {
        assert_eq!(Profile::parse(None).unwrap(), Profile::Quick);
        assert_eq!(Profile::parse(Some("quick")).unwrap(), Profile::Quick);
        assert_eq!(Profile::parse(Some("deep")).unwrap(), Profile::Deep);
        let err = Profile::parse(Some("turbo")).unwrap_err();
        assert_eq!(err.class, error_class::RUN_FAILED);
    }

    #[test]
    fn detached_fast_failure_is_reaped_and_does_not_block_second_run() {
        let home = tempfile::tempdir().unwrap();
        let _home = crate::config::override_test_prview_home(home.path().to_path_buf());
        let repo_name = "mcp-reaper-test";
        let branch_key = "main";
        let run_dir = home
            .path()
            .join("runs")
            .join(repo_name)
            .join(branch_key)
            .join("fast-failure");
        std::fs::create_dir_all(&run_dir).unwrap();
        let signal_dir = tempfile::tempdir().unwrap();
        let signal = signal_dir.path().join("exit-now");
        let child = spawn_reaper_fixture(&signal);

        let pid = activate_detached_child(
            &run_dir,
            child,
            Profile::Deep,
            "abc1234".to_string(),
            vec!["main".to_string()],
        )
        .unwrap();
        assert_eq!(
            active_run(repo_name, branch_key).as_deref(),
            Some("fast-failure")
        );

        std::fs::write(&signal, b"go").unwrap();
        wait_until_process_is_reaped(pid);
        assert!(matches!(
            read::run_status(&run_dir),
            read::RunStatus::Stale { .. }
        ));
        assert_eq!(
            active_run(repo_name, branch_key),
            None,
            "a failed deep child must not block the next run"
        );
    }

    #[test]
    fn detached_reaper_uses_captured_publication_index() {
        let home = tempfile::tempdir().unwrap();
        let _home = crate::config::override_test_prview_home(home.path().to_path_buf());
        let run_dir = home
            .path()
            .join("runs")
            .join("mcp-reaper-test")
            .join("main")
            .join("published-run");
        let summary = run_dir.join("00_summary");
        std::fs::create_dir_all(&summary).unwrap();
        std::fs::write(summary.join("SANITY.json"), "{}").unwrap();
        let published = serde_json::json!({
            "id": "published-run",
            "repo": "mcp-reaper-test",
            "branch": "main",
            "commit": "abc1234",
            "path": run_dir,
            "created_at": "2026-07-01T12:00:00Z",
            "quality_pass": true,
            "merge_status": "ALLOW",
            "policy_mode": "shadow",
            "checks_passed": 1,
            "checks_failed": 0,
            "files_changed": 1,
            "size_bytes": 1,
            "has_dashboard": false,
        });
        std::fs::write(
            home.path().join("index.jsonl"),
            format!("{}\n", serde_json::to_string(&published).unwrap()),
        )
        .unwrap();

        let signal_dir = tempfile::tempdir().unwrap();
        let signal = signal_dir.path().join("exit-now");
        let child = spawn_reaper_fixture(&signal);
        let pid = activate_detached_child(
            &run_dir,
            child,
            Profile::Deep,
            "abc1234".to_string(),
            vec!["main".to_string()],
        )
        .unwrap();
        assert!(read::running_marker_path(&run_dir).exists());

        std::fs::write(&signal, b"go").unwrap();
        wait_until_process_is_reaped(pid);
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while read::running_marker_path(&run_dir).exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            !read::running_marker_path(&run_dir).exists(),
            "reaper must resolve completion from the caller's captured publication index"
        );
    }

    #[cfg(unix)]
    #[test]
    fn active_run_skips_latest_alias_to_published_run() {
        let home = tempfile::tempdir().unwrap();
        let _home = crate::config::override_test_prview_home(home.path().to_path_buf());
        let branch_dir = home.path().join("runs/mcp-latest-test/main");
        let run_dir = branch_dir.join("published-run");
        let summary = run_dir.join("00_summary");
        std::fs::create_dir_all(&summary).unwrap();
        std::fs::write(summary.join("SANITY.json"), "{}").unwrap();
        let marker = read::RunningMarker {
            schema_version: read::RUNNING_MARKER_SCHEMA_VERSION,
            pid: std::process::id(),
            process_birth_id: crate::storage::process_birth_identity(std::process::id()).ok(),
            started_at: "2026-07-01T12:00:00Z".to_string(),
            profile: "deep".to_string(),
            commit: "abc1234".to_string(),
            base_used: vec!["main".to_string()],
        };
        std::fs::write(
            read::running_marker_path(&run_dir),
            serde_json::to_string(&marker).unwrap(),
        )
        .unwrap();
        let published = serde_json::json!({
            "id": "published-run",
            "repo": "mcp-latest-test",
            "branch": "main",
            "commit": "abc1234",
            "path": run_dir,
            "created_at": "2026-07-01T12:00:00Z",
            "quality_pass": true,
            "merge_status": "ALLOW",
            "policy_mode": "shadow",
            "checks_passed": 1,
            "checks_failed": 0,
            "files_changed": 1,
            "size_bytes": 1,
            "has_dashboard": false,
        });
        std::fs::write(
            home.path().join("index.jsonl"),
            format!("{}\n", serde_json::to_string(&published).unwrap()),
        )
        .unwrap();
        std::os::unix::fs::symlink("published-run", branch_dir.join("latest")).unwrap();

        assert_eq!(read::run_status(&run_dir), read::RunStatus::Completed);
        assert_eq!(active_run("mcp-latest-test", "main"), None);
    }

    #[cfg(unix)]
    #[test]
    fn detached_reaper_terminates_residual_process_group() {
        let run_dir = tempfile::tempdir().unwrap();
        let fixture = tempfile::tempdir().unwrap();
        let signal = fixture.path().join("exit-now");
        let pidfile = fixture.path().join("grandchild.pid");
        let child = spawn_reaper_fixture_with_grandchild(&signal, &pidfile);

        let pid = activate_detached_child(
            run_dir.path(),
            child,
            Profile::Deep,
            "abc1234".to_string(),
            vec!["main".to_string()],
        )
        .unwrap();

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !pidfile.exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        let grandchild: u32 = std::fs::read_to_string(&pidfile)
            .expect("fixture records grandchild pid")
            .trim()
            .parse()
            .expect("numeric grandchild pid");
        assert!(crate::storage::is_process_alive(grandchild));

        std::fs::write(&signal, b"go").unwrap();
        wait_until_process_is_reaped(pid);
        wait_until_process_is_reaped(grandchild);
    }

    #[test]
    fn marker_write_failure_terminates_and_reaps_detached_child() {
        let run_dir = tempfile::tempdir().unwrap();
        // A directory at the marker path makes the durable write fail without
        // relying on platform-specific permission semantics.
        std::fs::create_dir(read::running_marker_path(run_dir.path())).unwrap();
        let signal_dir = tempfile::tempdir().unwrap();
        let child = spawn_reaper_fixture(&signal_dir.path().join("never-release"));
        let pid = child.id();

        let error = activate_detached_child(
            run_dir.path(),
            child,
            Profile::Deep,
            "abc1234".to_string(),
            vec!["main".to_string()],
        )
        .unwrap_err();

        assert_eq!(error.class, error_class::RUN_FAILED);
        assert!(error.message.contains("running marker"));
        wait_until_process_is_reaped(pid);
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
    fn detected_default_branch_must_exist_before_use() {
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
            .args([
                "symbolic-ref",
                "refs/remotes/origin/HEAD",
                "refs/remotes/origin/missing",
            ])
            .current_dir(repo)
            .output()
            .unwrap();

        let selection = select_bases(repo, None);

        assert!(selection.base_fallback);
        assert_eq!(selection.bases, vec!["main"]);
        assert!(selection.caveats.iter().any(|c| c.contains("missing")));
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

    #[test]
    fn positional_args_terminate_options_before_branch_name() {
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
        crate::git::git_cmd()
            .args(["switch", "-q", "--", "-dash"])
            .current_dir(repo)
            .output()
            .unwrap();

        let args = positional_args(
            repo,
            &BaseSelection {
                bases: vec!["main".to_string()],
                base_fallback: false,
                caveats: Vec::new(),
            },
        );

        assert_eq!(args, vec!["--", "-dash", "main"]);
    }

    /// PR #12 review: two allocations that collide on the same timestamp within
    /// one branch must not share a directory. Exclusive `create_dir` makes the
    /// second caller take a distinct suffixed id instead of clobbering the pack.
    #[test]
    fn allocate_run_dir_is_exclusive_within_branch() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let stamp = "20260701-120000";
        let (dir1, id1) = crate::config::allocate_run_dir_in(root, "main", stamp, None).unwrap();
        let (dir2, id2) = crate::config::allocate_run_dir_in(root, "main", stamp, None).unwrap();
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
        let (existing, existing_id) =
            crate::config::allocate_run_dir_in(root, "feature", stamp, Some("aaaa111")).unwrap();
        let (fresh, fresh_id) =
            crate::config::allocate_run_dir_in(root, "main", stamp, Some("bbbb222")).unwrap();
        assert_eq!(existing_id, "20260701-120000-aaaa111");
        assert_eq!(fresh_id, "20260701-120000-bbbb222");
        assert_ne!(existing_id, fresh_id);
        assert!(existing.is_dir() && fresh.is_dir());
    }

    #[test]
    fn allocate_run_dir_keeps_commit_suffix_unique_when_same_commit_collides() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let stamp = "20260701-120000";
        let (_existing, existing_id) =
            crate::config::allocate_run_dir_in(root, "feature", stamp, Some("aaaa111")).unwrap();
        let (_fresh, fresh_id) =
            crate::config::allocate_run_dir_in(root, "main", stamp, Some("aaaa111")).unwrap();

        assert_eq!(existing_id, "20260701-120000-aaaa111");
        assert_eq!(fresh_id, "20260701-120000-aaaa111-2");
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

        let body = completed_body(run_dir, "20260701-120000", "abc1234").unwrap();
        assert_eq!(
            body["artifact_paths"]["report"],
            serde_json::json!("report.json")
        );
    }

    /// mcp-2 delivery-verifier (c): a completed pack whose `MERGE_GATE.json` is
    /// unreadable must fail loud (`storage_corrupt`) instead of returning a
    /// `status=completed` body with `verdict=UNKNOWN, caveats=[]`. This mirrors
    /// the `verdict` tool, which rejects the identical state.
    #[test]
    fn completed_body_fails_loud_on_corrupt_merge_gate() {
        let tmp = tempfile::tempdir().unwrap();
        let run_dir = tmp.path();
        let summary = run_dir.join("00_summary");
        std::fs::create_dir_all(&summary).unwrap();
        // Present but not valid JSON — read_decision must reject it.
        std::fs::write(summary.join("MERGE_GATE.json"), "{ not json ").unwrap();

        let err = completed_body(run_dir, "20260701-120000", "abc1234").unwrap_err();
        assert_eq!(err.class, error_class::STORAGE_CORRUPT);
    }

    // The quick-timeout tree kill uses crate::proc::terminate_process_tree,
    // proven canonically on Unix and by a real Windows-only descendant test.
}
