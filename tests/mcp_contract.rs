//! Contract tests for the `prview mcp` stdio server.
//!
//! These drive the real binary over JSON-RPC on stdin/stdout, asserting the
//! wire contract from `2026-07-01-prview-mcp-v1-design.md`.

use prview::git::git_cmd;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdout, Command, Stdio};

// --- fixture git repo helpers -------------------------------------------------

/// Route every fixture git invocation through `git_cmd()` — the same isolated
/// builder production uses. The doctrine is "production and test code alike":
/// a raw `Command::new("git")` here would inherit a poisoned `GIT_*` env from
/// the parent (hook/editor/worktree) and could silently retarget the fixture
/// setup at the wrong repository.
fn run_git(repo: &Path, args: &[&str]) {
    let status = git_cmd()
        .args(args)
        .current_dir(repo)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("run git");
    assert!(status.success(), "git {args:?} failed");
}

/// Short HEAD sha of `repo`, read through the isolated git builder.
fn git_short_head(repo: &Path) -> String {
    let out = git_cmd()
        .args(["rev-parse", "--short", "HEAD"])
        .current_dir(repo)
        .output()
        .expect("rev-parse HEAD");
    assert!(
        out.status.success(),
        "rev-parse HEAD failed in {}: {}",
        repo.display(),
        String::from_utf8_lossy(&out.stderr).trim()
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// A fixture repo on branch `feature` (one commit) diffable against the named base.
fn fixture_repo_with_base(base_branch: &str, origin_head: bool) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path();
    run_git(repo, &["init", "-b", base_branch]);
    run_git(repo, &["config", "user.email", "test@test.com"]);
    run_git(repo, &["config", "user.name", "Test"]);
    std::fs::write(repo.join("a.txt"), "hello\n").unwrap();
    run_git(repo, &["add", "-A"]);
    run_git(repo, &["commit", "-m", "init"]);
    if origin_head {
        run_git(
            repo,
            &[
                "update-ref",
                &format!("refs/remotes/origin/{base_branch}"),
                "HEAD",
            ],
        );
        run_git(
            repo,
            &[
                "symbolic-ref",
                "refs/remotes/origin/HEAD",
                &format!("refs/remotes/origin/{base_branch}"),
            ],
        );
    }
    run_git(repo, &["checkout", "-b", "feature"]);
    std::fs::write(repo.join("a.txt"), "hello world\n").unwrap();
    run_git(repo, &["add", "-A"]);
    run_git(repo, &["commit", "-m", "feature change"]);
    dir
}

/// A fixture repo on branch `feature` (one commit) diffable against `main`.
fn fixture_repo() -> tempfile::TempDir {
    fixture_repo_with_base("main", false)
}

/// Run a synchronous quick review to completion, registering it under `home`.
fn run_quick_review(repo: &Path, home: &Path) {
    let status = Command::new(env!("CARGO_BIN_EXE_prview"))
        .current_dir(repo)
        .args([
            "--quick",
            "--no-fetch",
            "--local-only",
            "--no-dashboard",
            "--no-zip",
            "feature",
            "main",
        ])
        .env("PRVIEW_HOME", home)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("run prview --quick");
    // prview exits non-zero when the gate blocks; the pack is still produced.
    let _ = status;
}

/// Plant a synthetic completed run directory (RUN.json + SANITY.json +
/// MERGE_GATE + optional SARIF) under the standard storage layout, so
/// `verdict`/`findings` can be exercised without a real review. SANITY.json is
/// the finalization marker `run_status` keys completion on.
fn plant_completed_run(
    home: &Path,
    repo_name: &str,
    branch_key: &str,
    run_id: &str,
    merge_gate: &serde_json::Value,
    sarif: Option<&serde_json::Value>,
) -> std::path::PathBuf {
    let run_dir = home
        .join("runs")
        .join(repo_name)
        .join(branch_key)
        .join(run_id);
    let summary = run_dir.join("00_summary");
    std::fs::create_dir_all(&summary).unwrap();
    std::fs::write(summary.join("RUN.json"), "{}").unwrap();
    std::fs::write(summary.join("SANITY.json"), "{}").unwrap();
    std::fs::write(
        summary.join("MERGE_GATE.json"),
        serde_json::to_string_pretty(merge_gate).unwrap(),
    )
    .unwrap();
    if let Some(sarif) = sarif {
        let ctx = run_dir.join("30_context");
        std::fs::create_dir_all(&ctx).unwrap();
        std::fs::write(
            ctx.join("INLINE_FINDINGS.sarif"),
            serde_json::to_string_pretty(sarif).unwrap(),
        )
        .unwrap();
    }
    run_dir
}

fn repo_basename(repo: &Path) -> String {
    repo.file_name().unwrap().to_str().unwrap().to_string()
}

/// A live `prview mcp` process with an initialized MCP session.
struct McpSession {
    child: Child,
    reader: BufReader<ChildStdout>,
    next_id: i64,
}

impl McpSession {
    fn start(envs: &[(&str, &str)]) -> Self {
        Self::start_in(None, envs)
    }

    fn start_in(cwd: Option<&Path>, envs: &[(&str, &str)]) -> Self {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_prview"));
        cmd.arg("mcp")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        if let Some(dir) = cwd {
            cmd.current_dir(dir);
        }
        for (k, v) in envs {
            cmd.env(k, v);
        }
        let mut child = cmd.spawn().expect("spawn prview mcp");
        let reader = BufReader::new(child.stdout.take().unwrap());
        let mut session = Self {
            child,
            reader,
            next_id: 1,
        };
        session.initialize();
        session
    }

    fn send(&mut self, req: &serde_json::Value) {
        let stdin = self.child.stdin.as_mut().unwrap();
        writeln!(stdin, "{}", req).unwrap();
        stdin.flush().unwrap();
    }

    /// Send a request with the given id and read until its response arrives.
    fn request(&mut self, id: i64, method: &str, params: serde_json::Value) -> serde_json::Value {
        self.send(&serde_json::json!({
            "jsonrpc": "2.0", "id": id, "method": method, "params": params
        }));
        loop {
            let mut line = String::new();
            let n = self.reader.read_line(&mut line).expect("read line");
            assert!(n > 0, "server closed stdout before responding");
            if line.trim().is_empty() {
                continue;
            }
            let value: serde_json::Value = match serde_json::from_str(&line) {
                Ok(v) => v,
                Err(_) => continue,
            };
            if value.get("id").and_then(|v| v.as_i64()) == Some(id) {
                return value;
            }
        }
    }

    fn initialize(&mut self) -> serde_json::Value {
        let init = self.request(
            0,
            "initialize",
            serde_json::json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {"name": "test", "version": "0"}
            }),
        );
        self.send(&serde_json::json!({
            "jsonrpc": "2.0", "method": "notifications/initialized"
        }));
        init
    }

    fn list_tools(&mut self) -> serde_json::Value {
        let id = self.next_id;
        self.next_id += 1;
        self.request(id, "tools/list", serde_json::json!({}))
    }

    /// Call a tool and return the full `result` object (has `content`, `isError`).
    fn call_tool(&mut self, name: &str, arguments: serde_json::Value) -> serde_json::Value {
        let id = self.next_id;
        self.next_id += 1;
        let resp = self.request(
            id,
            "tools/call",
            serde_json::json!({"name": name, "arguments": arguments}),
        );
        resp["result"].clone()
    }
}

