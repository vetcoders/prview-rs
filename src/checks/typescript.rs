//! TypeScript and JavaScript checks (tsc, eslint, vitest)

use super::{
    Check, CheckProvenance, CheckResult, CheckStatus, TEST_TIMEOUT_SECS, find_hard_fail_signatures,
    js_tool_available, plan_check_run, run_js_command, run_js_command_with_timeout,
};
use crate::Config;
use anyhow::Result;
use async_trait::async_trait;
use chrono::Local;
use std::path::{Path, PathBuf};

pub struct TypeScriptCheck;
pub struct ESLintCheck;
pub struct VitestCheck;
pub struct StylelintCheck;

const GENERATED_IGNORE_PATTERNS: &[&str] = &[
    "**/target/**",
    "**/coverage/**",
    "**/tmp/**",
    "**/dist/**",
    "**/.next/**",
    "**/node_modules/**",
];

fn eslint_args(config: &Config) -> Vec<String> {
    let mut args = vec![
        ".".to_string(),
        "--ext".to_string(),
        ".ts,.tsx,.js,.jsx".to_string(),
        "--max-warnings".to_string(),
        "0".to_string(),
    ];
    for pattern in GENERATED_IGNORE_PATTERNS {
        args.push("--ignore-pattern".to_string());
        args.push(pattern.to_string());
    }
    for pattern in &config.lint_ignore_patterns {
        args.push("--ignore-pattern".to_string());
        args.push(pattern.clone());
    }
    args
}

fn stylelint_args(config: &Config) -> Vec<String> {
    let mut args = vec![
        "**/*.css".to_string(),
        "**/*.scss".to_string(),
        "--max-warnings".to_string(),
        "0".to_string(),
    ];
    for pattern in GENERATED_IGNORE_PATTERNS {
        args.push("--ignore-pattern".to_string());
        args.push(pattern.to_string());
    }
    for pattern in &config.lint_ignore_patterns {
        args.push("--ignore-pattern".to_string());
        args.push(pattern.clone());
    }
    args
}

fn vitest_args(config: &Config) -> Vec<String> {
    // Vitest's CLI flag overrides the project's configured maxWorkers. Using
    // the wider balanced-plan limit here could therefore *raise* a project's
    // intentional single-worker ceiling. Keep the owned Vitest pool at one
    // worker in every plan until Vitest exposes a portable "min(config, cap)"
    // mechanism; the run-wide limit remains an upper bound, never a request to
    // increase project concurrency.
    let mut args = vec![
        "run".to_string(),
        "--maxWorkers".to_string(),
        "1".to_string(),
    ];
    if let Some(pattern) = &config.tests_pattern {
        args.extend(["--testNamePattern".to_string(), pattern.clone()]);
    }
    args
}

/// The change-scoped Vitest command line.
///
/// `related` selects by the STATIC import graph, so the arguments are source
/// files, not test files — Vitest resolves the latter itself, which is also why
/// `selected` here counts inputs rather than specs.
///
/// `--passWithNoTests` is required rather than optional: an empty related set is
/// not a tool error, and without the flag Vitest exits non-zero and the check
/// would report a failure for a change that simply has no related tests. The
/// emptiness is then read from the JSON reporter (see [`vitest_reporter_args`])
/// and reported as `Skipped` (contract §8.1) — never `Passed`, which would claim
/// evidence from zero executed tests.
///
/// Everything the full command carries is preserved: the one-worker ceiling and
/// `--testNamePattern`, which keeps filtering INSIDE the selection (contract
/// §4.3) rather than widening it.
fn vitest_scoped_args(config: &Config, selector_inputs: &[String]) -> Vec<String> {
    let mut args = vec![
        "related".to_string(),
        "--run".to_string(),
        "--maxWorkers".to_string(),
        "1".to_string(),
        "--passWithNoTests".to_string(),
    ];
    if let Some(pattern) = &config.tests_pattern {
        args.extend(["--testNamePattern".to_string(), pattern.clone()]);
    }
    args.extend(selector_inputs.iter().cloned());
    args
}

/// The machine-readable proof of how much a narrowed run actually executed.
///
/// Appended AFTER the selector arguments so the selector stays a contiguous
/// verbatim fragment of the command line; Vitest accepts options after the
/// positional filters (verified on 3.2.4).
///
/// `--reporter=default` is repeated explicitly because naming a second reporter
/// replaces the default one: without it the run would lose its human-readable
/// log, which is the check's `output`. `--outputFile.json=` is the per-reporter
/// form — plain `--outputFile` would apply to whichever reporter claims it.
fn vitest_reporter_args(report_path: &Path) -> Vec<String> {
    vec![
        "--reporter=default".to_string(),
        "--reporter=json".to_string(),
        format!("--outputFile.json={}", report_path.display()),
    ]
}

/// Where a narrowed run writes its JSON report, alive until the run has been
/// classified.
///
/// A temporary directory rather than the artifact pack: a check is handed a
/// `Config`, and `artifacts_dir()` there is only a PREDICTION of the pack path
/// (the real one is allocated per run and can differ), while writing into the
/// reviewed tree would mutate the very tree under review. The report is
/// evidence for the verdict, not an artifact of its own — what survives into
/// the pack is the status and the reason it earned.
struct VitestReportFile {
    _dir: tempfile::TempDir,
    path: PathBuf,
}

impl VitestReportFile {
    fn create() -> std::io::Result<Self> {
        let dir = tempfile::Builder::new()
            .prefix("prview-vitest-")
            .tempdir()?;
        let path = dir.path().join("vitest-scope.json");
        Ok(Self { _dir: dir, path })
    }
}

/// What the JSON reporter said about the run that just happened.
struct VitestExecution {
    total_suites: u64,
    total_tests: u64,
    results: usize,
}

/// Read the reporter's own numbers, or say why they cannot be trusted.
///
/// Every failure mode collapses into `Err`, because the check treats them
/// identically: an unverifiable narrowed run is never green and never a skip.
fn read_vitest_execution(path: &Path) -> std::result::Result<VitestExecution, String> {
    let raw = std::fs::read_to_string(path)
        .map_err(|error| format!("{} could not be read: {error}", path.display()))?;
    let report: serde_json::Value = serde_json::from_str(&raw)
        .map_err(|error| format!("{} is not valid JSON: {error}", path.display()))?;
    let number = |key: &str| {
        report
            .get(key)
            .and_then(serde_json::Value::as_u64)
            .ok_or_else(|| format!("{} has no numeric {key}", path.display()))
    };
    let results = report
        .get("testResults")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| format!("{} has no testResults array", path.display()))?
        .len();
    Ok(VitestExecution {
        total_suites: number("numTotalTestSuites")?,
        total_tests: number("numTotalTests")?,
        results,
    })
}

