//! Heuristics module - structural analysis
//!
//! Integrates with:
//! - Loctree: dead code, circular imports, complexity
//! - Madge: JS circular dependencies
//! - Knip: JS dead code/exports
//! - Dependency-cruiser: JS dependency analysis

mod loctree;

pub use loctree::{CycleInfo, DeadExport, DeadParrot, LoctreeAnalysis, TwinsAnalysis, run_loctree};

use crate::Config;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::time::Duration;
use tokio::process::Command as TokioCommand;

/// Combined heuristics results
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HeuristicsResult {
    pub loctree: Option<LoctreeAnalysis>,
    pub madge: Option<MadgeResult>,
    pub knip: Option<KnipResult>,
    pub depcruiser: Option<DepcruiserResult>,
    pub summary: HeuristicsSummary,
    /// Path used for analysis (snapshot or repo root). None = local cwd.
    pub analysis_root: Option<String>,
    /// Regression delta (base vs target heuristics). None if no base available.
    pub regression: Option<HeuristicsRegression>,
}

/// Delta between base and target heuristics runs
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HeuristicsRegression {
    pub base_sha: String,
    pub target_sha: String,
    pub dead_exports_delta: i64,
    pub cycles_delta: i64,
    #[serde(rename = "unused_symbols_delta", alias = "dead_parrots_delta")]
    pub dead_parrots_delta: i64,
    pub base_dead_exports: usize,
    pub target_dead_exports: usize,
    pub base_circular_imports: usize,
    pub target_circular_imports: usize,
    #[serde(rename = "base_unused_symbols", alias = "base_dead_parrots")]
    pub base_dead_parrots: usize,
    #[serde(rename = "target_unused_symbols", alias = "target_dead_parrots")]
    pub target_dead_parrots: usize,
    pub regression_detected: bool,
    pub improvement_detected: bool,
}

impl HeuristicsRegression {
    pub fn unused_symbols_delta(&self) -> i64 {
        self.dead_parrots_delta
    }

    pub fn base_unused_symbols(&self) -> usize {
        self.base_dead_parrots
    }