impl Drop for McpSession {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Parse the JSON body a tool wrote into its first text content block.
fn tool_body(result: &serde_json::Value) -> serde_json::Value {
    let text = result["content"][0]["text"]
        .as_str()
        .expect("tool result text content");
    serde_json::from_str(text).expect("tool body is JSON")
}

fn is_error(result: &serde_json::Value) -> bool {
    result["isError"].as_bool().unwrap_or(false)
}

fn repo_root() -> String {
    env!("CARGO_MANIFEST_DIR").to_string()
}

#[test]
fn mcp_initialize_and_lists_six_tools() {
    let mut s = McpSession::start(&[]);
    let init = s.request(
        0,
        "initialize",
        serde_json::json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": {"name": "test", "version": "0"}
        }),
    );
    assert_eq!(
        init["result"]["serverInfo"]["name"].as_str(),
        Some("prview"),
        "serverInfo.name should be prview: {init}"
    );

    let tools = s.list_tools();
    let names: Vec<&str> = tools["result"]["tools"]
        .as_array()
        .expect("tools array")
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    for expected in [
        "health",
        "state",
        "run_review",
        "verdict",
        "findings",
        "read_artifact",
    ] {
        assert!(names.contains(&expected), "missing tool {expected}");
    }
}

#[test]
fn health_without_repo_has_null_deps_repo() {
    let mut s = McpSession::start(&[]);
    let result = s.call_tool("health", serde_json::json!({}));
    assert!(!is_error(&result), "health must not error: {result}");
    let body = tool_body(&result);
    assert_eq!(body["schema_version"], "prview.mcp.v1");
    assert_eq!(body["protocol"], "prview.mcp.v1");
    assert!(body["version"].as_str().is_some());
    assert!(body["deps_global"]["git"].is_boolean());
    assert!(
        body["deps_repo"].is_null(),
        "deps_repo must be null: {body}"
    );
}