/// A finished Vitest run, as the check reads it.
struct VitestVerdict {
    status: CheckStatus,
    output: String,
    /// Test files the reporter says it collected, for a narrowed run whose
    /// report could be read. `None` for a full run (nothing narrowed, nothing
    /// to count) and for a narrowed run whose report is missing or unreadable —
    /// which is the same thing as not knowing.
    collected: Option<usize>,
}

/// Turn a finished Vitest run into a verdict.
///
/// `report_path` is `Some` exactly for a narrowed run. For that run the exit
/// code alone cannot tell "everything related passed" from "nothing ran"
/// (`--passWithNoTests` makes both exit 0), and the tool's prose cannot be
/// trusted to tell them apart either: stdout carries the reviewed code's own
/// output, so a test that prints "No test files found" would otherwise spoof a
/// skip. The JSON reporter's counters are the only witness that is not under
/// the reviewed tree's control.
///
/// A full run keeps exactly the classification it has always had.
fn classify_vitest_outcome(
    report_path: Option<&Path>,
    success: bool,
    selected_inputs: usize,
    combined: &str,
) -> VitestVerdict {
    let Some(report_path) = report_path else {
        // A full run: the exit code is the whole verdict, exactly as before.
        return VitestVerdict {
            status: if success {
                CheckStatus::Passed
            } else {
                CheckStatus::Failed
            },
            output: combined.to_string(),
            collected: None,
        };
    };
    let execution = read_vitest_execution(report_path);
    let collected = execution.as_ref().ok().map(|execution| execution.results);
    let (status, output) = match execution {
        // Nothing was collected and nothing ran: the narrowing was correct and
        // this change has no related test (contract §8.1).
        Ok(execution) if execution.total_suites == 0 && execution.results == 0 => (
            CheckStatus::Skipped,
            format!(
                "{}\nVitest found no test file importing any of the {selected_inputs} changed \
                 source file(s).\n{combined}",
                crate::checks::scope::NO_TESTS_RELATED_TO_THE_CHANGE
            ),
        ),
        // Tests ran, so the tool's own verdict is the verdict.
        Ok(execution) if execution.total_tests > 0 => {
            if success {
                (CheckStatus::Passed, combined.to_string())
            } else {
                (CheckStatus::Failed, combined.to_string())
            }
        }
        // Test files were collected but no test came out of them. Neither a
        // pass nor "no tests related to the change": something was there and it
        // did not run.
        Ok(execution) if !success => (
            CheckStatus::Failed,
            format!(
                "the narrowed Vitest run collected {} test file(s) and executed no test\n{combined}",
                execution.results
            ),
        ),
        Ok(execution) => (
            CheckStatus::Error,
            format!(
                "could not verify that the narrowed Vitest run executed any test: it collected {} \
                 suite(s) in {} test file(s) and executed none\n{combined}",
                execution.total_suites, execution.results
            ),
        ),
        Err(detail) => (
            CheckStatus::Error,
            format!(
                "could not verify that the narrowed Vitest run executed any test (reporter output \
                 missing): {detail}\n{combined}"
            ),
        ),
    };
    VitestVerdict {
        status,
        output,
        collected,
    }
}

/// The executed scope a narrowed run publishes once its reporter has spoken.
///
/// Contract §7 counts SELECTED UNITS, and for Vitest a unit is a TEST FILE —
/// not a changed source file, which is an input to the selector. The plan can
/// only count inputs, because which test files they pull in is a property of
/// the import graph that Vitest resolves at run time; so the planned count is
/// replaced by the reporter's once the run is over.
///
/// A report that could not be read leaves nothing proven, and therefore nothing
/// counted: `selected: 0` beside a non-null selector, on a row the classifier
/// has already turned into an `Error`. The selector itself is never rewritten —
/// it is what was asked for, and that did not change.
fn published_executed_scope(
    planned: Option<crate::checks::scope::ExecutedScope>,
    collected: Option<usize>,
) -> Option<crate::checks::scope::ExecutedScope> {
    match planned {
        Some(crate::checks::scope::ExecutedScope::ChangeScoped { selector, .. }) => {
            Some(crate::checks::scope::ExecutedScope::ChangeScoped {
                selected: collected.unwrap_or(0),
                selector,
            })
        }
        other => other,
    }
}

/// What Vitest will run, and the evidence of it, given this run's scope
/// decision.
///
/// `Err` is not used for tool failure — it is the one shape that says "no
/// command at all", which is what an empty selection means.
enum VitestPlan {
    /// Run these arguments; `executed` is the provenance evidence, `None` when
    /// the run was full because the decision itself said so (the decision's own
    /// reason is then the honest report). `report` is `Some` exactly for a
    /// narrowed run — the only run whose result cannot be read off the exit
    /// code — and names the file its JSON reporter must produce.
    Run {
        args: Vec<String>,
        executed: Option<crate::checks::scope::ExecutedScope>,
        report: Option<VitestReportFile>,
    },
    /// Execute nothing: the decision selected no input.
    Skip,
}

fn plan_vitest_run(config: &Config, run_dir: &Path) -> VitestPlan {
    use crate::checks::scope::{Ecosystem, ExecutedScope, ScopeDecision, reason};

    let Some(decision) = config
        .test_scope
        .as_ref()
        .map(|scope| scope.get(Ecosystem::Vitest))
    else {
        return VitestPlan::Run {
            args: vitest_args(config),
            executed: None,
            report: None,
        };
    };
    let ScopeDecision::ChangeScoped {
        selector_inputs, ..
    } = decision
    else {
        return VitestPlan::Run {
            args: vitest_args(config),
            executed: None,
            report: None,
        };
    };
    if selector_inputs.is_empty() {
        return VitestPlan::Skip;
    }
    // Contract §4.1: the inputs must exist on disk in the tree that is about to
    // be read. The decision was made against the reviewed tree's path list; if
    // one of those paths is not actually there, the selection describes a tree
    // this command is not going to read, and handing Vitest a missing file
    // would silently shrink the selection instead of failing it.
    if let Some(missing) = selector_inputs
        .iter()
        .find(|input| !run_dir.join(input).exists())
    {
        return VitestPlan::Run {
            args: vitest_args(config),
            executed: Some(ExecutedScope::Full {
                reason: reason::resolution_failed(&format!(
                    "{missing} is missing from the reviewed tree"
                )),
            }),
            report: None,
        };
    }
    // A narrowed run is only allowed to be believed if it can prove what it
    // executed, and the proof is a file. With nowhere to write it the narrowing
    // itself is abandoned — widening keeps the review honest, while running
    // narrow-but-unverifiable would trade a real verdict for a guess.
    let report = match VitestReportFile::create() {
        Ok(report) => report,
        Err(error) => {
            return VitestPlan::Run {
                args: vitest_args(config),
                executed: Some(ExecutedScope::Full {
                    reason: reason::resolution_failed(&format!(
                        "the narrowed run has nowhere to write its JSON report ({error})"
                    )),
                }),
                report: None,
            };
        }
    };
    let selector_args = vitest_scoped_args(config, selector_inputs);
    // The whole argument line from the subcommand onward: with Vitest the
    // SUBCOMMAND is half the selector, so the fragment that expresses the
    // selection is the invocation itself. Rendered from the very arguments
    // about to be spawned, so it cannot drift from `provenance.command`. The
    // reporter flags are appended after it and stay OUT of the selector: they
    // are how the run is observed, not what it selects, and their temporary
    // path would change from run to run.
    let selector = selector_args.join(" ");
    let mut args = selector_args;
    args.extend(vitest_reporter_args(&report.path));
    VitestPlan::Run {
        executed: Some(ExecutedScope::ChangeScoped {
            selected: selector_inputs.len(),
            selector,
        }),
        args,
        report: Some(report),
    }
}

