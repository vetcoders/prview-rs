//! Quality checks system
//!
//! Trait-based check system for running various quality tools.

use crate::cache::Cache;
use crate::config::{Config, ProfileKind};
use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::process::Output;
use std::str::FromStr;
use std::time::Duration;
use tokio::process::Command;
use tokio::sync::Semaphore;

/// Default timeout for checks (5 minutes — large Rust workspaces need this)
pub const CHECK_TIMEOUT_SECS: u64 = 300;

/// Timeout for test commands (15 minutes - ML projects with model loading need this)
pub const TEST_TIMEOUT_SECS: u64 = 900;

mod cargo;
mod python;
mod semgrep;
mod typescript;

pub use cargo::{
    CargoAuditCheck, CargoCheck, CargoGeigerCheck, CargoTestCheck, ClippyCheck, RustfmtCheck,
};
pub use python::{MypyCheck, PytestCheck, RuffCheck};
pub use semgrep::SemgrepCheck;
pub(crate) use semgrep::output_reports_scan_errors as semgrep_output_reports_scan_errors;
pub use typescript::{ESLintCheck, StylelintCheck, TypeScriptCheck, VitestCheck};

/// Provenance data for a check execution (Artifact Pack v1)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckProvenance {
    pub command: String,
    pub tool_version: Option<String>,
    pub cwd: String,
    pub exit_code: Option<i32>,
    pub started_at: String,
    pub finished_at: String,
    pub hard_fail_signatures: Vec<String>,
    pub cache_key: Option<String>,
}

/// A check that was configured but could not run
#[derive(Debug, Clone, Serialize)]
pub struct SkippedCheck {
    pub id: String,
    pub name: String,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckEligibility {
    Run,
    Skip(String),
}

/// Result of a check execution
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckResult {
    pub name: String,
    pub status: CheckStatus,
    pub duration: Duration,
    pub output: String,
    pub cached: bool,
    /// Provenance for Artifact Pack v1 (None for cached/legacy results)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provenance: Option<CheckProvenance>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CheckStatus {
    Passed,
    Failed,
    Warnings,
    Skipped,
    Error,
}

impl FromStr for CheckStatus {
    type Err = std::convert::Infallible;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s.to_lowercase().as_str() {
            "passed" => Self::Passed,
            "failed" => Self::Failed,
            "warnings" => Self::Warnings,
            "skipped" => Self::Skipped,
            _ => Self::Error,
        })
    }
}

impl CheckStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Passed => "passed",
            Self::Failed => "failed",
            Self::Warnings => "warnings",
            Self::Skipped => "skipped",
            Self::Error => "error",
        }
    }
}

/// Trait for implementing checks
#[async_trait]
pub trait Check: Send + Sync {
    /// Human-readable name
    fn name(&self) -> &str;

    /// Check if this check can run in current context
    fn check_eligibility(&self, config: &Config) -> CheckEligibility;

    /// Run the check
    async fn run(&self, config: &Config) -> Result<CheckResult>;

    /// Get cache key (None = not cacheable)
    fn cache_key(&self, _config: &Config) -> Option<String> {
        None
    }
}

/// Run all applicable checks with caching (parallel execution, streaming output).
pub async fn run_all(config: &Config) -> Result<(Vec<CheckResult>, Vec<SkippedCheck>)> {
    use colored::Colorize;
    use futures::stream::{FuturesUnordered, StreamExt};
    use std::io::Write;
    use std::sync::Arc;

    let checks: Vec<Box<dyn Check>> = get_checks_for_profile(config);
    let cache = Arc::new(Cache::new(config));
    let emit = !config.json && !config.quiet;

    if emit {
        println!("{}", "Running quality checks...".cyan());
        println!();
    }

    // Separate checks into cached, skipped, and runnable
    let mut results = Vec::new();
    let mut skipped = Vec::new();
    let mut runnable_checks = Vec::new();

    for check in checks {
        match check.check_eligibility(config) {
            CheckEligibility::Skip(reason) => {
                skipped.push(build_skipped_check(check.as_ref(), reason));
                continue;
            }
            CheckEligibility::Run => {}
        }

        if let Some(result) = load_cached_result(check.as_ref(), config, cache.as_ref()) {
            let status_str = format_status(result.status);
            if emit {
                println!("  {} {} (cached)", status_str, check.name());
            }
            results.push(result);
            continue;
        }

        runnable_checks.push(check);
    }

    // Pre-sync Python venv if any Python checks will run and uv is available.
    // This separates venv build time from the per-check timeout budget.
    let has_python_checks = runnable_checks
        .iter()
        .any(|c| matches!(c.name(), "Ruff" | "Mypy" | "Pytest"));
    if has_python_checks && config.profile.runs_python_checks() && which::which("uv").is_ok() {
        if emit {
            print!("  {} Syncing Python venv...", "●".blue());
            let _ = std::io::stdout().flush();
        }
        match run_command_with_timeout(
            "uv",
            &["sync", "--quiet"],
            &config.repo_root,
            CHECK_TIMEOUT_SECS,
        )
        .await
        {
            Ok(output) => {
                if emit {
                    if output.status.success() {
                        print!("\r\x1b[2K  {} Python venv ready\n", "✓".green());
                    } else {
                        let stderr = String::from_utf8_lossy(&output.stderr);
                        print!(
                            "\r\x1b[2K  {} uv sync failed: {}\n",
                            "⚠".yellow(),
                            stderr.lines().next().unwrap_or("unknown error")
                        );
                    }
                    let _ = std::io::stdout().flush();
                }
            }
            Err(e) => {
                if emit {
                    print!("\r\x1b[2K  {} uv sync: {}\n", "⚠".yellow(), e);
                    let _ = std::io::stdout().flush();
                }
            }
        }
    }

    if !runnable_checks.is_empty() {
        let mut remaining: Vec<String> = runnable_checks
            .iter()
            .map(|c| c.name().to_string())
            .collect();

        if emit {
            print!("  {} Running: {}", "●".blue(), remaining.join(", "));
            let _ = std::io::stdout().flush();
        }

        // Launch all checks in parallel, stream results as they complete.
        // Cargo checks share one target/ build lock, so they serialize on a
        // single-permit semaphore while non-cargo checks stay parallel (PV-17).
        let config = Arc::new(config.clone());
        let cargo_lock = Arc::new(Semaphore::new(1));
        let mut futs: FuturesUnordered<_> = runnable_checks
            .into_iter()
            .map(|check| {
                let config = Arc::clone(&config);
                let cache = Arc::clone(&cache);
                let cargo_lock = Arc::clone(&cargo_lock);
                async move {
                    let _permit = if is_cargo_target_check(check.name()) {
                        Some(
                            cargo_lock
                                .acquire()
                                .await
                                .expect("cargo-target semaphore never closed"),
                        )
                    } else {
                        None
                    };
                    execute_live_check(check, config.as_ref(), cache.as_ref()).await
                }
            })
            .collect();

        // Elapsed timer — ticks every second on the "Running" line
        let start = std::time::Instant::now();
        let mut timer = tokio::time::interval(tokio::time::Duration::from_secs(1));
        timer.tick().await; // consume immediate first tick

        // PV-18: soft thresholds at which we print a one-time "still running"
        // notice, so a slow run informs the user instead of hanging silently.
        // We never abort the run here — each check self-terminates at its own
        // timeout (PV-16); this only keeps the operator informed.
        const SLOW_NOTICE_THRESHOLDS_SECS: [u64; 3] = [60, 300, 900];
        let mut next_slow_notice = 0usize;

        loop {
            tokio::select! {
                biased;

                Some(result) = futs.next() => {
                    // Remove completed check from remaining list
                    remaining.retain(|n| n != &result.name);

                    if emit {
                        // Clear the "Running" line and print the result
                        print!("\r\x1b[2K");
                        let status_str = format_status(result.status);
                        println!(
                            "  {} {} ({:.1}s)",
                            status_str,
                            result.name,
                            result.duration.as_secs_f32(),
                        );

                        // Show updated "Running" line if checks remain
                        if !remaining.is_empty() {
                            let elapsed = start.elapsed().as_secs();
                            print!(
                                "  {} Running: {} ({}s)",
                                "●".blue(),
                                remaining.join(", "),
                                elapsed
                            );
                            let _ = std::io::stdout().flush();
                        }
                    }

                    results.push(result);

                    if remaining.is_empty() {
                        break;
                    }
                }

                _ = timer.tick(), if emit && !remaining.is_empty() => {
                    let elapsed = start.elapsed().as_secs();
                    // PV-18: when a run crosses a soft threshold, print a
                    // one-time note naming what's still running and how to bail.
                    // We inform, we do not abort — the checks own their timeouts.
                    if next_slow_notice < SLOW_NOTICE_THRESHOLDS_SECS.len()
                        && elapsed >= SLOW_NOTICE_THRESHOLDS_SECS[next_slow_notice]
                    {
                        next_slow_notice += 1;
                        println!(
                            "\r\x1b[2K  {} Still running after {}s: {}. Cargo checks compile \
                             the whole workspace and can take several minutes (each has its \
                             own timeout). Press Ctrl-C to abort.",
                            "ℹ".cyan(),
                            elapsed,
                            remaining.join(", "),
                        );
                    }
                    // Update elapsed time on the "Running" line
                    print!(
                        "\r\x1b[2K  {} Running: {} ({}s)",
                        "●".blue(),
                        remaining.join(", "),
                        elapsed
                    );
                    let _ = std::io::stdout().flush();
                }
            }
        }
    }

    if emit {
        println!();
    }

    Ok((results, skipped))
}