#[test]
fn health_with_repo_reports_profile() {
    let mut s = McpSession::start(&[]);
    let result = s.call_tool("health", serde_json::json!({"repo": repo_root()}));
    assert!(!is_error(&result));
    let body = tool_body(&result);
    // prview-rs is a Rust crate → profile Rust, cargo probed.
    assert_eq!(body["deps_repo"]["profile"], "Rust");
    assert!(body["deps_repo"]["tools"]["cargo"].is_boolean());
}

#[test]
fn state_on_repo_reports_branch() {
    let home = tempfile::tempdir().unwrap();
    let mut s = McpSession::start(&[("PRVIEW_HOME", home.path().to_str().unwrap())]);
    let result = s.call_tool("state", serde_json::json!({"repo": repo_root()}));
    assert!(!is_error(&result), "state must not error: {result}");
    let body = tool_body(&result);
    assert_eq!(body["schema_version"], "prview.mcp.v1");
    assert!(
        body["branch"]
            .as_str()
            .map(|b| !b.is_empty())
            .unwrap_or(false),
        "branch must be non-empty: {body}"
    );
    assert!(body["commit"].as_str().is_some());
    // Fresh PRVIEW_HOME → no runs recorded for HEAD.
    assert!(body["latest_run_for_head"].is_null());
}

#[test]
fn run_review_quick_completes_and_verdict_reads_it() {
    let home = tempfile::tempdir().unwrap();
    let repo = fixture_repo();
    let mut s = McpSession::start(&[("PRVIEW_HOME", home.path().to_str().unwrap())]);

    let result = s.call_tool(
        "run_review",
        serde_json::json!({"repo": repo.path().to_str().unwrap(), "profile": "quick"}),
    );
    assert!(
        !is_error(&result),
        "run_review quick must not error: {result}"
    );
    let body = tool_body(&result);
    assert_eq!(body["status"], "completed");
    assert_eq!(body["schema_version"], "prview.mcp.v1");
    let run_id = body["run_id"].as_str().expect("run_id").to_string();
    assert!(body["verdict"].as_str().is_some());
    assert!(body["stats"]["files_changed"].is_number());
    let pack = body["artifact_paths"]["pack"].as_str().expect("pack path");
    assert!(
        Path::new(pack)
            .join("00_summary")
            .join("MERGE_GATE.json")
            .exists(),
        "pack MERGE_GATE.json must exist on disk"
    );

    // verdict by explicit run_id resolves the same completed run.
    let v = s.call_tool(
        "verdict",
        serde_json::json!({"repo": repo.path().to_str().unwrap(), "run_id": run_id}),
    );
    let vbody = tool_body(&v);
    assert_eq!(vbody["status"], "completed");
    assert_eq!(vbody["run_id"], run_id);
}