/// Build output, by prview's own built-in knowledge of where tools write it.
///
/// Split out from [`is_generated_artifact_path`] because the two questions are
/// not the same one. THIS answers "is this file a tool's output rather than
/// source", which is a fact about the repository layout. The other one also
/// folds in the operator's `lint_ignore_patterns`, which answer "do I want to
/// see lint findings here" — a preference, and one that says nothing about
/// whether a test reads the file.
///
/// Test selection must use this half alone. A repository that lint-ignores
/// `src/legacy/**` still has tests importing `src/legacy/foo.ts`, and dropping
/// that path from the selector inputs would stop running them without anyone
/// asking for it — invisible, because the lint setting is where nobody would
/// look for it.
pub(crate) fn is_builtin_generated_output_path(path: &str) -> bool {
    let normalized = path.replace('\\', "/");
    normalized.contains("/node_modules/")
        || normalized.starts_with("node_modules/")
        || normalized.contains("/coverage/")
        || normalized.starts_with("coverage/")
        || normalized.contains("/tmp/")
        || normalized.starts_with("tmp/")
        || normalized.contains("/dist/")
        || normalized.starts_with("dist/")
        || normalized.contains("/.next/")
        || normalized.starts_with(".next/")
        || normalized.starts_with("target/")
        || (normalized.contains("/target/")
            && (normalized.contains("/debug/") || normalized.contains("/release/")))
}

pub(crate) fn is_generated_artifact_path(path: &str, config: &Config) -> bool {
    if is_builtin_generated_output_path(path) {
        return true;
    }
    let normalized = path.replace('\\', "/");
    config.lint_ignore_patterns.iter().any(|pattern| {
        let stripped = pattern.trim_matches('*').trim_matches('/');
        !stripped.is_empty() && (normalized.contains(stripped) || normalized.starts_with(stripped))
    })
}

fn sanitize_grouped_lint_output<F>(
    output: &str,
    is_file_header: F,
    is_generated_path: impl Fn(&str) -> bool,
    is_finding_line: impl Fn(&str) -> Option<&'static str>,
) -> String
where
    F: Fn(&str) -> bool,
{
    let mut kept_lines = Vec::new();
    let mut current_block = Vec::new();
    let mut keep_current = false;
    let mut error_count = 0usize;
    let mut warning_count = 0usize;

    let flush_block =
        |block: &mut Vec<String>, keep: bool, kept: &mut Vec<String>, force_blank_line: bool| {
            if keep && !block.is_empty() {
                kept.append(block);
                if force_blank_line {
                    kept.push(String::new());
                }
            } else {
                block.clear();
            }
        };

    for line in output.lines() {
        let trimmed = line.trim_end();

        if is_file_header(trimmed) {
            flush_block(&mut current_block, keep_current, &mut kept_lines, true);
            keep_current = !is_generated_path(trimmed);
            current_block.push(trimmed.to_string());
            continue;
        }

        if trimmed.starts_with('✖') || trimmed.starts_with('✔') {
            continue;
        }

        if !current_block.is_empty() {
            current_block.push(line.to_string());
            if keep_current && let Some(level) = is_finding_line(trimmed) {
                match level {
                    "error" => error_count += 1,
                    "warning" => warning_count += 1,
                    _ => {}
                }
            }
        } else if !trimmed.is_empty() {
            kept_lines.push(line.to_string());
        }
    }

    flush_block(&mut current_block, keep_current, &mut kept_lines, false);

    while matches!(kept_lines.last(), Some(last) if last.is_empty()) {
        kept_lines.pop();
    }

    if error_count + warning_count > 0 {
        kept_lines.push(String::new());
        kept_lines.push(format!(
            "✖ {} problem{} ({} error{}, {} warning{})",
            error_count + warning_count,
            if error_count + warning_count == 1 {
                ""
            } else {
                "s"
            },
            error_count,
            if error_count == 1 { "" } else { "s" },
            warning_count,
            if warning_count == 1 { "" } else { "s" }
        ));
    }

    kept_lines.join("\n")
}

fn sanitize_eslint_output(output: &str, config: &Config) -> String {
    sanitize_grouped_lint_output(
        output,
        |line| line.starts_with('/') || line.contains(":\\"),
        |line| is_generated_artifact_path(line, config),
        |line| {
            if line.contains(" error ") {
                Some("error")
            } else if line.contains(" warning ") {
                Some("warning")
            } else {
                None
            }
        },
    )
}

fn sanitize_stylelint_output(output: &str, config: &Config) -> String {
    sanitize_grouped_lint_output(
        output,
        |line| !line.is_empty() && !line.starts_with(' ') && !line.starts_with('\t'),
        |line| is_generated_artifact_path(line, config),
        |line| {
            if line.contains("✖") || line.contains("×") {
                Some("error")
            } else if line.contains("⚠") || line.contains("‼") {
                Some("warning")
            } else {
                None
            }
        },
    )
}

#[async_trait]
impl Check for TypeScriptCheck {
    fn name(&self) -> &str {
        "TypeScript"
    }

    /// `tsc` exposes no stable worker-pool cap, so it must serialize.
    fn resource_weight(&self) -> crate::governor::Weight {
        crate::governor::Weight::Exclusive
    }

    fn check_eligibility(&self, config: &Config) -> super::CheckEligibility {
        if !config.profile.has_tsconfig {
            return super::CheckEligibility::Skip(format!(
                "profile {}",
                config.profile.kind.as_str().to_lowercase()
            ));
        }
        if config.is_fast_remote_only_standard() && !config.lint_forced {
            return super::CheckEligibility::Skip("fast remote-only preset".to_string());
        }
        if !js_tool_available("tsc", &config.repo_root) {
            return super::CheckEligibility::Skip(
                "tool not installed (node_modules/.bin/tsc is missing)".to_string(),
            );
        }
        super::CheckEligibility::Run
    }

