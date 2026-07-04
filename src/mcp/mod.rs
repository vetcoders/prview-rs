//! prview MCP server (`prview mcp`).
//!
//! A thin contract adapter that lets agents run reviews and consume the
//! verdict/artifacts through MCP tools instead of the CLI. Transport is stdio;
//! tools are stateless and idempotent — every call carries an explicit `repo`
//! path and reads truth from storage (`~/.prview/`), never from process cwd or
//! in-memory session state. See `2026-07-01-prview-mcp-v1-design.md`.

pub mod read;
pub mod run;
pub mod types;

use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::{ServerHandler, ServiceExt, tool, tool_handler, tool_router, transport::stdio};
use serde::Deserialize;
use serde_json::json;
use types::error_class;

// ---------------------------------------------------------------------------
// Tool argument schemas
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct HealthArgs {
    /// Absolute path to a git repository. When provided, repo-specific tool
    /// availability (profile) is included; omit for a global-only probe.
    #[serde(default)]
    pub repo: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct StateArgs {
    /// Absolute path to the git repository to inspect.
    pub repo: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct RunReviewArgs {
    /// Absolute path to the git repository to review.
    pub repo: String,
    /// Base ref to diff against. Default: merge-base with the repo default branch.
    #[serde(default)]
    pub base: Option<String>,
    /// "quick" (synchronous, 120s budget) or "deep" (async; poll verdict). Default quick.
    #[serde(default)]
    pub profile: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct VerdictArgs {
    /// Absolute path to the git repository.
    pub repo: String,
    /// Opaque run id. Default: latest run for the current repo HEAD.
    #[serde(default)]
    pub run_id: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct FindingsArgs {
    /// Absolute path to the git repository.
    pub repo: String,
    /// Opaque run id. Default: latest run for the current repo HEAD.
    #[serde(default)]
    pub run_id: Option<String>,
    /// Filter to a single SARIF level (e.g. "error", "warning", "note").
    #[serde(default)]
    pub severity: Option<String>,
    /// Filter to findings whose file path starts with this prefix.
    #[serde(default)]
    pub path: Option<String>,
    /// Opaque pagination cursor from a previous call's `next_cursor`.
    #[serde(default)]
    pub cursor: Option<String>,
    /// Max items to return in this page.
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ReadArtifactArgs {
    /// Absolute path to the git repository.
    pub repo: String,
    /// Opaque run id owning the artifact.
    pub run_id: String,
    /// Pack-relative artifact path (e.g. "00_summary/MERGE_GATE.json").
    pub artifact: String,
    /// Opaque pagination cursor from a previous call's `next_cursor`.
    #[serde(default)]
    pub cursor: Option<String>,
    /// Max lines to return in this page.
    #[serde(default)]
    pub limit: Option<usize>,
}

// ---------------------------------------------------------------------------
// Server
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct PrviewMcp {
    tool_router: rmcp::handler::server::router::tool::ToolRouter<Self>,
}

impl Default for PrviewMcp {
    fn default() -> Self {
        Self::new()
    }
}

#[tool_router]
impl PrviewMcp {
    pub fn new() -> Self {
        Self {
            tool_router: Self::tool_router(),
        }
    }

    #[tool(
        name = "health",
        description = "Call once at session start to confirm prview is operational."
    )]
    async fn health(&self, Parameters(args): Parameters<HealthArgs>) -> CallToolResult {
        let git_ok = crate::git::git_cmd()
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);

        // deps_repo is only meaningful with a repo (the profile picks the tools).
        let deps_repo = match args.repo.as_deref() {
            Some(repo) => match read::resolve_repo_root(repo) {
                Ok(root) => match crate::config::Config::for_state_viewer(&root) {
                    Ok(config) => {
                        let kind = config.profile.kind;
                        json!({
                            "profile": kind.as_str(),
                            "tools": profile_tool_availability(kind),
                        })
                    }
                    // Repo exists but profile detection failed: honest null, not a fabricated profile.
                    Err(_) => serde_json::Value::Null,
                },
                Err(_) => serde_json::Value::Null,
            },
            None => serde_json::Value::Null,
        };

        types::tool_success(json!({
            "version": env!("CARGO_PKG_VERSION"),
            "protocol": types::SCHEMA_VERSION,
            "deps_global": { "git": git_ok },
            "deps_repo": deps_repo,
        }))
    }

    #[tool(
        name = "state",
        description = "Cheap repo snapshot: branch, HEAD, dirty, latest run. Use before deciding whether a fresh run_review is needed."
    )]
    async fn state(&self, Parameters(args): Parameters<StateArgs>) -> CallToolResult {
        let root = match read::resolve_repo_root(&args.repo) {
            Ok(root) => root,
            Err(e) => return e.into_result(),
        };

        let repo_state = match crate::state::collect_state(
            &root,
            &crate::state::StateOpts {
                fast: true,
                json: true,
                hot: false,
            },
        ) {
            Ok(s) => s,
            Err(e) => {
                return types::tool_error(
                    error_class::NOT_A_GIT_REPO,
                    &format!("failed to read repo state: {e}"),
                    json!({}),
                );
            }
        };

        let repo_name = crate::config::repo_name_from_root(&root);
        // Same storage key as the write path so detached-HEAD runs resolve.
        let branch_key = crate::config::storage_branch_key(&root);
        let index = crate::storage::RunIndex::load();

        let running_for_head = running_run_summary(&repo_name, &branch_key, Some(&repo_state.head));
        let running_any = running_run_summary(&repo_name, &branch_key, None);

        let for_head = running_for_head
            .or_else(|| {
                read::latest_for_head(&index, &repo_name, &branch_key, &repo_state.head)
                    .map(run_summary_for_state)
            })
            .unwrap_or(serde_json::Value::Null);
        let any = running_any
            .or_else(|| {
                read::latest_any(&index, &repo_name, &branch_key).map(run_summary_for_state)
            })
            .unwrap_or(serde_json::Value::Null);

        let dirty = repo_state.is_dirty();
        let base_selection = run::select_bases(&root, None);

        types::tool_success(json!({
            "branch": repo_state.branch,
            "commit": repo_state.head,
            "default_branch": base_selection.bases.first().cloned(),
            "base_fallback": base_selection.base_fallback,
            "base_caveats": base_selection.caveats,
            "dirty": dirty,
            "files_changed": repo_state.files_changed,
            "latest_run_for_head": for_head,
            "latest_run_any": any,
        }))
    }

    #[tool(
        name = "run_review",
        description = "Generate a review pack. profile=quick is synchronous (120s budget). profile=deep returns immediately with run_id; poll verdict(run_id) for completion."
    )]
    async fn run_review(&self, Parameters(args): Parameters<RunReviewArgs>) -> CallToolResult {
        let root = match read::resolve_repo_root(&args.repo) {
            Ok(root) => root,
            Err(e) => return e.into_result(),
        };
        let profile = match run::Profile::parse(args.profile.as_deref()) {
            Ok(p) => p,
            Err(e) => return e.into_result(),
        };
        match run::start(&root, args.base, profile).await {
            Ok(body) => types::tool_success(body),
            Err(e) => e.into_result(),
        }
    }

    #[tool(
        name = "verdict",
        description = "Single decision truth for a run. Default: latest run for repo HEAD. For deep runs poll this until status=completed."
    )]
    async fn verdict(&self, Parameters(args): Parameters<VerdictArgs>) -> CallToolResult {
        let root = match read::resolve_repo_root(&args.repo) {
            Ok(root) => root,
            Err(e) => return e.into_result(),
        };
        let resolved = match read::resolve_run(&root, args.run_id.as_deref()) {
            Ok(r) => r,
            Err(e) => return e.into_result(),
        };

        match read::run_status(&resolved.run_dir) {
            read::RunStatus::Completed => {
                let decision = match read::read_decision(&resolved.run_dir) {
                    Ok(d) => d,
                    Err(e) => return e.into_result(),
                };
                types::tool_success(json!({
                    "run_id": resolved.run_id,
                    "commit": resolved.commit,
                    "status": "completed",
                    "base_used": decision.base_used,
                    "merge_recommendation": decision.merge_recommendation,
                    "allow_merge": decision.allow_merge,
                    "verdict": decision.verdict,
                    "blocking_issues": decision.blocking_issues,
                    "caveats": decision.caveats,
                    "gates": read::read_gates(&resolved.run_dir),
                    "generated_at": read::read_generated_at(&resolved.run_dir),
                }))
            }
            read::RunStatus::Running { .. } => types::tool_success(
                read::read_running_marker(&resolved.run_dir)
                    .map(|marker| in_progress_body(&resolved.run_id, &resolved.commit, &marker))
                    .unwrap_or_else(|| {
                        json!({
                            "run_id": resolved.run_id,
                            "commit": resolved.commit,
                            "status": "in_progress",
                            "run_status": "running",
                            "started_at": serde_json::Value::Null,
                            "elapsed_s": serde_json::Value::Null,
                            "base_used": [],
                            "retry_after_ms": 5000,
                        })
                    }),
            ),
            read::RunStatus::Stale { started_at, .. } => {
                let marker = read::read_running_marker(&resolved.run_dir);
                types::tool_success(json!({
                    "run_id": resolved.run_id,
                    "commit": resolved.commit,
                    "status": "stale",
                    "started_at": started_at,
                    "base_used": marker.map(|m| m.base_used).unwrap_or_default(),
                }))
            }
            read::RunStatus::Failed => types::tool_success(json!({
                "run_id": resolved.run_id,
                "commit": resolved.commit,
                "status": "failed",
                "base_used": [],
            })),
        }
    }

    #[tool(
        name = "findings",
        description = "Paged structured findings for a run. Prefer this over read_artifact."
    )]
    async fn findings(&self, Parameters(args): Parameters<FindingsArgs>) -> CallToolResult {
        let root = match read::resolve_repo_root(&args.repo) {
            Ok(root) => root,
            Err(e) => return e.into_result(),
        };
        let resolved = match read::resolve_run(&root, args.run_id.as_deref()) {
            Ok(r) => r,
            Err(e) => return e.into_result(),
        };
        if let Err(e) = require_completed(&resolved.run_dir) {
            return e.into_result();
        }

        let mut items = read::read_findings(&resolved.run_dir);
        if let Some(sev) = &args.severity {
            items.retain(|i| i.severity.eq_ignore_ascii_case(sev));
        }
        if let Some(prefix) = &args.path {
            items.retain(|i| i.file.starts_with(prefix));
        }

        let total = items.len();
        let offset = parse_cursor(&args.cursor).min(total);
        let limit = args.limit.unwrap_or(100).clamp(1, 1000);
        let page: Vec<serde_json::Value> = items
            .iter()
            .skip(offset)
            .take(limit)
            .map(|i| {
                json!({
                    "file": i.file,
                    "line": i.line,
                    "severity": i.severity,
                    "rule": i.rule,
                    "message": i.message,
                    "artifact_ref": read::sarif_ref(),
                })
            })
            .collect();
        let next = offset + page.len();
        let next_cursor = if next < total {
            Some(next.to_string())
        } else {
            None
        };

        types::tool_success(json!({
            "items": page,
            "total": total,
            "next_cursor": next_cursor,
        }))
    }

    #[tool(
        name = "read_artifact",
        description = "Raw artifact body, paged. Use only when findings/verdict summaries are not enough."
    )]
    async fn read_artifact(
        &self,
        Parameters(args): Parameters<ReadArtifactArgs>,
    ) -> CallToolResult {
        let root = match read::resolve_repo_root(&args.repo) {
            Ok(root) => root,
            Err(e) => return e.into_result(),
        };
        let resolved = match read::resolve_run(&root, Some(&args.run_id)) {
            Ok(r) => r,
            Err(e) => return e.into_result(),
        };

        // Logs are always readable (a failed/stale run must expose its post-mortem);
        // every other artifact requires a completed pack.
        let is_log = args.artifact == "run.log" || args.artifact == "run.stderr.log";
        if !is_log
            && read::run_status(&resolved.run_dir) != read::RunStatus::Completed
            && let Err(e) = require_completed(&resolved.run_dir)
        {
            return e.into_result();
        }

        let path = match read::resolve_artifact_path(&resolved.run_dir, &args.artifact) {
            Ok(p) => p,
            Err(e) => return e.into_result(),
        };
        let content = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(_) => {
                return types::tool_error(
                    error_class::ARTIFACT_MISSING,
                    "artifact is not readable as UTF-8 text",
                    json!({ "artifact": args.artifact }),
                );
            }
        };

        let lines: Vec<&str> = content.lines().collect();
        let total_lines = lines.len();
        let offset = parse_cursor(&args.cursor).min(total_lines);
        let limit = args.limit.unwrap_or(200).clamp(1, 5000);
        let taken: Vec<&str> = lines.iter().skip(offset).take(limit).copied().collect();
        let next = offset + taken.len();
        let next_cursor = if next < total_lines {
            Some(next.to_string())
        } else {
            None
        };

        types::tool_success(json!({
            "content": taken.join("\n"),
            "total_lines": total_lines,
            "next_cursor": next_cursor,
        }))
    }
}