#[test]
fn run_review_without_base_uses_origin_head_default_branch() {
    let home = tempfile::tempdir().unwrap();
    let repo = fixture_repo_with_base("visits-1404", true);
    let mut s = McpSession::start(&[("PRVIEW_HOME", home.path().to_str().unwrap())]);

    let state = tool_body(&s.call_tool(
        "state",
        serde_json::json!({"repo": repo.path().to_str().unwrap()}),
    ));
    assert_eq!(state["default_branch"], "visits-1404");
    assert_eq!(state["base_fallback"], false);

    let result = s.call_tool(
        "run_review",
        serde_json::json!({"repo": repo.path().to_str().unwrap(), "profile": "quick"}),
    );
    assert!(
        !is_error(&result),
        "run_review quick must not error: {result}"
    );
    let body = tool_body(&result);
    assert_eq!(body["status"], "completed");
    assert_eq!(body["base_used"], serde_json::json!(["visits-1404"]));
    assert_eq!(body["base_fallback"], false);
}

#[test]
fn run_review_without_detectable_default_uses_fallback_with_caveat() {
    let home = tempfile::tempdir().unwrap();
    let repo = fixture_repo();
    let mut s = McpSession::start(&[("PRVIEW_HOME", home.path().to_str().unwrap())]);

    let result = s.call_tool(
        "run_review",
        serde_json::json!({"repo": repo.path().to_str().unwrap(), "profile": "quick"}),
    );
    assert!(
        !is_error(&result),
        "run_review quick must not error: {result}"
    );
    let body = tool_body(&result);
    assert_eq!(body["status"], "completed");
    assert_eq!(body["base_used"], serde_json::json!(["main"]));
    assert_eq!(body["base_fallback"], true);
    let caveats = body["caveats"].as_array().expect("caveats");
    assert!(
        caveats.iter().any(|c| c
            .as_str()
            .map(|s| s.contains("base_fallback"))
            .unwrap_or(false)),
        "fallback caveat required: {body}"
    );
}

#[test]
fn run_review_deep_returns_running_then_completes() {
    let home = tempfile::tempdir().unwrap();
    let repo = fixture_repo();
    let mut s = McpSession::start(&[("PRVIEW_HOME", home.path().to_str().unwrap())]);

    let result = s.call_tool(
        "run_review",
        serde_json::json!({"repo": repo.path().to_str().unwrap(), "profile": "deep"}),
    );
    assert!(
        !is_error(&result),
        "run_review deep must not error: {result}"
    );
    let body = tool_body(&result);
    assert_eq!(body["status"], "running");
    let run_id = body["run_id"].as_str().expect("run_id").to_string();

    // Poll verdict until the detached deep run completes. This drives a real
    // detached subprocess (a full prview deep run), which alone takes ~18-22s
    // and is starved further under the parallel pre-push suite. Bound the wait
    // on a generous wall clock (not a fixed iteration count) with exponential
    // backoff, so a fast completion is caught promptly while a load-slowed run
    // still has headroom before the deadline. This is the one e2e deep-lifecycle
    // test and stays in the default suite.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
    let mut backoff = std::time::Duration::from_millis(250);
    let final_status = loop {
        let v = s.call_tool(
            "verdict",
            serde_json::json!({"repo": repo.path().to_str().unwrap(), "run_id": run_id}),
        );
        let vbody = tool_body(&v);
        let status = vbody["status"].as_str().unwrap_or("").to_string();
        if status == "completed" || std::time::Instant::now() >= deadline {
            break status;
        }
        std::thread::sleep(backoff);
        backoff = (backoff * 2).min(std::time::Duration::from_secs(3));
    };
    assert_eq!(
        final_status, "completed",
        "deep run must reach completed within the 120s deadline"
    );
}