    fn cache_key(&self, _config: &Config) -> Option<String> {
        // A sound tsc key must bind every effective source/config input plus
        // the compiler and dependency environment. The old *.ts/*.tsx hash
        // could replay a PASS after tsconfig, JS/JSX or toolchain changes.
        None
    }

    async fn run(&self, config: &Config) -> Result<CheckResult> {
        let start = std::time::Instant::now();
        let started_at = Local::now().to_rfc3339();

        let plan = plan_check_run(config)?;
        let run_dir = &plan.scan_dir;

        let output = run_js_command("tsc", &["--noEmit"], run_dir).await?;
        let finished_at = Local::now().to_rfc3339();

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let combined = format!("{}\n{}", stdout, stderr);

        let status = if output.status.success() {
            CheckStatus::Passed
        } else {
            CheckStatus::Failed
        };

        let js_runner = if which::which("pnpm").is_ok() {
            "pnpm exec"
        } else {
            "npx"
        };
        Ok(CheckResult {
            name: self.name().to_string(),
            status,
            duration: start.elapsed(),
            output: combined.clone(),
            cached: false,
            provenance: Some(
                CheckProvenance {
                    command: format!("{} tsc --noEmit", js_runner),
                    tool_version: None,
                    cwd: run_dir.display().to_string(),
                    exit_code: output.status.code(),
                    started_at,
                    finished_at,
                    hard_fail_signatures: find_hard_fail_signatures(&combined),
                    cache_key: self.cache_key(config),
                    target_sha: None,
                    tree_state: None,
                    executed_scope: None,
                }
                .with_scan_substrate(self.name(), run_dir, &config.repo_root),
            ),
        })
    }
}

#[async_trait]
impl Check for ESLintCheck {
    fn name(&self) -> &str {
        "ESLint"
    }

    /// ESLint worker controls vary by installed major version; serialize instead
    /// of passing an option an older project may reject.
    fn resource_weight(&self) -> crate::governor::Weight {
        crate::governor::Weight::Exclusive
    }

    fn check_eligibility(&self, config: &Config) -> super::CheckEligibility {
        if !config.profile.has_package_json {
            return super::CheckEligibility::Skip(format!(
                "profile {}",
                config.profile.kind.as_str().to_lowercase()
            ));
        }
        if config.is_fast_remote_only_standard() && !config.lint_forced {
            return super::CheckEligibility::Skip("fast remote-only preset".to_string());
        }
        if !config.run_lint {
            return super::CheckEligibility::Skip("lint disabled".to_string());
        }
        if !js_tool_available("eslint", &config.repo_root) {
            return super::CheckEligibility::Skip(
                "tool not installed (node_modules/.bin/eslint is missing)".to_string(),
            );
        }
        super::CheckEligibility::Run
    }

    fn cache_key(&self, _config: &Config) -> Option<String> {
        // ESLint consumes more than TS sources (JS/JSX, config, ignore rules,
        // plugins and CLI policy), so source-only replay is not truth-preserving.
        None
    }

    async fn run(&self, config: &Config) -> Result<CheckResult> {
        let start = std::time::Instant::now();
        let started_at = Local::now().to_rfc3339();

        let plan = plan_check_run(config)?;
        let run_dir = &plan.scan_dir;

        let args = eslint_args(config);
        let args_ref: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        let output = run_js_command("eslint", &args_ref, run_dir).await?;
        let finished_at = Local::now().to_rfc3339();

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let combined = format!("{}\n{}", stdout, stderr);
        let filtered_output = sanitize_eslint_output(&combined, config);

        let status = classify_eslint_status(output.status.success(), &filtered_output);

        let js_runner = if which::which("pnpm").is_ok() {
            "pnpm exec"
        } else {
            "npx"
        };
        Ok(CheckResult {
            name: self.name().to_string(),
            status,
            duration: start.elapsed(),
            output: filtered_output.clone(),
            cached: false,
            provenance: Some(
                CheckProvenance {
                    command: format!("{} eslint {}", js_runner, args.join(" ")),
                    tool_version: None,
                    cwd: run_dir.display().to_string(),
                    exit_code: output.status.code(),
                    started_at,
                    finished_at,
                    hard_fail_signatures: find_hard_fail_signatures(&combined),
                    cache_key: self.cache_key(config),
                    target_sha: None,
                    tree_state: None,
                    executed_scope: None,
                }
                .with_scan_substrate(self.name(), run_dir, &config.repo_root),
            ),
        })
    }
}

fn classify_eslint_status(command_succeeded: bool, output: &str) -> CheckStatus {
    if command_succeeded || output.trim().is_empty() {
        return CheckStatus::Passed;
    }

    if let Some((_, error_count, warning_count)) = eslint_problem_counts(output) {
        if error_count == 0 && warning_count > 0 {
            return CheckStatus::Warnings;
        }

        if error_count > 0 {
            return CheckStatus::Failed;
        }
    }

    let has_warning_finding = output.lines().any(|line| {
        let trimmed = line.trim();
        trimmed.contains(" warning ") || trimmed.starts_with("warning ")
    });
    let has_error_finding = output.lines().any(|line| {
        let trimmed = line.trim();
        trimmed.contains(" error ") || trimmed.starts_with("error ")
    });

    if has_warning_finding && !has_error_finding {
        return CheckStatus::Warnings;
    }

    CheckStatus::Failed
}

fn eslint_problem_counts(output: &str) -> Option<(usize, usize, usize)> {
    output.lines().find_map(|line| {
        let lower = line.to_ascii_lowercase();
        if !lower.contains("problem") || !lower.contains("error") || !lower.contains("warning") {
            return None;
        }

        let counts: Vec<usize> = lower
            .split(|c: char| !c.is_ascii_digit())
            .filter(|segment| !segment.is_empty())
            .filter_map(|segment| segment.parse::<usize>().ok())
            .collect();

        if counts.len() >= 3 {
            Some((counts[0], counts[1], counts[2]))
        } else {
            None
        }
    })
}

#[async_trait]
impl Check for VitestCheck {
    fn name(&self) -> &str {
        "Vitest"
    }

    /// Heavy: see [`Check::resource_weight`] for the one list of tools that
    /// want the whole machine.
    fn resource_weight(&self) -> crate::governor::Weight {
        crate::governor::Weight::Heavy
    }