/// Callback type for check events (used by TUI)
pub type CheckEventCallback = Box<dyn Fn(CheckEvent) + Send + Sync>;

/// Events emitted during check execution
#[derive(Debug, Clone)]
pub enum CheckEvent {
    Started { name: String },
    Completed { result: Box<CheckResult> },
    Skipped { name: String },
}

/// Run all applicable checks with event callbacks (for TUI mode)
pub async fn run_all_with_events<F>(
    config: &Config,
    on_event: F,
) -> Result<(Vec<CheckResult>, Vec<SkippedCheck>)>
where
    F: Fn(CheckEvent) + Send + Sync,
{
    let checks: Vec<Box<dyn Check>> = get_checks_for_profile(config);
    let cache = Cache::new(config);
    let mut results = Vec::new();
    let mut skipped = Vec::new();
    let mut runnable_checks: Vec<Box<dyn Check>> = Vec::new();

    // First pass: resolve skipped/cached checks and collect runnable ones.
    for check in checks {
        match check.check_eligibility(config) {
            CheckEligibility::Skip(reason) => {
                let skipped_check = build_skipped_check(check.as_ref(), reason);
                let name = skipped_check.name.clone();
                skipped.push(skipped_check);
                on_event(CheckEvent::Skipped { name });
                continue;
            }
            CheckEligibility::Run => {}
        }

        if let Some(result) = load_cached_result(check.as_ref(), config, &cache) {
            on_event(CheckEvent::Completed {
                result: Box::new(result.clone()),
            });
            results.push(result);
            continue;
        }

        runnable_checks.push(check);
    }

    // Pre-sync Python venv before running checks, mirroring run_all behaviour.
    // This keeps venv build time outside the per-check timeout budget.
    let has_python_checks = runnable_checks
        .iter()
        .any(|c| matches!(c.name(), "Ruff" | "Mypy" | "Pytest"));
    if has_python_checks && config.profile.runs_python_checks() && which::which("uv").is_ok() {
        let _ = run_command_with_timeout(
            "uv",
            &["sync", "--quiet"],
            &config.repo_root,
            CHECK_TIMEOUT_SECS,
        )
        .await;
    }

    // Second pass: run checks in parallel, fire events as they complete.
    {
        use futures::stream::{FuturesUnordered, StreamExt};
        use std::sync::Arc;

        for check in &runnable_checks {
            on_event(CheckEvent::Started {
                name: check.name().to_string(),
            });
        }

        let config = Arc::new(config.clone());
        let cache = Arc::new(cache);
        // Cargo checks share one target/ build lock, so they serialize on a
        // single-permit semaphore while non-cargo checks stay parallel (PV-17).
        let cargo_lock = Arc::new(Semaphore::new(1));
        let mut futs: FuturesUnordered<_> = runnable_checks
            .into_iter()
            .map(|check| {
                let config = Arc::clone(&config);
                let cache = Arc::clone(&cache);
                let cargo_lock = Arc::clone(&cargo_lock);
                async move {
                    let _permit = if is_cargo_target_check(check.name()) {
                        Some(
                            cargo_lock
                                .acquire()
                                .await
                                .expect("cargo-target semaphore never closed"),
                        )
                    } else {
                        None
                    };
                    execute_live_check(check, config.as_ref(), cache.as_ref()).await
                }
            })
            .collect();

        while let Some(result) = futs.next().await {
            on_event(CheckEvent::Completed {
                result: Box::new(result.clone()),
            });
            results.push(result);
        }
    }

    Ok((results, skipped))
}