#[test]
fn run_review_single_active_run_is_locked() {
    let home = tempfile::tempdir().unwrap();
    let repo = fixture_repo();

    // Plant a live RUNNING marker for this repo/branch (feature) so the guard
    // must reject a second run_review. pid = this test process (alive).
    let repo_name = Path::new(repo.path())
        .file_name()
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    let active_dir = home
        .path()
        .join("runs")
        .join(&repo_name)
        .join("feature")
        .join("20260101-000000");
    std::fs::create_dir_all(&active_dir).unwrap();
    let marker = serde_json::json!({
        "pid": std::process::id(),
        "started_at": "2026-07-01T00:00:00Z",
        "profile": "deep",
        "commit": "abc1234",
        "base_used": ["main"]
    });
    std::fs::write(active_dir.join("RUNNING.json"), marker.to_string()).unwrap();

    let mut s = McpSession::start(&[("PRVIEW_HOME", home.path().to_str().unwrap())]);
    let result = s.call_tool(
        "run_review",
        serde_json::json!({"repo": repo.path().to_str().unwrap(), "profile": "quick"}),
    );
    assert!(is_error(&result), "second run must be locked");
    let body = tool_body(&result);
    assert_eq!(body["error_class"], "storage_locked");
    assert_eq!(body["active_run_id"], "20260101-000000");
    assert!(body["retry_after_ms"].is_number());
}

#[test]
fn findings_and_read_artifact_on_completed_run() {
    let home = tempfile::tempdir().unwrap();
    let repo = fixture_repo();
    let mut s = McpSession::start(&[("PRVIEW_HOME", home.path().to_str().unwrap())]);
    let run = s.call_tool(
        "run_review",
        serde_json::json!({"repo": repo.path().to_str().unwrap(), "profile": "quick"}),
    );
    let run_id = tool_body(&run)["run_id"].as_str().unwrap().to_string();

    // findings: honest structured page (may be empty for a quick review).
    let f = s.call_tool(
        "findings",
        serde_json::json!({"repo": repo.path().to_str().unwrap(), "run_id": run_id}),
    );
    assert!(!is_error(&f), "findings must not error: {f}");
    let fbody = tool_body(&f);
    assert!(fbody["items"].is_array());
    assert!(fbody["total"].is_number());

    // read_artifact: MERGE_GATE.json is readable on a completed run.
    let a = s.call_tool(
        "read_artifact",
        serde_json::json!({
            "repo": repo.path().to_str().unwrap(),
            "run_id": run_id,
            "artifact": "00_summary/MERGE_GATE.json"
        }),
    );
    assert!(!is_error(&a), "read_artifact must not error: {a}");
    let abody = tool_body(&a);
    assert!(
        abody["content"]
            .as_str()
            .map(|c| !c.is_empty())
            .unwrap_or(false)
    );
    assert!(abody["total_lines"].is_number());

    // R5: path traversal is refused as artifact_missing, no read.
    let escape = s.call_tool(
        "read_artifact",
        serde_json::json!({
            "repo": repo.path().to_str().unwrap(),
            "run_id": run_id,
            "artifact": "../../../etc/passwd"
        }),
    );
    assert!(is_error(&escape));
    assert_eq!(tool_body(&escape)["error_class"], "artifact_missing");
}

#[test]
fn verdict_on_completed_run_reports_decision() {
    let home = tempfile::tempdir().unwrap();
    let repo = fixture_repo();
    run_quick_review(repo.path(), home.path());

    let mut s = McpSession::start(&[("PRVIEW_HOME", home.path().to_str().unwrap())]);
    let result = s.call_tool(
        "verdict",
        serde_json::json!({"repo": repo.path().to_str().unwrap()}),
    );
    assert!(!is_error(&result), "verdict must not error: {result}");
    let body = tool_body(&result);
    assert_eq!(body["schema_version"], "prview.mcp.v1");
    assert_eq!(body["status"], "completed");
    assert!(body["run_id"].as_str().is_some());
    assert!(
        body["commit"]
            .as_str()
            .map(|c| !c.is_empty())
            .unwrap_or(false)
    );
    assert!(body["merge_recommendation"].as_str().is_some());
    assert!(body["allow_merge"].is_boolean());
    assert!(body["base_used"].is_array());
    assert!(body["gates"].is_array());
}