    fn check_eligibility(&self, config: &Config) -> super::CheckEligibility {
        if !config.profile.has_package_json {
            return super::CheckEligibility::Skip(format!(
                "profile {}",
                config.profile.kind.as_str().to_lowercase()
            ));
        }
        if config.is_fast_remote_only_standard() && !config.run_tests {
            return super::CheckEligibility::Skip("fast remote-only preset".to_string());
        }
        if !config.run_tests {
            return super::CheckEligibility::Skip("tests disabled".to_string());
        }
        if !js_tool_available("vitest", &config.repo_root) {
            return super::CheckEligibility::Skip(
                "tool not installed (node_modules/.bin/vitest is missing)".to_string(),
            );
        }
        super::CheckEligibility::Run
    }

    fn cache_key(&self, _config: &Config) -> Option<String> {
        // Tests shouldn't be cached - they might depend on external state
        None
    }

    async fn run(&self, config: &Config) -> Result<CheckResult> {
        let start = std::time::Instant::now();
        let started_at = Local::now().to_rfc3339();

        let plan = plan_check_run(config)?;
        let run_dir = &plan.scan_dir;

        // Vitest's supported worker cap bounds its descendant pool. Keep owned
        // strings because the limit is selected at runtime.
        let (args, executed_scope, report) = match plan_vitest_run(config, run_dir) {
            VitestPlan::Run {
                args,
                executed,
                report,
            } => (args, executed, report),
            // No command is spawned at all: the decision proved this change has
            // no related test. The row still carries provenance — no command and
            // no exit code, but the tree that was read to decide and the empty
            // selection itself, which is what proves the skip was earned.
            VitestPlan::Skip => {
                return Ok(CheckResult {
                    name: self.name().to_string(),
                    status: CheckStatus::Skipped,
                    duration: start.elapsed(),
                    output: crate::checks::scope::NO_TESTS_RELATED_TO_THE_CHANGE.to_string(),
                    cached: false,
                    provenance: Some(super::nothing_selected_provenance(
                        self.name(),
                        run_dir,
                        run_dir.display().to_string(),
                        &config.repo_root,
                        started_at,
                    )),
                });
            }
        };
        let args_ref: Vec<&str> = args.iter().map(String::as_str).collect();

        // Use longer timeout for tests
        let output =
            run_js_command_with_timeout("vitest", &args_ref, run_dir, TEST_TIMEOUT_SECS).await?;
        let finished_at = Local::now().to_rfc3339();

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let combined = format!("{}\n{}", stdout, stderr);

        // `--passWithNoTests` makes an empty related set exit 0, which is right
        // for the tool and wrong for a review: zero executed tests is not
        // evidence that anything passed (contract §2.4/§8.1). A narrowed run is
        // therefore classified from its JSON reporter, not from its exit code
        // and never from its prose — stdout belongs to the code under review,
        // which could print any sentence a substring match looks for.
        let selected_inputs = match &executed_scope {
            Some(crate::checks::scope::ExecutedScope::ChangeScoped { selected, .. }) => *selected,
            _ => 0,
        };
        let verdict = classify_vitest_outcome(
            report.as_ref().map(|report| report.path.as_path()),
            output.status.success(),
            selected_inputs,
            &combined,
        );

        let executed_scope = published_executed_scope(executed_scope, verdict.collected);

        let js_runner = if which::which("pnpm").is_ok() {
            "pnpm exec"
        } else {
            "npx"
        };
        let cmd_str = format!("{} vitest {}", js_runner, args.join(" "));
        Ok(CheckResult {
            name: self.name().to_string(),
            status: verdict.status,
            duration: start.elapsed(),
            output: verdict.output,
            cached: false,
            provenance: Some(
                CheckProvenance {
                    command: cmd_str,
                    tool_version: None,
                    cwd: run_dir.display().to_string(),
                    exit_code: output.status.code(),
                    started_at,
                    finished_at,
                    hard_fail_signatures: find_hard_fail_signatures(&combined),
                    cache_key: self.cache_key(config),
                    target_sha: None,
                    tree_state: None,
                    executed_scope: None,
                }
                .with_scan_substrate(self.name(), run_dir, &config.repo_root)
                .with_executed_scope(executed_scope),
            ),
        })
    }
}

#[async_trait]
impl Check for StylelintCheck {
    fn name(&self) -> &str {
        "Stylelint"
    }

    fn check_eligibility(&self, config: &Config) -> super::CheckEligibility {
        if !config.profile.has_package_json {
            return super::CheckEligibility::Skip(format!(
                "profile {}",
                config.profile.kind.as_str().to_lowercase()
            ));
        }
        if config.is_fast_remote_only_standard() && !config.lint_forced {
            return super::CheckEligibility::Skip("fast remote-only preset".to_string());
        }
        if !config.run_lint {
            return super::CheckEligibility::Skip("lint disabled".to_string());
        }
        if !js_tool_available("stylelint", &config.repo_root) {
            return super::CheckEligibility::Skip(
                "tool not installed (node_modules/.bin/stylelint is missing)".to_string(),
            );
        }
        super::CheckEligibility::Run
    }

    fn cache_key(&self, _config: &Config) -> Option<String> {
        // Stylelint's result also depends on ignore files, plugins, CLI policy
        // and the installed toolchain; the former content hash omitted them.
        None
    }