    pub fn target_unused_symbols(&self) -> usize {
        self.target_dead_parrots
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct HeuristicsSummary {
    pub total_files: usize,
    pub total_loc: usize,
    pub dead_exports: usize,
    pub circular_imports: usize,
    pub dead_parrots: usize,
    pub exact_twins: usize,
}

/// Madge circular dependency analysis
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MadgeResult {
    pub circular_count: usize,
    pub circular_deps: Vec<Vec<String>>,
    pub output: String,
}

/// Knip dead code analysis
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct KnipResult {
    pub unused_files: usize,
    pub unused_exports: usize,
    pub unused_deps: usize,
    pub output: String,
}

/// Dependency-cruiser analysis
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DepcruiserResult {
    pub violations: usize,
    pub output: String,
}

/// Run all heuristics.
///
/// `analysis_root` overrides `config.repo_root` as the directory to scan.
/// Used in snapshot mode to point heuristics at an extracted git tree.
pub async fn run_all(config: &Config, analysis_root: Option<&Path>) -> Result<HeuristicsResult> {
    use colored::Colorize;

    if !config.run_heuristics {
        return Ok(HeuristicsResult::default());
    }
    let root = analysis_root.unwrap_or(&config.repo_root);
    let emit_human_stdout = !config.json && !config.quiet;

    if emit_human_stdout {
        println!("{}", "Running heuristics...".cyan());
        if analysis_root.is_some() {
            println!("  {} Analysis root: {}", "ℹ".blue(), root.display());
        }
        println!();
    }

    let mut result = HeuristicsResult::default();

    // Run loctree
    match run_loctree(root).await {
        Ok(analysis) => {
            result.summary.total_files = analysis.stats.total_files;
            result.summary.total_loc = analysis.stats.total_loc;
            result.summary.dead_exports = analysis.dead_exports.len();
            result.summary.circular_imports = analysis.cycles.len();
            result.summary.dead_parrots = analysis.twins.dead_parrots.len();
            result.summary.exact_twins = analysis.twins.exact_twins.len();

            let status = if analysis.dead_exports.is_empty() && analysis.cycles.is_empty() {
                "✓".green()
            } else {
                "⚠".yellow()
            };

            if emit_human_stdout {
                println!(
                    "  {} Loctree: {} files, {} LOC, {} dead exports, {} unused symbols, {} cycles",
                    status,
                    analysis.stats.total_files,
                    analysis.stats.total_loc,
                    analysis.dead_exports.len(),
                    analysis.twins.dead_parrots.len(),
                    analysis.cycles.len()
                );
            }

            result.loctree = Some(analysis);
        }
        Err(e) => {
            // Honest degraded status: loctree produced no signal this run, so
            // say so instead of leaving `result.loctree = None` behind a green
            // line. Downstream (compute_delta_checked, findings) treats the
            // absent signal as "skipped", never as a healthy zero.
            if emit_human_stdout {
                println!("  {} Loctree: not available ({})", "○".dimmed(), e);
            }
        }
    }

    // Run JS heuristics only if there are actual JS/TS source files
    if config.profile.has_js_source {
        // Madge
        match run_madge(root).await {
            Ok(madge) => {
                let status = if madge.circular_count == 0 {
                    "✓".green()
                } else {
                    "⚠".yellow()
                };
                if emit_human_stdout {
                    println!(
                        "  {} Madge: {} circular dependencies",
                        status, madge.circular_count
                    );
                }
                result.madge = Some(madge);
            }
            Err(e) => {
                if emit_human_stdout {
                    println!("  {} Madge: not available ({})", "○".dimmed(), e);
                }
            }
        }

        // Knip
        match run_knip(root).await {
            Ok(knip) => {
                let total_issues = knip.unused_files + knip.unused_exports + knip.unused_deps;
                let status = if total_issues == 0 {
                    "✓".green()
                } else {
                    "⚠".yellow()
                };
                if emit_human_stdout {
                    println!(
                        "  {} Knip: {} unused files, {} unused exports, {} unused deps",
                        status, knip.unused_files, knip.unused_exports, knip.unused_deps
                    );
                }
                result.knip = Some(knip);
            }
            Err(e) => {
                if emit_human_stdout {
                    println!("  {} Knip: not available ({})", "○".dimmed(), e);
                }
            }
        }

        // Dependency-cruiser
        match run_depcruiser(root).await {
            Ok(depcruiser) => {
                let status = if depcruiser.violations == 0 {
                    "✓".green()
                } else {
                    "⚠".yellow()
                };
                if emit_human_stdout {
                    println!(
                        "  {} Dependency-cruiser: {} violations",
                        status, depcruiser.violations
                    );
                }
                result.depcruiser = Some(depcruiser);
            }
            Err(e) => {
                if emit_human_stdout {
                    println!(
                        "  {} Dependency-cruiser: not available ({})",
                        "○".dimmed(),
                        e
                    );
                }
            }
        }
    }

    if emit_human_stdout {
        println!();
    }

    Ok(result)
}

/// Hard ceiling for a single JS heuristic tool run. Heuristics are advisory
/// signals, not gates — a wedged npx must degrade to "not available", never
/// hang the whole review.
const NPX_TOOL_TIMEOUT_SECS: u64 = 300;

/// Run a command with the safety rails every child of prview needs:
/// - stdin detached (a tool must fail or skip, never sit on an interactive
///   prompt inherited from the operator's terminal),
/// - kill-on-drop + timeout (a stuck child is killed and reported honestly).
async fn run_command_with_timeout(
    cmd: TokioCommand,
    label: &str,
    timeout_secs: u64,
) -> Result<std::process::Output> {
    // Shared rails (stdin-null, kill_on_drop, own process group) + concurrent
    // output drain + group-SIGKILL on timeout live in crate::proc.
    crate::proc::run_capture_with_timeout(cmd, Duration::from_secs(timeout_secs), label, || {
        anyhow::anyhow!("{label} timed out after {timeout_secs}s (process killed)")
    })
    .await
}

/// Run a JS heuristic tool, preferring a resolved local binary.
///
/// A `node_modules/.bin/<tool>` is executed directly — no launcher, no npm
/// registry consult, no interactive prompt. Only when the tool is not installed
/// locally do we fall back to `npx --no-install <tool>`, whose `--no-install`
/// still turns a missing tool into a fast, parseable failure rather than npm's
/// "Ok to proceed?" prompt (root cause of the --deep hang) (PR #12 review
/// #15/#17).
async fn run_npx_tool(root: &Path, tool: &str, args: &[&str]) -> Result<std::process::Output> {
    if let Some(bin) = crate::checks::local_js_bin(tool, root) {
        let mut cmd = TokioCommand::new(bin);
        cmd.args(args).current_dir(root);
        return run_command_with_timeout(cmd, tool, NPX_TOOL_TIMEOUT_SECS).await;
    }
    let mut cmd = TokioCommand::new("npx");
    cmd.arg("--no-install")
        .arg(tool)
        .args(args)
        .current_dir(root);
    run_command_with_timeout(cmd, tool, NPX_TOOL_TIMEOUT_SECS).await
}

/// Run madge circular dependency analysis
async fn run_madge(root: &Path) -> Result<MadgeResult> {
    let output = run_npx_tool(root, "madge", &["--circular", "--json", "src"]).await?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    if !output.status.success() && stdout.trim().is_empty() {
        anyhow::bail!(format_tool_failure("madge", &stdout, &stderr));
    }

    parse_madge_output(&stdout, &stderr)
}

/// Run knip dead code analysis (uses JSON reporter for reliable parsing)
async fn run_knip(root: &Path) -> Result<KnipResult> {
    let output = run_npx_tool(root, "knip", &["--reporter", "json"]).await?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    parse_knip_output(&stdout, &stderr)
}

fn parse_knip_output(stdout: &str, stderr: &str) -> Result<KnipResult> {
    // Try to parse JSON output
    #[derive(serde::Deserialize, Default)]
    #[serde(default)]
    struct KnipJson {
        files: Vec<serde_json::Value>,
        issues: Vec<serde_json::Value>,
        #[serde(rename = "unlisted")]
        unlisted_deps: Vec<serde_json::Value>,
        #[serde(rename = "unresolved")]
        unresolved: Vec<serde_json::Value>,
    }

    let parsed: KnipJson = serde_json::from_str(stdout).with_context(|| {
        let stdout = stdout.trim();
        let stderr = stderr.trim();

        match (stdout.is_empty(), stderr.is_empty()) {
            (false, false) => format!(
                "knip did not produce valid JSON output\nstdout: {}\nstderr: {}",
                stdout, stderr
            ),
            (false, true) => format!("knip did not produce valid JSON output\nstdout: {}", stdout),
            (true, false) => format!("knip did not produce valid JSON output\nstderr: {}", stderr),
            (true, true) => "knip did not produce any output".to_string(),
        }
    })?;

    let unused_files = parsed.files.len();
    let unused_exports = parsed.issues.len();
    let unused_deps = parsed.unlisted_deps.len() + parsed.unresolved.len();

    Ok(KnipResult {
        unused_files,
        unused_exports,
        unused_deps,
        output: format!("{}\n{}", stdout, stderr),
    })
}

/// Run dependency-cruiser analysis
async fn run_depcruiser(root: &Path) -> Result<DepcruiserResult> {
    let output = run_npx_tool(root, "depcruise", &["src", "--output-type", "err"]).await?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    parse_depcruiser_output(output.status.success(), &stdout, &stderr)
}

fn parse_madge_output(stdout: &str, stderr: &str) -> Result<MadgeResult> {
    let trimmed_stdout = stdout.trim();
    if trimmed_stdout.is_empty() {
        anyhow::bail!(format_tool_failure("madge", stdout, stderr));
    }

    let circular_deps: Vec<Vec<String>> = serde_json::from_str(trimmed_stdout)
        .with_context(|| format_tool_failure("madge", stdout, stderr))?;
    let circular_count = circular_deps.len();

    Ok(MadgeResult {
        circular_count,
        circular_deps,
        output: stdout.to_string(),
    })
}

fn parse_depcruiser_output(
    command_succeeded: bool,
    stdout: &str,
    stderr: &str,
) -> Result<DepcruiserResult> {
    let combined = combine_tool_output(stdout, stderr);
    let violations = depcruiser_violation_count(&combined);

    if violations > 0 || command_succeeded {
        return Ok(DepcruiserResult {
            violations,
            output: combined,
        });
    }

    anyhow::bail!(format_tool_failure("depcruise", stdout, stderr));
}

fn depcruiser_violation_count(output: &str) -> usize {
    output
        .lines()
        .filter(|line| is_depcruiser_violation_line(line))
        .count()
}

fn is_depcruiser_violation_line(line: &str) -> bool {
    let trimmed = line.trim();
    let Some((severity, rest)) = trimmed.split_once(' ') else {
        return false;
    };

    if severity != "warn" && severity != "error" {
        return false;
    }

    let Some((rule_name, details)) = rest.split_once(':') else {
        return false;
    };

    !details.trim().is_empty()
        && !rule_name.is_empty()
        && rule_name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

fn combine_tool_output(stdout: &str, stderr: &str) -> String {
    match (stdout.trim().is_empty(), stderr.trim().is_empty()) {
        (false, false) => format!("{}\n{}", stdout, stderr),
        (false, true) => stdout.to_string(),
        (true, false) => stderr.to_string(),
        (true, true) => String::new(),
    }
}

fn format_tool_failure(tool: &str, stdout: &str, stderr: &str) -> String {
    let combined = combine_tool_output(stdout, stderr);
    let detail = combined
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("no output");

    format!("{tool} failed: {detail}")
}

/// Compute delta between base and target heuristics snapshots.
pub fn compute_delta(
    base: &HeuristicsResult,
    target: &HeuristicsResult,
    base_sha: &str,
    target_sha: &str,
) -> HeuristicsRegression {
    let dead_delta = target.summary.dead_exports as i64 - base.summary.dead_exports as i64;
    let cycles_delta =
        target.summary.circular_imports as i64 - base.summary.circular_imports as i64;
    let parrots_delta = target.summary.dead_parrots as i64 - base.summary.dead_parrots as i64;

    HeuristicsRegression {
        base_sha: base_sha.to_string(),
        target_sha: target_sha.to_string(),
        dead_exports_delta: dead_delta,
        cycles_delta,
        dead_parrots_delta: parrots_delta,
        base_dead_exports: base.summary.dead_exports,
        target_dead_exports: target.summary.dead_exports,
        base_circular_imports: base.summary.circular_imports,
        target_circular_imports: target.summary.circular_imports,
        base_dead_parrots: base.summary.dead_parrots,
        target_dead_parrots: target.summary.dead_parrots,
        regression_detected: dead_delta > 0 || cycles_delta > 0 || parrots_delta > 0,
        improvement_detected: dead_delta < 0 || cycles_delta < 0 || parrots_delta < 0,
    }
}

/// True only when loctree actually produced a signal for this result.
///
/// A failed loctree run leaves `loctree = None` (see `run_all`), so its zeroed
/// summary is meaningless. Availability is the honest gate before any delta.
fn loctree_available(result: &HeuristicsResult) -> bool {
    result
        .loctree
        .as_ref()
        .map(|l| l.available)
        .unwrap_or(false)
}

/// Compute a regression delta only when BOTH sides carry a real loctree signal.
///
/// This is the fail-open guard for the regression surface: if either side is
/// blind (loctree failed → not available), its zeroed dead-export/cycle counts
/// would manufacture a false regression (blind base → target's N reads as +N)
/// or a false improvement (blind target → base's N reads as -N). Returning
/// `None` makes the caller report "no signal" instead of a fabricated delta.
pub fn compute_delta_checked(
    base: &HeuristicsResult,
    target: &HeuristicsResult,
    base_sha: &str,
    target_sha: &str,
) -> Option<HeuristicsRegression> {
    if !loctree_available(base) || !loctree_available(target) {
        return None;
    }
    Some(compute_delta(base, target, base_sha, target_sha))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn run_command_with_timeout_kills_stuck_child() {
        // A child that would outlive any sane heuristic must be killed and
        // reported as a timeout, not awaited forever (the --deep hang class).
        let mut cmd = TokioCommand::new("sleep");
        cmd.arg("30");
        let err = run_command_with_timeout(cmd, "sleepy-tool", 1)
            .await
            .expect_err("stuck child must time out");
        assert!(err.to_string().contains("sleepy-tool timed out after 1s"));
    }

    // The grandchild process-group kill is proven canonically in
    // crate::proc::tests; the tests here guard the heuristics integration
    // (timeout message + stdin detach) through run_command_with_timeout.

    #[tokio::test]
    async fn run_command_with_timeout_detaches_stdin() {
        // `cat` with inherited terminal stdin would block forever; with
        // stdin detached it sees EOF and exits immediately. This guards the
        // exact npm "Ok to proceed?" prompt scenario.
        let cmd = TokioCommand::new("cat");
        let output = run_command_with_timeout(cmd, "cat", 5)
            .await
            .expect("cat with null stdin exits at once");
        assert!(output.status.success());
    }

    #[test]
    fn test_heuristics_result_default() {
        let result = HeuristicsResult::default();
        assert!(result.loctree.is_none());
        assert_eq!(result.summary.total_files, 0);
        assert_eq!(result.summary.total_loc, 0);
        assert!(result.analysis_root.is_none());
        assert!(result.regression.is_none());
    }

    #[test]
    fn test_heuristics_summary_default() {
        let summary = HeuristicsSummary::default();
        assert_eq!(summary.total_files, 0);
        assert_eq!(summary.total_loc, 0);
        assert_eq!(summary.dead_exports, 0);
        assert_eq!(summary.circular_imports, 0);
    }

    #[test]
    fn test_heuristics_summary_creation() {
        let summary = HeuristicsSummary {
            total_files: 100,
            total_loc: 5000,
            dead_exports: 5,
            circular_imports: 2,
            dead_parrots: 0,
            exact_twins: 0,
        };
        assert_eq!(summary.total_files, 100);
        assert_eq!(summary.total_loc, 5000);
        assert_eq!(summary.dead_exports, 5);
        assert_eq!(summary.circular_imports, 2);
    }

    #[test]
    fn test_heuristics_result_with_summary() {
        let result = HeuristicsResult {
            summary: HeuristicsSummary {
                total_files: 50,
                total_loc: 2500,
                dead_exports: 3,
                circular_imports: 1,
                dead_parrots: 0,
                exact_twins: 0,
            },
            ..Default::default()
        };
        assert!(result.loctree.is_none());
        assert_eq!(result.summary.total_files, 50);
    }

    #[test]
    fn test_heuristics_result_clone() {
        let original = HeuristicsResult {
            summary: HeuristicsSummary {
                total_files: 25,
                total_loc: 1000,
                ..Default::default()
            },
            ..Default::default()
        };
        let cloned = original.clone();
        assert_eq!(original.summary.total_files, cloned.summary.total_files);
        assert_eq!(original.summary.total_loc, cloned.summary.total_loc);
    }

    #[test]
    fn test_parse_knip_output_counts_items() {
        let result = parse_knip_output(
            r#"{
                "files": [{"path":"src/a.ts"}],
                "issues": [{"symbol":"foo"}, {"symbol":"bar"}],
                "unlisted": [{"name":"left-pad"}],
                "unresolved": [{"name":"missing"}]
            }"#,
            "",
        )
        .expect("valid knip json");

        assert_eq!(result.unused_files, 1);
        assert_eq!(result.unused_exports, 2);
        assert_eq!(result.unused_deps, 2);
    }

    #[test]
    fn test_parse_knip_output_rejects_invalid_json() {
        let err = parse_knip_output("npm ERR! knip not found", "command failed")
            .expect_err("invalid output should fail");

        assert!(
            err.to_string()
                .contains("knip did not produce valid JSON output")
        );
    }

    #[test]
    fn test_parse_madge_output_counts_cycles() {
        let result =
            parse_madge_output(r#"[["src/a.ts","src/b.ts"]]"#, "").expect("valid madge json");

        assert_eq!(result.circular_count, 1);
        assert_eq!(result.circular_deps.len(), 1);
    }

    #[test]
    fn test_parse_madge_output_rejects_invalid_json() {
        let err = parse_madge_output("Error: failed to parse file", "SyntaxError")
            .expect_err("invalid madge output should fail");

        assert!(err.to_string().contains("madge failed"));
    }

    #[test]
    fn test_parse_depcruiser_output_counts_rule_violations() {
        let result = parse_depcruiser_output(
            false,
            "warn no-circular: src/a.ts -> src/b.ts\nerror not-to-unresolvable: src/a.ts -> missing\n",
            "",
        )
        .expect("depcruise violations should parse");

        assert_eq!(result.violations, 2);
    }

    #[test]
    fn test_parse_depcruiser_output_rejects_command_failure() {
        let err = parse_depcruiser_output(false, "", "Error: Cannot find module depcruise")
            .expect_err("command failure should not look clean");

        assert!(err.to_string().contains("depcruise failed"));
    }

    #[test]
    fn test_depcruiser_violation_line_rejects_runtime_errors() {
        assert!(!is_depcruiser_violation_line(
            "error Error: Cannot find module dependency-cruiser"
        ));
        assert!(is_depcruiser_violation_line(
            "error no-circular: src/a.ts -> src/b.ts"
        ));
    }

    #[test]
    fn test_heuristics_summary_serialization() {
        let summary = HeuristicsSummary {
            total_files: 10,
            total_loc: 500,
            dead_exports: 2,
            circular_imports: 1,
            dead_parrots: 0,
            exact_twins: 0,
        };
        let json = serde_json::to_string(&summary).unwrap();
        assert!(json.contains("\"total_files\":10"));
        assert!(json.contains("\"total_loc\":500"));
    }

    #[test]
    fn test_heuristics_summary_deserialization() {
        let json = r#"{"total_files":20,"total_loc":1000,"dead_exports":1,"circular_imports":0}"#;
        let summary: HeuristicsSummary = serde_json::from_str(json).unwrap();
        assert_eq!(summary.total_files, 20);
        assert_eq!(summary.total_loc, 1000);
        assert_eq!(summary.dead_exports, 1);
    }

    fn available_result(dead: usize, cycles: usize) -> HeuristicsResult {
        HeuristicsResult {
            summary: HeuristicsSummary {
                dead_exports: dead,
                circular_imports: cycles,
                ..Default::default()
            },
            loctree: Some(LoctreeAnalysis {
                available: true,
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn compute_delta_checked_skips_when_a_side_is_blind() {
        // A failed loctree run leaves loctree=None (not available). Its zeroed
        // summary must NOT become a real delta: a target with 5 dead exports
        // against a blind base would otherwise manufacture a +5 "regression".
        let blind_base = HeuristicsResult {
            loctree: None,
            ..Default::default()
        };
        let target = available_result(5, 0);
        assert!(
            compute_delta_checked(&blind_base, &target, "a", "b").is_none(),
            "blind base must not produce a false regression"
        );

        // Symmetric: a blind target must not read as a false improvement.
        let base = available_result(5, 0);
        let blind_target = HeuristicsResult {
            loctree: None,
            ..Default::default()
        };
        assert!(
            compute_delta_checked(&base, &blind_target, "a", "b").is_none(),
            "blind target must not produce a false improvement"
        );
    }

    #[test]
    fn compute_delta_checked_computes_when_both_available() {
        let base = available_result(3, 1);
        let target = available_result(5, 1);
        let reg = compute_delta_checked(&base, &target, "a", "b")
            .expect("both sides carry a real loctree signal");
        assert_eq!(reg.dead_exports_delta, 2);
        assert!(reg.regression_detected);
    }

    #[test]
    fn test_compute_delta_no_change() {
        let base = HeuristicsResult {
            summary: HeuristicsSummary {
                dead_exports: 5,
                circular_imports: 2,
                dead_parrots: 3,
                ..Default::default()
            },
            ..Default::default()
        };
        let target = base.clone();
        let reg = compute_delta(&base, &target, "aaa", "bbb");
        assert_eq!(reg.dead_exports_delta, 0);
        assert_eq!(reg.cycles_delta, 0);
        assert!(!reg.regression_detected);
        assert!(!reg.improvement_detected);
    }

    #[test]
    fn test_compute_delta_detected() {
        let base = HeuristicsResult {
            summary: HeuristicsSummary {
                dead_exports: 3,
                circular_imports: 1,
                dead_parrots: 0,
                ..Default::default()
            },
            ..Default::default()
        };
        let target = HeuristicsResult {
            summary: HeuristicsSummary {
                dead_exports: 5,
                circular_imports: 4,
                dead_parrots: 0,
                ..Default::default()
            },
            ..Default::default()
        };
        let reg = compute_delta(&base, &target, "aaa", "bbb");
        assert_eq!(reg.dead_exports_delta, 2);
        assert_eq!(reg.cycles_delta, 3);
        assert!(reg.regression_detected);
        assert!(!reg.improvement_detected);
    }

    #[test]
    fn test_compute_delta_improvement() {
        let base = HeuristicsResult {
            summary: HeuristicsSummary {
                dead_exports: 10,
                circular_imports: 5,
                dead_parrots: 8,
                ..Default::default()
            },
            ..Default::default()
        };
        let target = HeuristicsResult {
            summary: HeuristicsSummary {
                dead_exports: 3,
                circular_imports: 2,
                dead_parrots: 4,
                ..Default::default()
            },
            ..Default::default()
        };
        let reg = compute_delta(&base, &target, "aaa", "bbb");
        assert_eq!(reg.dead_exports_delta, -7);
        assert_eq!(reg.cycles_delta, -3);
        assert_eq!(reg.dead_parrots_delta, -4);
        assert!(!reg.regression_detected);
        assert!(reg.improvement_detected);
    }

    #[test]
    fn test_compute_delta_mixed_signals() {
        // dead_exports goes up (regression), cycles goes down (improvement)
        // Both flags should be true simultaneously
        let base = HeuristicsResult {
            summary: HeuristicsSummary {
                dead_exports: 2,
                circular_imports: 6,
                dead_parrots: 0,
                ..Default::default()
            },
            ..Default::default()
        };
        let target = HeuristicsResult {
            summary: HeuristicsSummary {
                dead_exports: 5,
                circular_imports: 2,
                dead_parrots: 0,
                ..Default::default()
            },
            ..Default::default()
        };
        let reg = compute_delta(&base, &target, "base111", "target222");

        assert_eq!(reg.dead_exports_delta, 3, "dead_exports went up by 3");
        assert_eq!(reg.cycles_delta, -4, "cycles went down by 4");
        assert_eq!(reg.dead_parrots_delta, 0);
        assert!(
            reg.regression_detected,
            "dead_exports increase should trigger regression"
        );
        assert!(
            reg.improvement_detected,
            "cycles decrease should trigger improvement"
        );
        assert_eq!(reg.base_sha, "base111");
        assert_eq!(reg.target_sha, "target222");
        assert_eq!(reg.base_dead_exports, 2);
        assert_eq!(reg.target_dead_exports, 5);
        assert_eq!(reg.base_circular_imports, 6);
        assert_eq!(reg.target_circular_imports, 2);
    }

    #[test]
    fn test_regression_serialization() {
        let reg = HeuristicsRegression {
            base_sha: "abc".to_string(),
            target_sha: "def".to_string(),
            dead_exports_delta: 2,
            cycles_delta: -1,
            dead_parrots_delta: 0,
            base_dead_exports: 3,
            target_dead_exports: 5,
            base_circular_imports: 4,
            target_circular_imports: 3,
            base_dead_parrots: 2,
            target_dead_parrots: 2,
            regression_detected: true,
            improvement_detected: true,
        };
        let json = serde_json::to_string(&reg).unwrap();
        assert!(json.contains("\"dead_exports_delta\":2"));
        assert!(json.contains("\"regression_detected\":true"));
    }

    #[test]
    fn test_regression_unused_symbol_accessors_follow_serialized_names() {
        let reg = HeuristicsRegression {
            dead_parrots_delta: -3,
            base_dead_parrots: 9,
            target_dead_parrots: 6,
            ..Default::default()
        };

        assert_eq!(reg.unused_symbols_delta(), -3);
        assert_eq!(reg.base_unused_symbols(), 9);
        assert_eq!(reg.target_unused_symbols(), 6);
    }
}