#[test]
fn verdict_without_run_errors_run_not_found() {
    let home = tempfile::tempdir().unwrap();
    let repo = fixture_repo();
    // No review has been run under this PRVIEW_HOME.
    let mut s = McpSession::start(&[("PRVIEW_HOME", home.path().to_str().unwrap())]);
    let result = s.call_tool(
        "verdict",
        serde_json::json!({"repo": repo.path().to_str().unwrap()}),
    );
    assert!(is_error(&result));
    let body = tool_body(&result);
    assert_eq!(body["error_class"], "run_not_found");
}

#[test]
fn verdict_normalizes_inconsistent_core_decision() {
    // R1: core emits allow_merge:true alongside a block recommendation. The MCP
    // adapter must return a coherent pair (block + allow_merge:false) and a
    // core_inconsistency caveat carrying the originals.
    let home = tempfile::tempdir().unwrap();
    let repo = fixture_repo();
    let repo_name = repo_basename(repo.path());
    plant_completed_run(
        home.path(),
        &repo_name,
        "feature",
        "20260101-120000",
        &serde_json::json!({
            "bases": ["main"],
            "decision": {
                "merge_recommendation": "block",
                "verdict": "BLOCK",
                "allow_merge": true,
                "blocking_issues": ["forced"],
                "review_caveats": []
            }
        }),
        None,
    );

    let mut s = McpSession::start(&[("PRVIEW_HOME", home.path().to_str().unwrap())]);
    let v = s.call_tool(
        "verdict",
        serde_json::json!({"repo": repo.path().to_str().unwrap(), "run_id": "20260101-120000"}),
    );
    assert!(!is_error(&v), "verdict must not error: {v}");
    let body = tool_body(&v);
    assert_eq!(body["status"], "completed");
    assert_eq!(body["merge_recommendation"], "block");
    assert_eq!(body["allow_merge"], false, "allow_merge must be derived");
    let caveats = body["caveats"].as_array().expect("caveats");
    assert!(
        caveats.iter().any(|c| c
            .as_str()
            .map(|s| s.contains("core_inconsistency"))
            .unwrap_or(false)),
        "core_inconsistency caveat required: {body}"
    );
}

#[test]
fn verdict_after_new_commit_is_run_not_found() {
    // R3: a run recorded on commit A does not satisfy verdict once HEAD moves.
    let home = tempfile::tempdir().unwrap();
    let repo = fixture_repo();
    run_quick_review(repo.path(), home.path());

    // Move HEAD forward with a new commit.
    std::fs::write(repo.path().join("a.txt"), "hello world again\n").unwrap();
    run_git(repo.path(), &["add", "-A"]);
    run_git(repo.path(), &["commit", "-m", "second change"]);

    let mut s = McpSession::start(&[("PRVIEW_HOME", home.path().to_str().unwrap())]);
    let v = s.call_tool(
        "verdict",
        serde_json::json!({"repo": repo.path().to_str().unwrap()}),
    );
    assert!(is_error(&v), "stale HEAD must not yield a verdict");
    assert_eq!(tool_body(&v)["error_class"], "run_not_found");

    // state must also report no run for the new HEAD.
    let st = s.call_tool(
        "state",
        serde_json::json!({"repo": repo.path().to_str().unwrap()}),
    );
    assert!(tool_body(&st)["latest_run_for_head"].is_null());
}