    async fn run(&self, config: &Config) -> Result<CheckResult> {
        let start = std::time::Instant::now();
        let started_at = Local::now().to_rfc3339();

        let plan = plan_check_run(config)?;
        let run_dir = &plan.scan_dir;

        let args = stylelint_args(config);
        let args_ref: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        let output = run_js_command("stylelint", &args_ref, run_dir).await?;
        let finished_at = Local::now().to_rfc3339();

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let combined = format!("{}\n{}", stdout, stderr);
        let filtered_output = sanitize_stylelint_output(&combined, config);

        let status = if output.status.success() || filtered_output.trim().is_empty() {
            CheckStatus::Passed
        } else if filtered_output.contains("No configuration provided")
            || filtered_output.contains("No files matching")
        {
            CheckStatus::Skipped
        } else if filtered_output.contains("warning") && !filtered_output.contains("error") {
            CheckStatus::Warnings
        } else {
            CheckStatus::Failed
        };

        let js_runner = if which::which("pnpm").is_ok() {
            "pnpm exec"
        } else {
            "npx"
        };
        Ok(CheckResult {
            name: self.name().to_string(),
            status,
            duration: start.elapsed(),
            output: filtered_output.clone(),
            cached: false,
            provenance: Some(
                CheckProvenance {
                    command: format!("{} stylelint {}", js_runner, args.join(" ")),
                    tool_version: None,
                    cwd: run_dir.display().to_string(),
                    exit_code: output.status.code(),
                    started_at,
                    finished_at,
                    hard_fail_signatures: find_hard_fail_signatures(&combined),
                    cache_key: self.cache_key(config),
                    target_sha: None,
                    tree_state: None,
                    executed_scope: None,
                }
                .with_scan_substrate(self.name(), run_dir, &config.repo_root),
            ),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{test_config_builder, test_js_profile};

    fn create_test_config(has_tsconfig: bool) -> Config {
        test_config_builder()
            .profile(test_js_profile(has_tsconfig))
            .do_fetch(false)
            .use_cache(false)
            .create_zip(false)
            .build()
    }

    #[test]
    fn test_typescript_check_name() {
        let check = TypeScriptCheck;
        assert_eq!(check.name(), "TypeScript");
    }

    #[test]
    fn test_typescript_check_requires_tsconfig() {
        // Without tsconfig, should not run regardless of tool availability
        let config = create_test_config(false);
        let check = TypeScriptCheck;
        assert!(matches!(
            check.check_eligibility(&config),
            super::super::CheckEligibility::Skip(_)
        ));
    }

    #[test]
    fn test_typescript_check_disables_unsafe_persistent_cache() {
        let config = create_test_config(true);
        let check = TypeScriptCheck;
        assert!(check.cache_key(&config).is_none());
    }

    #[test]
    fn test_eslint_disables_unsafe_persistent_cache() {
        let config = create_test_config(true);
        let check = ESLintCheck;
        assert!(check.cache_key(&config).is_none());
    }

    #[test]
    fn test_stylelint_disables_unsafe_persistent_cache() {
        let config = create_test_config(true);
        let check = StylelintCheck;
        assert!(check.cache_key(&config).is_none());
    }

    #[test]
    fn test_js_tool_available_nonexistent() {
        use super::js_tool_available;
        use std::path::PathBuf;
        // Non-existent path should return false
        assert!(!js_tool_available("tsc", &PathBuf::from("/nonexistent")));
    }

    #[test]
    fn test_eslint_skips_fast_remote_only_by_default() {
        let mut config = create_test_config(true);
        config.remote_only = true;
        let check = ESLintCheck;
        assert!(
            matches!(check.check_eligibility(&config), super::super::CheckEligibility::Skip(reason) if reason == "fast remote-only preset")
        );
    }

    #[test]
    fn test_stylelint_skips_fast_remote_only_by_default() {
        let mut config = create_test_config(true);
        config.remote_only = true;
        let check = StylelintCheck;
        assert!(
            matches!(check.check_eligibility(&config), super::super::CheckEligibility::Skip(reason) if reason == "fast remote-only preset")
        );
    }

    #[test]
    fn test_tsc_skips_fast_remote_only_by_default() {
        let mut config = create_test_config(true);
        config.remote_only = true;
        let check = TypeScriptCheck;
        assert!(
            matches!(check.check_eligibility(&config), super::super::CheckEligibility::Skip(reason) if reason == "fast remote-only preset")
        );
    }

    #[test]
    fn test_classify_eslint_status_warning_only_output() {
        let output = "\
/tmp/src/app.ts
  4:2  warning  Unexpected console statement  no-console

✖ 1 problem (0 errors, 1 warning)
";

        assert_eq!(classify_eslint_status(false, output), CheckStatus::Warnings);
    }

    #[test]
    fn test_classify_eslint_status_error_output() {
        let output = "\
/tmp/src/app.ts
  4:2  error  Unexpected console statement  no-console

✖ 1 problem (1 error, 0 warnings)
";

        assert_eq!(classify_eslint_status(false, output), CheckStatus::Failed);
    }

    #[test]
    fn test_classify_eslint_status_empty_filtered_output_passes() {
        assert_eq!(classify_eslint_status(false, ""), CheckStatus::Passed);
    }

    #[test]
    fn test_eslint_args_ignore_generated_directories() {
        let config = create_test_config(false);
        let args = eslint_args(&config);
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--ignore-pattern", "**/target/**"])
        );
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--ignore-pattern", "**/coverage/**"])
        );
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--ignore-pattern", "**/tmp/**"])
        );
    }

    #[test]
    fn vitest_args_preserve_project_ceiling_and_pattern() {
        let mut config = create_test_config(true);
        config.resource_plan.worker_limit = 3;
        config.tests_pattern = Some("critical path".to_string());

        assert_eq!(
            vitest_args(&config),
            vec![
                "run",
                "--maxWorkers",
                "1",
                "--testNamePattern",
                "critical path"
            ]
        );
    }

    #[test]
    fn test_eslint_args_additive_ignore_patterns() {
        let mut config = create_test_config(false);
        config.lint_ignore_patterns = vec!["**/custom_exclude/**".to_string()];

        let args = eslint_args(&config);
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--ignore-pattern", "**/target/**"])
        );
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--ignore-pattern", "**/custom_exclude/**"])
        );
    }

    #[test]
    fn test_stylelint_args_ignore_generated_directories() {
        let config = create_test_config(false);
        let args = stylelint_args(&config);
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--ignore-pattern", "**/target/**"])
        );
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--ignore-pattern", "**/coverage/**"])
        );
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--ignore-pattern", "**/tmp/**"])
        );
    }

    #[test]
    fn test_is_generated_artifact_path_matches_nested_tauri_target() {
        let config = create_test_config(false);
        assert!(is_generated_artifact_path("coverage/base.css", &config));
        assert!(is_generated_artifact_path(
            "tmp/tailwind.generated.css",
            &config
        ));
        assert!(!is_generated_artifact_path(
            "src/target/selector.rs",
            &config
        ));
    }

    #[test]
    fn test_sanitize_eslint_output_drops_generated_blocks_and_rebuilds_summary() {
        let output = "\
/Users/test/repo/src/main.ts
  4:2  error  Unexpected console statement  no-console

/Users/test/repo/src-tauri/target/release/build/MyApp/out/tauri-codegen-assets/foo.js
  1:1  error  Parsing error: Unexpected character 'x'

✖ 2 problems (2 errors, 0 warnings)
";
        let config = create_test_config(false);
        let filtered = sanitize_eslint_output(output, &config);

        assert!(filtered.contains("/Users/test/repo/src/main.ts"));
        assert!(!filtered.contains("tauri-codegen-assets/foo.js"));
        assert!(filtered.contains("✖ 1 problem (1 error, 0 warnings)"));
    }

    #[test]
    fn test_sanitize_stylelint_output_drops_generated_blocks_and_rebuilds_summary() {
        let output = "\
coverage/base.css
  5:1  ✖  Expected empty line before rule  rule-empty-line-before

src/styles/app.css
  10:5  ✖  Unexpected unit  unit-disallowed-list

✖ 2 problems (2 errors, 0 warnings)
";
        let config = create_test_config(false);
        let filtered = sanitize_stylelint_output(output, &config);

        assert!(!filtered.contains("coverage/base.css"));
        assert!(filtered.contains("src/styles/app.css"));
        assert!(filtered.contains("✖ 1 problem (1 error, 0 warnings)"));
    }

    #[test]
    fn test_eslint_problem_counts_parses_summary() {
        assert_eq!(
            eslint_problem_counts("✖ 2 problems (0 errors, 2 warnings)"),
            Some((2, 0, 2))
        );
    }
    // -----------------------------------------------------------------------
    // Change-scoped execution
    // -----------------------------------------------------------------------

    use crate::checks::scope::{ExecutedScope, ScopeDecision, ScopeDecisions};

    /// A config whose Vitest decision is exactly `decision`; Cargo is pinned to
    /// full so a stray read of the wrong ecosystem would be visible.
    fn config_with_vitest_scope(decision: ScopeDecision) -> Config {
        let mut config = create_test_config(true);
        config.test_scope = Some(ScopeDecisions {
            cargo: ScopeDecision::Full {
                reason: "not under test".to_string(),
                inputs: None,
            },
            vitest: decision,
            non_participating: Vec::new(),
        });
        config
    }

    fn vitest_scoped(inputs: &[&str]) -> ScopeDecision {
        ScopeDecision::ChangeScoped {
            inputs: inputs.len(),
            selected: inputs.iter().map(|i| (*i).to_string()).collect(),
            universe: None,
            selector_inputs: inputs.iter().map(|i| (*i).to_string()).collect(),
        }
    }

    /// A reviewed tree that really contains `files`, because the missing-input
    /// escalation is a filesystem fact.
    fn reviewed_tree_with(files: &[&str]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("temp dir");
        for file in files {
            let path = dir.path().join(file);
            std::fs::create_dir_all(path.parent().expect("a parent")).expect("mkdir");
            std::fs::write(path, "export const x = 1;\n").expect("write");
        }
        dir
    }

    fn vitest_run_args(plan: &VitestPlan) -> &[String] {
        match plan {
            VitestPlan::Run { args, .. } => args,
            VitestPlan::Skip => panic!("expected a run, got a skip"),
        }
    }

    fn vitest_executed(plan: &VitestPlan) -> Option<&ExecutedScope> {
        match plan {
            VitestPlan::Run { executed, .. } => executed.as_ref(),
            VitestPlan::Skip => panic!("expected a run, got a skip"),
        }
    }

    /// The reporter flags a narrowed run appends, with the temporary path it
    /// was actually given.
    fn vitest_reporter_tail(plan: &VitestPlan) -> Vec<String> {
        let VitestPlan::Run {
            report: Some(report),
            ..
        } = plan
        else {
            panic!("expected a narrowed run with a report file");
        };
        vitest_reporter_args(&report.path)
    }

    /// A JSON report exactly as Vitest's `json` reporter writes one, reduced to
    /// the fields the check reads.
    fn vitest_report_file(dir: &std::path::Path, suites: u64, tests: u64, files: usize) -> PathBuf {
        let results: Vec<serde_json::Value> = (0..files)
            .map(|index| serde_json::json!({ "name": format!("src/{index}.test.ts") }))
            .collect();
        let path = dir.join("vitest-scope.json");
        std::fs::write(
            &path,
            serde_json::json!({
                "numTotalTestSuites": suites,
                "numTotalTests": tests,
                "testResults": results,
            })
            .to_string(),
        )
        .expect("write report");
        path
    }

    #[test]
    fn a_scoped_vitest_decision_runs_only_the_related_tests() {
        let tree = reviewed_tree_with(&["src/a.ts", "src/b.ts"]);
        let config = config_with_vitest_scope(vitest_scoped(&["src/a.ts", "src/b.ts"]));

        let plan = plan_vitest_run(&config, tree.path());

        let mut expected = vec![
            "related".to_string(),
            "--run".to_string(),
            "--maxWorkers".to_string(),
            "1".to_string(),
            "--passWithNoTests".to_string(),
            "src/a.ts".to_string(),
            "src/b.ts".to_string(),
        ];
        // The reporter flags come last, after the positional inputs, so the
        // selector stays one contiguous fragment of the command line.
        expected.extend(vitest_reporter_tail(&plan));
        assert_eq!(vitest_run_args(&plan), expected.as_slice());

        let Some(ExecutedScope::ChangeScoped { selected, selector }) = vitest_executed(&plan)
        else {
            panic!("a narrowed run reports a narrowed scope");
        };
        assert_eq!(*selected, 2);
        assert_eq!(
            selector, "related --run --maxWorkers 1 --passWithNoTests src/a.ts src/b.ts",
            "the selector is what was selected, not how the run was observed",
        );
        assert!(
            vitest_run_args(&plan).join(" ").contains(selector.as_str()),
            "selector {selector:?} must be a verbatim fragment of the command line",
        );
    }

    #[test]
    fn a_full_vitest_decision_runs_todays_command_unchanged() {
        let tree = reviewed_tree_with(&[]);
        let config = config_with_vitest_scope(ScopeDecision::Full {
            reason: "unsupported input".to_string(),
            inputs: Some(2),
        });

        let plan = plan_vitest_run(&config, tree.path());

        assert_eq!(vitest_run_args(&plan), ["run", "--maxWorkers", "1"]);
        assert_eq!(vitest_executed(&plan), None);
    }

    #[test]
    fn an_empty_vitest_selection_plans_no_run_at_all() {
        let tree = reviewed_tree_with(&[]);
        let config = config_with_vitest_scope(ScopeDecision::ChangeScoped {
            inputs: 3,
            selected: Vec::new(),
            universe: None,
            selector_inputs: Vec::new(),
        });

        assert!(matches!(
            plan_vitest_run(&config, tree.path()),
            VitestPlan::Skip
        ));
    }

    #[test]
    fn an_input_missing_from_the_reviewed_tree_widens_the_run() {
        // Handing Vitest a path that is not there would silently shrink the
        // selection to whatever remains: a narrower run than the one decided.
        let tree = reviewed_tree_with(&["src/a.ts"]);
        let config = config_with_vitest_scope(vitest_scoped(&["src/a.ts", "src/gone.ts"]));

        let plan = plan_vitest_run(&config, tree.path());

        assert_eq!(vitest_run_args(&plan), ["run", "--maxWorkers", "1"]);
        let Some(ExecutedScope::Full { reason }) = vitest_executed(&plan) else {
            panic!("a missing input must widen the run, loudly");
        };
        assert!(
            reason.contains("src/gone.ts"),
            "the reason must name the missing input: {reason}",
        );
    }

    #[test]
    fn the_tests_pattern_filters_inside_the_vitest_selection() {
        let tree = reviewed_tree_with(&["src/a.ts"]);
        let mut config = config_with_vitest_scope(vitest_scoped(&["src/a.ts"]));
        config.tests_pattern = Some("renders".to_string());

        let plan = plan_vitest_run(&config, tree.path());
        let mut expected = vec![
            "related".to_string(),
            "--run".to_string(),
            "--maxWorkers".to_string(),
            "1".to_string(),
            "--passWithNoTests".to_string(),
            "--testNamePattern".to_string(),
            "renders".to_string(),
            "src/a.ts".to_string(),
        ];
        expected.extend(vitest_reporter_tail(&plan));
        assert_eq!(vitest_run_args(&plan), expected.as_slice());
    }

    #[test]
    fn a_narrowed_run_that_collected_nothing_is_a_skip() {
        // Vitest's own counters, not its prose: zero suites and zero collected
        // files is the one shape that means "no test relates to this change".
        let dir = tempfile::tempdir().expect("temp dir");
        let report = vitest_report_file(dir.path(), 0, 0, 0);

        let verdict = classify_vitest_outcome(
            Some(&report),
            true,
            2,
            "No test files found, exiting with code 0\n",
        );

        assert_eq!(verdict.status, CheckStatus::Skipped);
        assert!(
            verdict
                .output
                .starts_with(crate::checks::scope::NO_TESTS_RELATED_TO_THE_CHANGE)
        );
        assert_eq!(
            verdict.collected,
            Some(0),
            "a skip after a real run selected no test file, and says so",
        );
    }

    #[test]
    fn a_narrowed_run_that_executed_tests_cannot_be_spoofed_into_a_skip() {
        // The reviewed code owns stdout. A test that prints Vitest's empty-set
        // sentence must not turn an executed, passing run into a skip.
        let dir = tempfile::tempdir().expect("temp dir");
        let report = vitest_report_file(dir.path(), 3, 7, 2);

        let verdict = classify_vitest_outcome(
            Some(&report),
            true,
            2,
            "stdout: No test files found\n Test Files  2 passed (2)\n",
        );

        assert_eq!(verdict.status, CheckStatus::Passed);
        assert!(
            !verdict
                .output
                .contains(crate::checks::scope::NO_TESTS_RELATED_TO_THE_CHANGE)
        );
        assert_eq!(
            verdict.collected,
            Some(2),
            "the published selection counts the test files the reporter collected",
        );
    }

    #[test]
    fn a_narrowed_run_without_reporter_output_is_an_error_not_a_pass() {
        // Exit 0 plus `--passWithNoTests` proves nothing at all. Without the
        // reporter there is no witness, and an unverified run is neither green
        // nor a skip.
        let dir = tempfile::tempdir().expect("temp dir");
        let missing = dir.path().join("vitest-scope.json");

        let verdict = classify_vitest_outcome(Some(&missing), true, 2, "all good\n");

        assert_eq!(verdict.status, CheckStatus::Error);
        assert!(
            verdict
                .output
                .contains("could not verify that the narrowed Vitest run executed any test"),
            "the error must say what could not be verified: {}",
            verdict.output,
        );
        assert_eq!(
            verdict.collected, None,
            "an unreadable report counts nothing, rather than counting its inputs",
        );
    }

    #[test]
    fn a_narrowed_run_whose_report_is_unreadable_is_an_error() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("vitest-scope.json");
        std::fs::write(&path, "{ not json").expect("write report");

        let verdict = classify_vitest_outcome(Some(&path), true, 1, "");

        assert_eq!(verdict.status, CheckStatus::Error);
    }

    #[test]
    fn a_full_run_is_still_classified_by_its_exit_code_alone() {
        assert_eq!(
            classify_vitest_outcome(None, true, 0, "No test files found\n").status,
            CheckStatus::Passed,
        );
        assert_eq!(
            classify_vitest_outcome(None, false, 0, "boom\n").status,
            CheckStatus::Failed,
        );
    }

    /// Contract §7: for Vitest the selected unit is a test file. The plan can
    /// only name the changed sources it hands to `vitest related`; how many test
    /// files those pull in is the reporter's answer, and it is the one the pack
    /// publishes.
    #[test]
    fn the_published_selection_counts_test_files_not_changed_sources() {
        use crate::checks::scope::ExecutedScope;

        let planned = || {
            Some(ExecutedScope::ChangeScoped {
                selected: 3,
                selector: "related --run src/a.ts src/b.ts src/c.ts".to_string(),
            })
        };

        assert_eq!(
            published_executed_scope(planned(), Some(1)),
            Some(ExecutedScope::ChangeScoped {
                selected: 1,
                selector: "related --run src/a.ts src/b.ts src/c.ts".to_string(),
            }),
            "three changed sources can import a single test file",
        );
        assert_eq!(
            published_executed_scope(planned(), Some(0)),
            Some(ExecutedScope::ChangeScoped {
                selected: 0,
                selector: "related --run src/a.ts src/b.ts src/c.ts".to_string(),
            }),
            "a run that collected nothing selected nothing, and keeps its selector",
        );
        assert_eq!(
            published_executed_scope(planned(), None),
            Some(ExecutedScope::ChangeScoped {
                selected: 0,
                selector: "related --run src/a.ts src/b.ts src/c.ts".to_string(),
            }),
            "an unreadable report proves no selection, and must not publish the input count",
        );

        let escalated = Some(ExecutedScope::Full {
            reason: "manifest or lockfile changed: package-lock.json".to_string(),
        });
        assert_eq!(
            published_executed_scope(escalated.clone(), Some(7)),
            escalated,
            "a full run has no selection to count",
        );
        assert_eq!(published_executed_scope(None, Some(7)), None);
    }

    #[test]
    fn a_failing_narrowed_run_stays_a_failure() {
        let dir = tempfile::tempdir().expect("temp dir");
        let report = vitest_report_file(dir.path(), 1, 1, 1);

        let verdict = classify_vitest_outcome(Some(&report), false, 1, "1 failed\n");

        assert_eq!(verdict.status, CheckStatus::Failed);
    }

    #[test]
    fn an_operators_lint_ignore_does_not_hide_a_file_from_test_selection() {
        // `lint_ignore_patterns` says "do not lint this", never "this file
        // cannot affect a test". Test selection reads only the built-in
        // build-output half of the predicate.
        let mut config = create_test_config(true);
        config.lint_ignore_patterns = vec!["src/legacy/**".to_string()];

        assert!(is_generated_artifact_path("src/legacy/foo.ts", &config));
        assert!(!is_builtin_generated_output_path("src/legacy/foo.ts"));
        assert!(is_builtin_generated_output_path("dist/foo.js"));
        assert!(is_builtin_generated_output_path(
            "node_modules/pkg/index.js"
        ));
    }
}