fn build_skipped_check(check: &dyn Check, reason: String) -> SkippedCheck {
    let name = check.name().to_string();
    let id = name.to_lowercase().replace([' ', '-', '/'], "_");

    SkippedCheck { id, name, reason }
}

fn load_cached_result(check: &dyn Check, config: &Config, cache: &Cache) -> Option<CheckResult> {
    let cache_key = check.cache_key(config)?;
    let cached = cache.get(check.name(), &cache_key)?;
    let output = cached.output.unwrap_or_default();
    let mut status = cached.status.parse::<CheckStatus>().unwrap();

    if matches!(status, CheckStatus::Passed | CheckStatus::Warnings) && has_tool_crash(&output) {
        status = CheckStatus::Error;
    }

    Some(CheckResult {
        name: check.name().to_string(),
        status,
        duration: Duration::from_secs(0),
        output,
        cached: true,
        provenance: None,
    })
}

async fn execute_live_check(check: Box<dyn Check>, config: &Config, cache: &Cache) -> CheckResult {
    let start = std::time::Instant::now();
    let name = check.name().to_string();
    let cache_key = check.cache_key(config);

    match check.run(config).await {
        Ok(mut result) => {
            if matches!(result.status, CheckStatus::Passed | CheckStatus::Warnings)
                && has_tool_crash(&result.output)
            {
                result.status = CheckStatus::Error;
            }

            // Never cache a runtime Skipped result. A check that RAN but
            // skipped (mypy: uv "failed to spawn" a missing binary; geiger: a
            // virtual workspace manifest) reflects an environmental/transient
            // setup gap, not a stable property of the source. Caching it under
            // the source-hash key (e.g. `mypy-<python_hash>`) would pin the
            // transient miss for the whole hash lifetime, so a later run with
            // the tool present still reports Skipped (PR #12 review #14).
            if result.status != CheckStatus::Skipped
                && let Some(key) = cache_key
                && let Err(e) = cache.set(&name, &key, result.status.as_str(), Some(&result.output))
            {
                eprintln!("  warning: cache write failed for {name}: {e}");
            }

            result
        }
        Err(e) => {
            let msg = e.to_string();
            // A missing/unlaunchable tool is a setup gap, not a quality failure,
            // so downgrade to Skipped to avoid poisoning the gate. EXCEPTION:
            // security tools stay loud (Error) — one that passed which::which()
            // at eligibility but then fails to spawn (broken/partial binary,
            // PATH change, TOCTOU) must not vanish silently.
            let status = if tool_unavailable_signature(&msg) && !is_security_check(&name) {
                CheckStatus::Skipped
            } else {
                CheckStatus::Error
            };
            CheckResult {
                name,
                status,
                duration: start.elapsed(),
                output: msg,
                cached: false,
                provenance: None,
            }
        }
    }
}

fn format_status(status: CheckStatus) -> String {
    use colored::Colorize;
    match status {
        CheckStatus::Passed => "✓".green().to_string(),
        CheckStatus::Failed => "✗".red().to_string(),
        CheckStatus::Warnings => "⚠".yellow().to_string(),
        CheckStatus::Skipped => "○".dimmed().to_string(),
        CheckStatus::Error => "!".red().to_string(),
    }
}

/// Get checks supported by the detected profile.
///
/// Individual checks decide whether they can execute in the current run via
/// `Check::can_run`, which preserves explicit opt-out flows while keeping
/// canonical status outputs complete.
pub fn get_checks_for_profile(config: &Config) -> Vec<Box<dyn Check>> {
    let mut checks: Vec<Box<dyn Check>> = Vec::new();

    // Security scans are applicable to all profiles
    checks.push(Box::new(SemgrepCheck));

    match config.profile.kind {
        ProfileKind::Js => {
            checks.push(Box::new(TypeScriptCheck));
            checks.push(Box::new(ESLintCheck));
            checks.push(Box::new(StylelintCheck));
            checks.push(Box::new(VitestCheck));
        }
        ProfileKind::Rust => {
            checks.push(Box::new(CargoCheck));
            checks.push(Box::new(ClippyCheck));
            checks.push(Box::new(RustfmtCheck));
            checks.push(Box::new(CargoTestCheck));
            checks.push(Box::new(CargoAuditCheck));
            if config.security_full {
                checks.push(Box::new(CargoGeigerCheck));
            }
        }
        ProfileKind::Python => {
            checks.push(Box::new(RuffCheck));
            checks.push(Box::new(MypyCheck));
            checks.push(Box::new(PytestCheck));
        }
        ProfileKind::Mixed => {
            if config.profile.has_tsconfig {
                checks.push(Box::new(TypeScriptCheck));
            }
            if config.profile.has_package_json {
                checks.push(Box::new(ESLintCheck));
                checks.push(Box::new(StylelintCheck));
                checks.push(Box::new(VitestCheck));
            }
            if config.profile.has_cargo {
                checks.push(Box::new(CargoCheck));
                checks.push(Box::new(ClippyCheck));
                checks.push(Box::new(RustfmtCheck));
                checks.push(Box::new(CargoTestCheck));
                checks.push(Box::new(CargoAuditCheck));
                if config.security_full {
                    checks.push(Box::new(CargoGeigerCheck));
                }
            }
            if config.profile.runs_python_checks() {
                checks.push(Box::new(RuffCheck));
                checks.push(Box::new(MypyCheck));
                checks.push(Box::new(PytestCheck));
            }
        }
        ProfileKind::Generic => {}
    }

    checks
}

/// Hard failure signature patterns (Artifact Pack v1 spec)
const HARD_FAIL_SIGNATURES: &[(&str, &str)] = &[
    // Rust
    ("thread '", "Rust panic"),
    // Node
    ("unhandledpromiserejection", "Node unhandled rejection"),
    ("err_unhandled_rejection", "Node unhandled rejection"),
    // Python
    ("traceback (most recent call last):", "Python traceback"),
    // General
    ("segmentation fault", "Segfault"),
    ("addresssanitizer", "ASan"),
    ("ubsan:", "UBSan"),
    ("threadsanitizer", "TSan"),
    ("sigabrt", "SIGABRT"),
    ("sigsegv", "SIGSEGV"),
    ("fatal runtime error", "Fatal runtime error"),
    ("stack overflow", "Stack overflow"),
];