#[test]
fn findings_pagination_closes_the_set_without_duplicates() {
    let home = tempfile::tempdir().unwrap();
    let repo = fixture_repo();
    let repo_name = repo_basename(repo.path());
    let sarif = serde_json::json!({
        "version": "2.1.0",
        "runs": [{
            "results": [
                {"ruleId": "r1", "level": "error",
                 "message": {"text": "m1"},
                 "locations": [{"physicalLocation": {"artifactLocation": {"uri": "src/a.rs"}, "region": {"startLine": 1}}}]},
                {"ruleId": "r2", "level": "warning",
                 "message": {"text": "m2"},
                 "locations": [{"physicalLocation": {"artifactLocation": {"uri": "src/a.rs"}, "region": {"startLine": 2}}}]},
                {"ruleId": "r3", "level": "warning",
                 "message": {"text": "m3"},
                 "locations": [{"physicalLocation": {"artifactLocation": {"uri": "src/b.rs"}, "region": {"startLine": 3}}}]}
            ]
        }]
    });
    plant_completed_run(
        home.path(),
        &repo_name,
        "feature",
        "20260101-130000",
        &serde_json::json!({"bases": ["main"], "decision": {"verdict": "PASS", "merge_recommendation": "approve", "allow_merge": true}}),
        Some(&sarif),
    );

    let mut s = McpSession::start(&[("PRVIEW_HOME", home.path().to_str().unwrap())]);
    let repo_arg = repo.path().to_str().unwrap().to_string();

    // Walk the full set one item per page.
    let mut seen: Vec<String> = Vec::new();
    let mut cursor: Option<String> = None;
    let mut total_seen = 0;
    loop {
        let mut args =
            serde_json::json!({"repo": repo_arg, "run_id": "20260101-130000", "limit": 1});
        if let Some(c) = &cursor {
            args["cursor"] = serde_json::json!(c);
        }
        let page = tool_body(&s.call_tool("findings", args));
        assert_eq!(page["total"], 3, "total stable across pages");
        let items = page["items"].as_array().unwrap();
        for it in items {
            seen.push(it["rule"].as_str().unwrap().to_string());
            total_seen += 1;
        }
        match page["next_cursor"].as_str() {
            Some(c) => cursor = Some(c.to_string()),
            None => break,
        }
    }
    assert_eq!(total_seen, 3, "cursor walk must cover the whole set once");
    seen.sort();
    seen.dedup();
    assert_eq!(seen.len(), 3, "no duplicates across pages");

    // Severity filter changes total to the post-filter set.
    let filtered = tool_body(&s.call_tool(
        "findings",
        serde_json::json!({"repo": repo_arg, "run_id": "20260101-130000", "severity": "warning"}),
    ));
    assert_eq!(filtered["total"], 2, "total reflects post-filter set");
}

#[test]
fn tools_list_is_fast() {
    let mut s = McpSession::start(&[]);
    // Warm the process, then measure a round trip.
    let _ = s.list_tools();
    let start = std::time::Instant::now();
    let _ = s.list_tools();
    let elapsed = start.elapsed();
    assert!(
        elapsed < std::time::Duration::from_millis(200),
        "tools/list latency {elapsed:?} exceeds 200ms budget"
    );
}

#[test]
fn server_works_from_foreign_cwd() {
    // Spawn-from-foreign-cwd: the server started with cwd = a scratch dir must
    // operate purely on the explicit `repo` arg, never on its own cwd.
    let home = tempfile::tempdir().unwrap();
    let scratch = tempfile::tempdir().unwrap();
    let repo = fixture_repo();
    let mut s = McpSession::start_in(
        Some(scratch.path()),
        &[("PRVIEW_HOME", home.path().to_str().unwrap())],
    );
    let result = s.call_tool(
        "state",
        serde_json::json!({"repo": repo.path().to_str().unwrap()}),
    );
    assert!(
        !is_error(&result),
        "state from foreign cwd must work: {result}"
    );
    assert_eq!(tool_body(&result)["branch"], "feature");
}