/// Parse an opaque pagination cursor (numeric offset). An absent or malformed
/// cursor starts from the beginning.
fn parse_cursor(cursor: &Option<String>) -> usize {
    cursor
        .as_deref()
        .and_then(|c| c.parse::<usize>().ok())
        .unwrap_or(0)
}

/// Require a completed run for full artifact/finding reads. Maps non-completed
/// status to the fail-loud error class (with a retry hint while running).
fn require_completed(run_dir: &std::path::Path) -> Result<(), types::ToolError> {
    match read::run_status(run_dir) {
        read::RunStatus::Completed => Ok(()),
        read::RunStatus::Running { .. } => Err(types::ToolError::with_extra(
            error_class::STALE_RUN,
            "run is still in progress; poll verdict(run_id) until status=completed",
            json!({ "retry_after_ms": 5000 }),
        )),
        read::RunStatus::Stale { .. } => Err(types::ToolError::new(
            error_class::STALE_RUN,
            "run is stale (its process died before completing)",
        )),
        read::RunStatus::Failed => Err(types::ToolError::new(
            error_class::RUN_FAILED,
            "run failed and produced no completed pack",
        )),
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for PrviewMcp {
    fn get_info(&self) -> rmcp::model::ServerInfo {
        use rmcp::model::{Implementation, InitializeResult, ServerCapabilities};
        // Report a prview identity rather than the SDK default (which infers
        // "rmcp" from the SDK crate's build env).
        let mut server_info = Implementation::from_build_env();
        server_info.name = "prview".to_string();
        server_info.version = env!("CARGO_PKG_VERSION").to_string();

        let mut info = InitializeResult::new(ServerCapabilities::builder().enable_tools().build());
        info.server_info = server_info;
        info.instructions = Some(
            "prview review server. Call health at session start, run_review to generate a \
             review pack, then verdict/findings/read_artifact to consume it. Every tool takes \
             an absolute `repo` path; responses carry schema_version prview.mcp.v1."
                .to_string(),
        );
        info
    }
}

/// Probe availability of the profile-relevant external tools (cheap `which`).
fn profile_tool_availability(kind: crate::config::ProfileKind) -> serde_json::Value {
    use crate::config::ProfileKind;
    const RUST: &[&str] = &["cargo", "cargo-clippy", "rustfmt"];
    const JS: &[&str] = &["node", "npm"];
    const PYTHON: &[&str] = &["python3", "ruff", "mypy"];

    let mut tools: Vec<&str> = Vec::new();
    match kind {
        ProfileKind::Rust => tools.extend_from_slice(RUST),
        ProfileKind::Js => tools.extend_from_slice(JS),
        ProfileKind::Python => tools.extend_from_slice(PYTHON),
        ProfileKind::Mixed => {
            tools.extend_from_slice(RUST);
            tools.extend_from_slice(JS);
            tools.extend_from_slice(PYTHON);
        }
        ProfileKind::Generic => {}
    }

    let map: serde_json::Map<String, serde_json::Value> = tools
        .into_iter()
        .map(|bin| (bin.to_string(), json!(which::which(bin).is_ok())))
        .collect();
    serde_json::Value::Object(map)
}

fn elapsed_s(started_at: &str) -> Option<i64> {
    let started = chrono::DateTime::parse_from_rfc3339(started_at).ok()?;
    let elapsed = chrono::Local::now()
        .fixed_offset()
        .signed_duration_since(started);
    Some(elapsed.num_seconds().max(0))
}

fn in_progress_body(run_id: &str, commit: &str, marker: &read::RunningMarker) -> serde_json::Value {
    json!({
        "run_id": run_id,
        "commit": commit,
        "status": "in_progress",
        "run_status": "running",
        "started_at": marker.started_at.clone(),
        "elapsed_s": elapsed_s(&marker.started_at),
        "profile": marker.profile.clone(),
        "base_used": marker.base_used.clone(),
        "retry_after_ms": 5000,
    })
}

fn running_run_summary(
    repo_name: &str,
    branch_key: &str,
    head: Option<&str>,
) -> Option<serde_json::Value> {
    let base = crate::config::prview_home()
        .join("runs")
        .join(repo_name)
        .join(branch_key);
    running_run_summary_from_base(&base, head)
}

fn running_run_summary_from_base(
    base: &std::path::Path,
    head: Option<&str>,
) -> Option<serde_json::Value> {
    running_run_summary_from_base_with(base, head, read::run_status, read::read_running_marker)
}

fn running_run_summary_from_base_with(
    base: &std::path::Path,
    head: Option<&str>,
    run_status: impl Fn(&std::path::Path) -> read::RunStatus,
    read_marker: impl Fn(&std::path::Path) -> Option<read::RunningMarker>,
) -> Option<serde_json::Value> {
    let mut candidates: Vec<(String, serde_json::Value)> = Vec::new();
    for entry in std::fs::read_dir(base).ok()?.flatten() {
        let run_dir = entry.path();
        if !run_dir.is_dir() || !matches!(run_status(&run_dir), read::RunStatus::Running { .. }) {
            continue;
        }
        let Some(run_id) = run_dir.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let Some(marker) = read_marker(&run_dir) else {
            continue;
        };
        if let Some(head) = head
            && !read::commit_matches(&marker.commit, head)
        {
            continue;
        }
        let body = in_progress_body(run_id, &marker.commit, &marker);
        candidates.push((marker.started_at.clone(), body));
    }
    candidates
        .into_iter()
        .max_by(|a, b| a.0.cmp(&b.0))
        .map(|(_, body)| body)
}

/// Summarize a registered (completed) run for the `state` snapshot.
///
/// `profile` (quick/deep) is not persisted in the v1 index, so it is reported
/// as `null` rather than fabricated; `base_used` is read from the run's
/// MERGE_GATE (one small file). Registered index entries are, by construction,
/// completed runs.
fn run_summary_for_state(entry: &crate::storage::RunEntry) -> serde_json::Value {
    json!({
        "run_id": entry.id,
        "commit": entry.commit,
        "status": "completed",
        "profile": serde_json::Value::Null,
        "base_used": read::read_bases(&entry.path),
        "merge_status": entry.merge_status,
        "generated_at": entry.created_at,
    })
}

/// Serve the MCP protocol over stdio until the client disconnects.
pub async fn serve() -> anyhow::Result<()> {
    let service = PrviewMcp::new().serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_running_marker(run_dir: &std::path::Path, run_id_commit: &str, started_at: &str) {
        std::fs::write(
            read::running_marker_path(run_dir),
            serde_json::to_string(&read::RunningMarker {
                pid: std::process::id(),
                started_at: started_at.to_string(),
                profile: "deep".to_string(),
                commit: run_id_commit.to_string(),
                base_used: vec!["main".to_string()],
            })
            .unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn running_summary_skips_missing_marker_and_reports_healthy_run() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path();

        let healthy = base.join("20260704-healthy");
        std::fs::create_dir(&healthy).unwrap();
        write_running_marker(&healthy, "abcdef123456", "2026-07-04T00:00:02+00:00");

        let missing_marker = base.join("20260704-missing-marker");
        std::fs::create_dir(&missing_marker).unwrap();

        let summary = running_run_summary_from_base_with(
            base,
            None,
            |_| read::RunStatus::Running {
                pid: std::process::id(),
            },
            read::read_running_marker,
        )
        .unwrap();
        assert_eq!(summary["run_id"], serde_json::json!("20260704-healthy"));
        assert_eq!(summary["commit"], serde_json::json!("abcdef123456"));
    }
}