/// Detect tool crash indicators in combined output
pub fn has_tool_crash(output: &str) -> bool {
    !find_hard_fail_signatures(output).is_empty()
}

/// Cargo checks share one target/ build lock, so serialize them (PV-17).
fn is_cargo_target_check(name: &str) -> bool {
    matches!(
        name,
        "Cargo check" | "Clippy" | "Rustfmt" | "Cargo test" | "Cargo audit" | "Cargo geiger"
    )
}

/// Security checks stay loud: a spawn failure here is NOT downgraded to Skipped
/// (PV-01), so a broken or half-installed security tool can't silently vanish
/// from the gate. They pass which::which() at eligibility, but a runtime spawn
/// failure (broken/partial binary, PATH change, TOCTOU) must still surface.
fn is_security_check(name: &str) -> bool {
    matches!(name, "Semgrep scan" | "Cargo audit" | "Cargo geiger")
}

/// Find all matching hard failure signatures in output
pub fn find_hard_fail_signatures(output: &str) -> Vec<String> {
    let mut found = Vec::new();
    for line in output.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("test ") {
            continue;
        }

        let lower = trimmed.to_ascii_lowercase();
        for &(pattern, label) in HARD_FAIL_SIGNATURES {
            let matched = if pattern == "thread '" {
                lower.contains("panic") && lower.contains(pattern)
            } else {
                lower.contains(pattern)
            };

            if matched && !found.iter().any(|existing| existing == label) {
                found.push(label.to_string());
            }
        }
    }
    found
}

/// True when prview's OWN process-runner error string indicates the tool binary
/// could not be launched (ENOENT on spawn) — e.g.
/// "Failed to run mypy: No such file or directory (os error 2)". Safe ONLY for
/// prview-generated error strings, where a match reliably means a spawn failure.
/// Do NOT run this against raw tool output: a tool legitimately prints
/// "no such file or directory" in its own diagnostics — use
/// `tool_spawn_failure_in_output` for that case.
pub fn tool_unavailable_signature(err: &str) -> bool {
    let lower = err.to_ascii_lowercase();
    lower.contains("no such file or directory")
        || lower.contains("failed to spawn")
        || lower.contains("command not found")
        || lower.contains("program not found")
        || lower.contains("cannot find the file specified")
}

/// True when RAW TOOL OUTPUT shows a launcher could not spawn the requested tool.
/// Matches only markers a tool never emits in its own diagnostics — uv's
/// "failed to spawn" and a shell "command not found". A bare
/// "no such file or directory" is deliberately NOT matched here: tools print it
/// in genuine diagnostics, and matching it would turn a real failure into an
/// invisible pass (a tool-output false positive).
pub fn tool_spawn_failure_in_output(output: &str) -> bool {
    let lower = output.to_ascii_lowercase();
    lower.contains("failed to spawn") || lower.contains("command not found")
}

/// Build provenance from a command execution
pub struct ProvenanceBuilder<'a> {
    pub cmd: &'a str,
    pub args: &'a [&'a str],
    pub cwd: &'a Path,
    pub output: &'a Output,
    pub combined_output: &'a str,
    pub started_at: &'a str,
    pub finished_at: &'a str,
    pub cache_key: Option<String>,
}

impl<'a> ProvenanceBuilder<'a> {
    pub fn build(self) -> CheckProvenance {
        self.build_with_repo_root(None)
    }

    pub fn build_with_repo_root(self, repo_root: Option<&Path>) -> CheckProvenance {
        let cwd_str = self.cwd.display().to_string();
        let cwd_display = match repo_root {
            Some(root) => crate::paths::normalize_path_display(&cwd_str, root),
            None => cwd_str,
        };
        CheckProvenance {
            command: format!("{} {}", self.cmd, self.args.join(" ")),
            tool_version: None,
            cwd: cwd_display,
            exit_code: self.output.status.code(),
            started_at: self.started_at.to_string(),
            finished_at: self.finished_at.to_string(),
            hard_fail_signatures: find_hard_fail_signatures(self.combined_output),
            cache_key: self.cache_key,
        }
    }
}

/// Marker embedded in a command-timeout error. Shared so callers that detect a
/// timeout (e.g. cargo geiger's Skipped downgrade) match against one source of
/// truth instead of coupling to free text that a reword could silently break.
const TIMEOUT_MARKER: &str = "timed out after";

/// Build the error for a command that exceeded its timeout.
fn timeout_error(cmd: &str, timeout_secs: u64) -> anyhow::Error {
    anyhow::anyhow!("{} {} {}s", cmd, TIMEOUT_MARKER, timeout_secs)
}

/// Whether an error is a command timeout produced by [`timeout_error`].
pub fn is_timeout_error(err: &anyhow::Error) -> bool {
    err.to_string().contains(TIMEOUT_MARKER)
}

/// Helper to run a command with timeout
pub async fn run_command(cmd: &str, args: &[&str], cwd: &Path) -> Result<Output> {
    run_command_with_timeout(cmd, args, cwd, CHECK_TIMEOUT_SECS).await
}

/// Helper to run a command with custom timeout
pub async fn run_command_with_timeout(
    cmd: &str,
    args: &[&str],
    cwd: &Path,
    timeout_secs: u64,
) -> Result<Output> {
    let mut command = Command::new(cmd);
    command.args(args).current_dir(cwd);
    // Shared rails (stdin-null, kill_on_drop, own process group) + concurrent
    // output drain + group-SIGKILL on timeout live in crate::proc.
    crate::proc::run_capture_with_timeout(command, Duration::from_secs(timeout_secs), cmd, || {
        timeout_error(cmd, timeout_secs)
    })
    .await
}

/// Helper to run JS tools via pnpm or npx (with tool availability check)
pub async fn run_js_command(tool: &str, args: &[&str], cwd: &Path) -> Result<Output> {
    run_js_command_with_timeout(tool, args, cwd, CHECK_TIMEOUT_SECS).await
}