#[test]
fn fixture_git_is_hermetic_under_poisoned_env() {
    // git_cmd() strips inherited GIT_* so a poisoned parent env cannot retarget
    // git at the wrong repository. Prove it end-to-end in the contract suite:
    // with the MCP server's env poisoned to point GIT_DIR/GIT_WORK_TREE at a
    // *victim* repo, a `state` on the fixture must still report the fixture's
    // branch, and the victim must be left completely untouched.
    let home = tempfile::tempdir().unwrap();

    // Victim on a distinct branch: if the poison leaked, state would report it.
    let victim = tempfile::tempdir().unwrap();
    run_git(victim.path(), &["init", "-b", "victim-branch"]);
    run_git(victim.path(), &["config", "user.email", "victim@test.com"]);
    run_git(victim.path(), &["config", "user.name", "Victim"]);
    std::fs::write(victim.path().join("v.txt"), "victim\n").unwrap();
    run_git(victim.path(), &["add", "-A"]);
    run_git(victim.path(), &["commit", "-m", "victim init"]);
    let victim_head = git_short_head(victim.path());

    let repo = fixture_repo(); // branch "feature"
    let victim_git_dir = victim.path().join(".git");

    let mut s = McpSession::start(&[
        ("PRVIEW_HOME", home.path().to_str().unwrap()),
        ("GIT_DIR", victim_git_dir.to_str().unwrap()),
        ("GIT_WORK_TREE", victim.path().to_str().unwrap()),
    ]);
    let result = s.call_tool(
        "state",
        serde_json::json!({"repo": repo.path().to_str().unwrap()}),
    );
    assert!(
        !is_error(&result),
        "state under poisoned env must work: {result}"
    );
    assert_eq!(
        tool_body(&result)["branch"],
        "feature",
        "server must operate on the fixture, not the poisoned victim"
    );

    // Victim must be untouched: HEAD did not move.
    assert_eq!(
        git_short_head(victim.path()),
        victim_head,
        "victim HEAD must not move under a poisoned env"
    );
}

#[test]
fn state_on_relative_repo_errors_even_when_path_exists_under_cwd() {
    // Contract: the `repo` argument MUST be absolute; the server MUST NOT rely
    // on its own cwd. Spawn the server with a cwd that DOES contain a real git
    // repo under a relative path, then pass that relative path. It must still
    // fail loud (repo_not_found), proving the boundary rejects the path itself
    // rather than silently resolving it against the server's cwd.
    let scratch = tempfile::tempdir().unwrap();
    let nested = scratch.path().join("nested-repo");
    std::fs::create_dir_all(&nested).unwrap();
    run_git(&nested, &["init", "-b", "main"]);
    run_git(&nested, &["config", "user.email", "test@test.com"]);
    run_git(&nested, &["config", "user.name", "Test"]);
    std::fs::write(nested.join("a.txt"), "hello\n").unwrap();
    run_git(&nested, &["add", "-A"]);
    run_git(&nested, &["commit", "-m", "init"]);

    // Sanity: the relative path really does resolve to a git repo under cwd.
    assert!(scratch.path().join("nested-repo").join(".git").is_dir());

    let mut s = McpSession::start_in(Some(scratch.path()), &[]);
    let result = s.call_tool("state", serde_json::json!({"repo": "nested-repo"}));
    assert!(
        is_error(&result),
        "relative repo must be rejected: {result}"
    );
    let body = tool_body(&result);
    assert_eq!(body["error_class"], "repo_not_found");
    assert!(
        body["message"]
            .as_str()
            .map(|m| m.contains("absolute"))
            .unwrap_or(false),
        "message must name the absolute-path requirement: {body}"
    );
}

#[test]
fn state_on_missing_repo_errors_repo_not_found() {
    let mut s = McpSession::start(&[]);
    let result = s.call_tool(
        "state",
        serde_json::json!({"repo": "/tmp/prview-nonexistent-xyz"}),
    );
    assert!(is_error(&result), "must be a fail-loud error");
    let body = tool_body(&result);
    assert_eq!(body["error_class"], "repo_not_found");
    assert_eq!(body["schema_version"], "prview.mcp.v1");
}