/// Helper to run JS tools with custom timeout (for tests)
pub async fn run_js_command_with_timeout(
    tool: &str,
    args: &[&str],
    cwd: &Path,
    timeout_secs: u64,
) -> Result<Output> {
    // Build full args list
    let pnpm_args: Vec<&str> = std::iter::once("exec")
        .chain(std::iter::once(tool))
        .chain(args.iter().copied())
        .collect();

    // --no-install: a missing tool must fail fast and parseably, never reach
    // npm's interactive "Ok to proceed?" prompt (the --deep hang class).
    let npx_args: Vec<&str> = ["--no-install", tool]
        .into_iter()
        .chain(args.iter().copied())
        .collect();

    // Prefer a resolved local binary: a direct exec with no launcher, no npm
    // registry consult, and no prompt (PR #12 review #15/#17). Fall back to
    // pnpm exec, then npx --no-install, only when the tool is not installed
    // locally.
    if let Some(bin) = local_js_bin(tool, cwd) {
        let bin = bin.to_string_lossy().into_owned();
        run_command_with_timeout(&bin, args, cwd, timeout_secs).await
    } else if which::which("pnpm").is_ok() {
        run_command_with_timeout("pnpm", &pnpm_args, cwd, timeout_secs).await
    } else {
        run_command_with_timeout("npx", &npx_args, cwd, timeout_secs).await
    }
}

/// Resolve a JS tool to a directly-runnable local binary, bypassing npx.
///
/// `npx --no-install` still consults npm and, on some npm versions, can prompt
/// or hit the network; a resolved `node_modules/.bin/<tool>` is an unambiguous
/// local exec with neither. Returns None when the tool is not installed locally
/// (the caller then falls back to pnpm/npx) (PR #12 review #15/#17).
pub fn local_js_bin(tool: &str, cwd: &Path) -> Option<std::path::PathBuf> {
    let bin = cwd.join("node_modules/.bin").join(tool);
    bin.exists().then_some(bin)
}

/// Check if a JS tool is available in node_modules
pub fn js_tool_available(tool: &str, cwd: &Path) -> bool {
    local_js_bin(tool, cwd).is_some()
}

/// A resolved plan for running a check.
pub struct CheckPlan {
    /// Directory to run the check command in.
    pub scan_dir: std::path::PathBuf,
    /// Ephemeral worktree snapshot, kept alive until the check finishes.
    pub _snapshot: Option<crate::git::WorktreeSnapshot>,
}

/// Plan check execution path: if we are in a remote/PR mode (meaning resolved target
/// commit is different from the checked-out HEAD commit), create an ephemeral worktree
/// snapshot of the target commit and run there. Otherwise, scan the working tree in place.
pub fn plan_check_run(config: &Config) -> Result<CheckPlan> {
    let repo_root = config.repo_root.clone();
    let repo = match crate::git::Repository::open(&repo_root) {
        Ok(repo) => repo,
        Err(_) => {
            return Ok(CheckPlan {
                scan_dir: repo_root,
                _snapshot: None,
            });
        }
    };

    let (Ok(target), Ok(head)) = (repo.resolve_target(config), repo.head_commit_id()) else {
        return Ok(CheckPlan {
            scan_dir: repo_root,
            _snapshot: None,
        });
    };

    if head == target.commit_id {
        return Ok(CheckPlan {
            scan_dir: repo_root,
            _snapshot: None,
        });
    }

    // Ephemeral worktree
    let snapshot = crate::git::create_worktree_snapshot(&repo_root, &target.commit_id)?;
    Ok(CheckPlan {
        scan_dir: snapshot.worktree_path.clone(),
        _snapshot: Some(snapshot),
    })
}

impl CheckResult {
    pub fn is_failure(&self) -> bool {
        matches!(self.status, CheckStatus::Failed | CheckStatus::Error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::ExecutionMode;
    use crate::config::{Config, test_config, test_rust_profile};
    use std::time::Duration;

    fn rust_config(run_tests: bool, run_lint: bool, run_security: bool) -> Config {
        let mut config = test_config();
        config.profile = test_rust_profile(true);
        config.execution_mode = ExecutionMode::Standard;
        config.run_tests = run_tests;
        config.run_lint = run_lint;
        config.run_security = run_security;
        config.do_fetch = false;
        config.use_cache = false;
        config.create_zip = false;
        config
    }

    #[test]
    fn test_check_status_from_str_passed() {
        assert_eq!(
            "passed".parse::<CheckStatus>().unwrap(),
            CheckStatus::Passed
        );
        assert_eq!(
            "PASSED".parse::<CheckStatus>().unwrap(),
            CheckStatus::Passed
        );
    }

    #[test]
    fn test_check_status_from_str_failed() {
        assert_eq!(
            "failed".parse::<CheckStatus>().unwrap(),
            CheckStatus::Failed
        );
    }

    #[test]
    fn test_check_status_from_str_warnings() {
        assert_eq!(
            "warnings".parse::<CheckStatus>().unwrap(),
            CheckStatus::Warnings
        );
    }

    #[test]
    fn test_check_status_from_str_skipped() {
        assert_eq!(
            "skipped".parse::<CheckStatus>().unwrap(),
            CheckStatus::Skipped
        );
    }

    #[test]
    fn test_check_status_from_str_unknown() {
        assert_eq!(
            "unknown".parse::<CheckStatus>().unwrap(),
            CheckStatus::Error
        );
    }

    #[test]
    fn test_check_status_as_str() {
        assert_eq!(CheckStatus::Passed.as_str(), "passed");
        assert_eq!(CheckStatus::Failed.as_str(), "failed");
        assert_eq!(CheckStatus::Warnings.as_str(), "warnings");
        assert_eq!(CheckStatus::Skipped.as_str(), "skipped");
        assert_eq!(CheckStatus::Error.as_str(), "error");
    }

    #[test]
    fn test_check_result_is_failure_failed() {
        let result = CheckResult {
            name: "test".to_string(),
            status: CheckStatus::Failed,
            duration: Duration::from_secs(1),
            output: String::new(),
            cached: false,
            provenance: None,
        };
        assert!(result.is_failure());
    }

    #[test]
    fn test_check_result_is_failure_error() {
        let result = CheckResult {
            name: "test".to_string(),
            status: CheckStatus::Error,
            duration: Duration::from_secs(1),
            output: String::new(),
            cached: false,
            provenance: None,
        };
        assert!(result.is_failure());
    }

    #[test]
    fn is_timeout_error_matches_the_shared_constructor() {
        // The cargo geiger Skipped downgrade matches timeouts via is_timeout_error;
        // keep it coupled to the one constructor so a reword can't silently break
        // it. A rename on either side trips this test.
        assert!(is_timeout_error(&timeout_error("cargo", 600)));
        assert!(!is_timeout_error(&anyhow::anyhow!(
            "Failed to run cargo: No such file"
        )));
    }

    #[test]
    fn test_check_result_is_failure_passed() {
        let result = CheckResult {
            name: "test".to_string(),
            status: CheckStatus::Passed,
            duration: Duration::from_secs(1),
            output: String::new(),
            cached: false,
            provenance: None,
        };
        assert!(!result.is_failure());
    }

    #[test]
    fn test_check_result_is_failure_warnings() {
        let result = CheckResult {
            name: "test".to_string(),
            status: CheckStatus::Warnings,
            duration: Duration::from_secs(1),
            output: String::new(),
            cached: false,
            provenance: None,
        };
        assert!(!result.is_failure());
    }

    #[test]
    fn test_check_result_is_failure_skipped() {
        let result = CheckResult {
            name: "test".to_string(),
            status: CheckStatus::Skipped,
            duration: Duration::from_secs(1),
            output: String::new(),
            cached: false,
            provenance: None,
        };
        assert!(!result.is_failure());
    }

    #[test]
    fn test_check_status_serialization() {
        let passed = CheckStatus::Passed;
        let serialized = serde_json::to_string(&passed).unwrap();
        assert_eq!(serialized, "\"passed\"");

        let failed = CheckStatus::Failed;
        let serialized = serde_json::to_string(&failed).unwrap();
        assert_eq!(serialized, "\"failed\"");
    }

    #[test]
    fn test_check_status_deserialization() {
        let passed: CheckStatus = serde_json::from_str("\"passed\"").unwrap();
        assert_eq!(passed, CheckStatus::Passed);

        let failed: CheckStatus = serde_json::from_str("\"failed\"").unwrap();
        assert_eq!(failed, CheckStatus::Failed);
    }

    #[test]
    fn test_check_result_serialization() {
        let result = CheckResult {
            name: "test".to_string(),
            status: CheckStatus::Passed,
            duration: Duration::from_secs(5),
            output: "output".to_string(),
            cached: true,
            provenance: None,
        };
        let serialized = serde_json::to_string(&result).unwrap();
        assert!(serialized.contains("\"name\":\"test\""));
        assert!(serialized.contains("\"status\":\"passed\""));
        assert!(serialized.contains("\"cached\":true"));
    }

    #[test]
    fn test_check_result_clone() {
        let result = CheckResult {
            name: "test".to_string(),
            status: CheckStatus::Passed,
            duration: Duration::from_secs(1),
            output: "output".to_string(),
            cached: false,
            provenance: None,
        };
        let cloned = result.clone();
        assert_eq!(result.name, cloned.name);
        assert_eq!(result.status, cloned.status);
        assert_eq!(result.cached, cloned.cached);
    }

    #[test]
    fn test_check_event_started_clone() {
        let event = CheckEvent::Started {
            name: "test".to_string(),
        };
        let cloned = event.clone();
        match cloned {
            CheckEvent::Started { name } => assert_eq!(name, "test"),
            _ => panic!("wrong event type"),
        }
    }

    #[test]
    fn test_check_event_skipped_clone() {
        let event = CheckEvent::Skipped {
            name: "test".to_string(),
        };
        let cloned = event.clone();
        match cloned {
            CheckEvent::Skipped { name } => assert_eq!(name, "test"),
            _ => panic!("wrong event type"),
        }
    }

    #[test]
    fn test_check_event_completed_clone() {
        let result = CheckResult {
            name: "test".to_string(),
            status: CheckStatus::Passed,
            duration: Duration::from_secs(1),
            output: String::new(),
            cached: false,
            provenance: None,
        };
        let event = CheckEvent::Completed {
            result: Box::new(result.clone()),
        };
        let cloned = event.clone();
        match cloned {
            CheckEvent::Completed { result } => assert_eq!(result.name, "test"),
            _ => panic!("wrong event type"),
        }
    }

    #[test]
    fn test_format_status_passed() {
        let status = format_status(CheckStatus::Passed);
        assert!(status.contains('✓') || status.contains("✓"));
    }

    #[test]
    fn test_format_status_failed() {
        let status = format_status(CheckStatus::Failed);
        assert!(status.contains('✗') || status.contains("✗"));
    }

    #[test]
    fn test_format_status_warnings() {
        let status = format_status(CheckStatus::Warnings);
        assert!(status.contains('⚠') || status.contains("⚠"));
    }

    #[test]
    fn test_format_status_skipped() {
        let status = format_status(CheckStatus::Skipped);
        assert!(status.contains('○') || status.contains("○"));
    }

    #[test]
    fn test_format_status_error() {
        let status = format_status(CheckStatus::Error);
        assert!(status.contains('!') || status.contains("!"));
    }

    #[test]
    fn test_get_checks_for_profile_keeps_rust_checks_visible_when_disabled() {
        let config = rust_config(false, false, false);
        let check_names: Vec<String> = get_checks_for_profile(&config)
            .into_iter()
            .map(|check| check.name().to_string())
            .collect();

        assert!(check_names.iter().any(|name| name == "Cargo check"));
        assert!(check_names.iter().any(|name| name == "Clippy"));
        assert!(check_names.iter().any(|name| name == "Cargo test"));
        // cargo geiger is opt-in via --security-full: cleanly absent from the
        // default profile, never a skipped-caveat.
        assert!(!check_names.iter().any(|name| name == "Cargo geiger"));
    }

    #[test]
    fn test_get_checks_for_profile_adds_geiger_with_security_full() {
        let mut config = rust_config(false, false, false);
        config.security_full = true;
        let check_names: Vec<String> = get_checks_for_profile(&config)
            .into_iter()
            .map(|check| check.name().to_string())
            .collect();

        assert!(check_names.iter().any(|name| name == "Cargo geiger"));
    }

    #[test]
    fn test_gate_profile_keeps_rust_gate_fast_and_geiger_out() {
        let mut config = rust_config(true, true, true);
        config.execution_mode = ExecutionMode::Deep;
        config.security_full = true;
        config.apply_gate_profile();

        let checks = get_checks_for_profile(&config);
        let check_names: Vec<String> = checks
            .iter()
            .map(|check| check.name().to_string())
            .collect();

        assert_eq!(config.execution_mode, ExecutionMode::Quick);
        assert!(!config.run_tests);
        assert!(!config.run_lint);
        assert!(!config.run_security);
        assert!(!config.run_heuristics);
        assert!(!config.security_full);

        assert!(check_names.iter().any(|name| name == "Cargo check"));
        assert!(check_names.iter().any(|name| name == "Clippy"));
        assert!(check_names.iter().any(|name| name == "Rustfmt"));
        assert!(check_names.iter().any(|name| name == "Cargo test"));
        assert!(check_names.iter().any(|name| name == "Cargo audit"));
        assert!(!check_names.iter().any(|name| name == "Cargo geiger"));

        let cargo_check = checks
            .iter()
            .find(|check| check.name() == "Cargo check")
            .expect("cargo check configured");
        assert!(matches!(
            cargo_check.check_eligibility(&config),
            CheckEligibility::Run
        ));

        for name in ["Clippy", "Rustfmt", "Cargo test", "Cargo audit"] {
            let check = checks
                .iter()
                .find(|check| check.name() == name)
                .expect("rust check configured");
            assert!(matches!(
                check.check_eligibility(&config),
                CheckEligibility::Skip(_)
            ));
        }
    }

    #[test]
    fn test_get_checks_for_profile_mixed_geiger_follows_security_full() {
        use crate::config::{DetectedProfile, ProfileKind, test_config_builder};
        use std::path::PathBuf;

        let mixed_cargo = || DetectedProfile {
            kind: ProfileKind::Mixed,
            has_package_json: false,
            has_tsconfig: false,
            has_cargo: true,
            has_pyproject: false,
            has_python_source: false,
            has_js_source: false,
            cargo_root: Some(PathBuf::from(".")),
            rust_dirs: vec![PathBuf::from(".")],
            is_workspace: false,
        };

        // Default: geiger absent from a mixed cargo profile.
        let default_cfg = test_config_builder().profile(mixed_cargo()).build();
        let default_names: Vec<String> = get_checks_for_profile(&default_cfg)
            .into_iter()
            .map(|check| check.name().to_string())
            .collect();
        assert!(!default_names.iter().any(|name| name == "Cargo geiger"));

        // With --security-full: geiger joins the mixed profile.
        let full_cfg = test_config_builder()
            .profile(mixed_cargo())
            .security_full(true)
            .build();
        let full_names: Vec<String> = get_checks_for_profile(&full_cfg)
            .into_iter()
            .map(|check| check.name().to_string())
            .collect();
        assert!(full_names.iter().any(|name| name == "Cargo geiger"));
    }

    #[test]
    fn test_get_checks_for_profile_runs_python_for_pyproject_only_in_mixed_rust_repo() {
        // PR #12 review #11: a pyproject.toml is an explicit Python project
        // declaration, so a mixed Rust+Python repo that declares pyproject (even
        // without .py source yet) must run Ruff/Mypy/Pytest, not silently drop
        // them. (Reverses the earlier PV-05 "tooling-only pyproject does not
        // qualify" stance for the declared-project case.)
        use crate::config::{DetectedProfile, ProfileKind, test_config_builder};
        use std::path::PathBuf;

        let config = test_config_builder()
            .profile(DetectedProfile {
                kind: ProfileKind::Mixed,
                has_package_json: false,
                has_tsconfig: false,
                has_cargo: true,
                has_pyproject: true,
                has_python_source: false,
                has_js_source: false,
                cargo_root: Some(PathBuf::from(".")),
                rust_dirs: vec![PathBuf::from(".")],
                is_workspace: false,
            })
            .run_lint(true)
            .run_tests(true)
            .build();
        let check_names: Vec<String> = get_checks_for_profile(&config)
            .into_iter()
            .map(|check| check.name().to_string())
            .collect();

        assert!(check_names.iter().any(|name| name == "Cargo check"));
        assert!(check_names.iter().any(|name| name == "Ruff"));
        assert!(check_names.iter().any(|name| name == "Mypy"));
        assert!(check_names.iter().any(|name| name == "Pytest"));
    }

    #[test]
    fn test_has_tool_crash_rust_panic() {
        let output = "thread 'main' panicked at 'index out of bounds'";
        assert!(has_tool_crash(output));
    }

    #[test]
    fn test_has_tool_crash_segfault() {
        assert!(has_tool_crash("Segmentation fault (core dumped)"));
    }

    #[test]
    fn test_has_tool_crash_sigabrt() {
        assert!(has_tool_crash("Process received SIGABRT"));
    }

    #[test]
    fn test_has_tool_crash_stack_overflow() {
        assert!(has_tool_crash("fatal runtime error: stack overflow"));
    }

    #[test]
    fn test_has_tool_crash_clean_output() {
        assert!(!has_tool_crash("All checks passed successfully"));
    }

    #[test]
    fn test_has_tool_crash_panic_word_without_thread() {
        // "panic" alone without "thread '" should not trigger
        assert!(!has_tool_crash("Don't panic, everything is fine"));
    }

    #[test]
    fn test_has_tool_crash_ignores_test_harness_names() {
        let output = "\
test checks::tests::test_has_tool_crash_sigabrt ... ok
test checks::tests::test_has_tool_crash_rust_panic ... ok
test result: ok. 2 passed; 0 failed
";
        assert!(!has_tool_crash(output));
    }

    // ── PV-16: process-tree kill on timeout ─────────────────────────
    // The grandchild process-group kill is proven canonically in
    // crate::proc::tests; here we only guard that the checks public fn routes a
    // timeout through the shared helper (returns a timeout error, no hang).

    #[tokio::test]
    async fn test_run_command_with_timeout_public_fn_times_out() {
        // Public-fn shape: a long sleep with a 1s budget must return a timeout
        // error (not hang, not Ok).
        let tmp = tempfile::tempdir().expect("tempdir");
        let result = run_command_with_timeout("sleep", &["30"], tmp.path(), 1).await;
        let err = result.expect_err("sleep 30 with 1s timeout must error");
        assert!(is_timeout_error(&err), "error should be a timeout: {err}");
    }

    #[tokio::test]
    async fn test_run_command_with_timeout_success_returns_output() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let output = run_command_with_timeout("echo", &["hello"], tmp.path(), 10)
            .await
            .expect("echo should succeed");
        assert!(output.status.success(), "echo should exit 0");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            stdout.contains("hello"),
            "stdout should contain hello: {stdout}"
        );
    }

    // ── PV-17: cargo-family serialization ───────────────────────────

    #[test]
    fn test_is_cargo_target_check() {
        for name in [
            "Cargo check",
            "Clippy",
            "Rustfmt",
            "Cargo test",
            "Cargo audit",
            "Cargo geiger",
        ] {
            assert!(
                is_cargo_target_check(name),
                "{name} should be a cargo check"
            );
        }
        for name in [
            "Semgrep scan",
            "Ruff",
            "Mypy",
            "Pytest",
            "TypeScript",
            "ESLint",
        ] {
            assert!(
                !is_cargo_target_check(name),
                "{name} should NOT be a cargo check"
            );
        }
    }

    #[tokio::test]
    async fn test_cargo_semaphore_serializes() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        // N tasks share Semaphore(1); each bumps a counter on acquire and
        // asserts the in-flight count never exceeds 1.
        let sem = Arc::new(Semaphore::new(1));
        let in_flight = Arc::new(AtomicUsize::new(0));
        let max_seen = Arc::new(AtomicUsize::new(0));

        let mut handles = Vec::new();
        for _ in 0..8 {
            let sem = Arc::clone(&sem);
            let in_flight = Arc::clone(&in_flight);
            let max_seen = Arc::clone(&max_seen);
            handles.push(tokio::spawn(async move {
                let _permit = sem.acquire().await.expect("semaphore never closed");
                let now = in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                max_seen.fetch_max(now, Ordering::SeqCst);
                // Hold the permit briefly so overlap would be observable.
                tokio::time::sleep(Duration::from_millis(5)).await;
                in_flight.fetch_sub(1, Ordering::SeqCst);
            }));
        }
        for h in handles {
            h.await.expect("task join");
        }

        assert_eq!(
            max_seen.load(Ordering::SeqCst),
            1,
            "Semaphore(1) must never allow more than one cargo task at a time"
        );
    }

    // ── PV-01: missing tool => Skipped, not Failed ──────────────────

    #[test]
    fn test_tool_unavailable_signature_matches_runner_enoent() {
        // prview-generated runner-error string for a missing binary.
        let err = "Failed to run mypy: No such file or directory (os error 2)";
        assert!(tool_unavailable_signature(err));
    }

    #[test]
    fn test_tool_unavailable_signature_ignores_real_type_error() {
        let err = "src/x.py:3: error: Incompatible return value type";
        assert!(!tool_unavailable_signature(err));
    }

    #[test]
    fn test_tool_spawn_failure_in_output_matches_uv_marker() {
        let out =
            "error: Failed to spawn: `mypy`\n  Caused by: No such file or directory (os error 2)";
        assert!(tool_spawn_failure_in_output(out));
    }

    #[test]
    fn test_tool_spawn_failure_in_output_ignores_bare_enoent_in_diagnostics() {
        // P1 guard: a tool's own diagnostic mentioning "no such file or
        // directory" must NOT be read as a spawn failure (would be an invisible
        // pass). Only the unambiguous launcher marker counts.
        let out = "src/a.py:10: error: Cannot read file: No such file or directory";
        assert!(!tool_spawn_failure_in_output(out));
    }

    #[test]
    fn test_skipped_result_is_not_failure() {
        let result = CheckResult {
            name: "Mypy".to_string(),
            status: CheckStatus::Skipped,
            duration: Duration::from_secs(0),
            output: String::new(),
            cached: false,
            provenance: None,
        };
        assert!(!result.is_failure(), "Skipped must not count as a failure");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn run_js_command_prefers_local_node_modules_bin() {
        // PR #12 review #15/#17: a tool present in node_modules/.bin must be
        // executed DIRECTLY, never through npx (which can prompt / hit the
        // registry). Prove both the resolver and that run_js_command runs the
        // local bin (its output could only come from the local script).
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().expect("tempdir");
        let bindir = tmp.path().join("node_modules/.bin");
        std::fs::create_dir_all(&bindir).unwrap();
        let toolpath = bindir.join("faketool");
        std::fs::write(&toolpath, "#!/bin/sh\necho LOCAL_BIN_RAN\n").unwrap();
        let mut perms = std::fs::metadata(&toolpath).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&toolpath, perms).unwrap();

        assert!(
            local_js_bin("faketool", tmp.path()).is_some(),
            "installed tool must resolve to a local bin"
        );
        assert!(
            local_js_bin("absent", tmp.path()).is_none(),
            "absent tool must not resolve (caller falls back to npx)"
        );

        let output = run_js_command_with_timeout("faketool", &[], tmp.path(), 10)
            .await
            .expect("local bin should run");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            stdout.contains("LOCAL_BIN_RAN"),
            "run_js_command must exec the local bin directly, got: {stdout}"
        );
    }

    #[tokio::test]
    async fn runtime_skipped_result_is_not_cached() {
        // PR #12 review #14: a check that RAN but returned Skipped (mypy when uv
        // "failed to spawn" a missing binary) must NOT be persisted, or the
        // transient miss is pinned under the source-hash key for the whole hash
        // lifetime and a later run with the tool present still reports Skipped.
        use async_trait::async_trait;

        struct MockCheck {
            status: CheckStatus,
        }

        #[async_trait]
        impl Check for MockCheck {
            fn name(&self) -> &str {
                "Mock"
            }
            fn check_eligibility(&self, _config: &Config) -> CheckEligibility {
                CheckEligibility::Run
            }
            async fn run(&self, _config: &Config) -> Result<CheckResult> {
                Ok(CheckResult {
                    name: "Mock".to_string(),
                    status: self.status,
                    duration: Duration::from_secs(0),
                    output: "ok".to_string(),
                    cached: false,
                    provenance: None,
                })
            }
            fn cache_key(&self, _config: &Config) -> Option<String> {
                Some("mock-key".to_string())
            }
        }

        let tmp = tempfile::tempdir().expect("tempdir");
        let config = rust_config(true, true, true);

        // A runtime Skipped result must not land in the cache.
        let cache = Cache::with_dir(tmp.path().to_path_buf(), true);
        let _ = execute_live_check(
            Box::new(MockCheck {
                status: CheckStatus::Skipped,
            }),
            &config,
            &cache,
        )
        .await;
        assert!(
            cache.get("Mock", "mock-key").is_none(),
            "a runtime Skipped result must not be cached"
        );

        // Control: a Passed result IS cached, proving caching still works.
        let cache2 = Cache::with_dir(tmp.path().to_path_buf(), true);
        let _ = execute_live_check(
            Box::new(MockCheck {
                status: CheckStatus::Passed,
            }),
            &config,
            &cache2,
        )
        .await;
        assert!(
            cache2.get("Mock", "mock-key").is_some(),
            "a Passed result must still be cached"
        );
    }
}
