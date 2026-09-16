//! TypeScript and JavaScript checks (tsc, eslint, vitest)

use super::{
    Check, CheckProvenance, CheckResult, CheckStatus, TEST_TIMEOUT_SECS, find_hard_fail_signatures,
    js_tool_available, plan_check_run, run_js_command, run_js_command_with_timeout,
};
use crate::Config;
use anyhow::Result;
use async_trait::async_trait;
use chrono::Local;

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
/// emptiness is recognised from the output instead and reported as `Skipped`
/// (contract §8.1) — never `Passed`, which would claim evidence from zero
/// executed tests.
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

/// Vitest's own words for "the selection resolved to no spec at all".
///
/// Matched case-insensitively on the phrase rather than the whole sentence: 3.x
/// and 4.x differ in the tail (`, exiting with code 0`) and in whether a filter
/// is echoed after it, and the phrase is what both builds print. Only ever
/// consulted for a run that carried `--passWithNoTests`, where exit 0 alone
/// cannot distinguish "everything passed" from "nothing ran".
fn vitest_found_no_test_files(output: &str) -> bool {
    output.to_ascii_lowercase().contains("no test files found")
}

/// What Vitest will run, and the evidence of it, given this run's scope
/// decision.
///
/// `Err` is not used for tool failure — it is the one shape that says "no
/// command at all", which is what an empty selection means.
enum VitestPlan {
    /// Run these arguments; `executed` is the provenance evidence, `None` when
    /// the run was full because the decision itself said so (the decision's own
    /// reason is then the honest report).
    Run {
        args: Vec<String>,
        executed: Option<crate::checks::scope::ExecutedScope>,
    },
    /// Execute nothing: the decision selected no input.
    Skip,
}

fn plan_vitest_run(config: &Config, run_dir: &std::path::Path) -> VitestPlan {
    use crate::checks::scope::{Ecosystem, ExecutedScope, ScopeDecision, reason};

    let Some(decision) = config
        .test_scope
        .as_ref()
        .map(|scope| scope.get(Ecosystem::Vitest))
    else {
        return VitestPlan::Run {
            args: vitest_args(config),
            executed: None,
        };
    };
    let ScopeDecision::ChangeScoped {
        selector_inputs, ..
    } = decision
    else {
        return VitestPlan::Run {
            args: vitest_args(config),
            executed: None,
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
        };
    }
    let args = vitest_scoped_args(config, selector_inputs);
    VitestPlan::Run {
        executed: Some(ExecutedScope::ChangeScoped {
            selected: selector_inputs.len(),
            // The whole argument line from the subcommand onward: with Vitest
            // the SUBCOMMAND is half the selector, so the fragment that
            // expresses the selection is the invocation itself. Rendered from
            // the very arguments about to be spawned, so it cannot drift from
            // `provenance.command`.
            selector: args.join(" "),
        }),
        args,
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
        let (args, executed_scope) = match plan_vitest_run(config, run_dir) {
            VitestPlan::Run { args, executed } => (args, executed),
            // No command is spawned at all: the decision proved this change has
            // no related test. Provenance stays `None` because there is no
            // execution to describe — no command, no exit code, no tree read.
            VitestPlan::Skip => {
                return Ok(CheckResult {
                    name: self.name().to_string(),
                    status: CheckStatus::Skipped,
                    duration: start.elapsed(),
                    output: crate::checks::scope::NO_TESTS_RELATED_TO_THE_CHANGE.to_string(),
                    cached: false,
                    provenance: None,
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
        // evidence that anything passed (contract §2.4/§8.1). Recognised only
        // on a narrowed run, since that is the only one that carries the flag.
        let ran_narrowed = matches!(
            executed_scope,
            Some(crate::checks::scope::ExecutedScope::ChangeScoped { .. })
        );
        let selected_inputs = match &executed_scope {
            Some(crate::checks::scope::ExecutedScope::ChangeScoped { selected, .. }) => *selected,
            _ => 0,
        };
        let (status, output_text) = if output.status.success() {
            if ran_narrowed && vitest_found_no_test_files(&combined) {
                (
                    CheckStatus::Skipped,
                    format!(
                        "{}\nVitest found no test file importing any of the {selected_inputs} \
                         changed source file(s).\n{combined}",
                        crate::checks::scope::NO_TESTS_RELATED_TO_THE_CHANGE
                    ),
                )
            } else {
                (CheckStatus::Passed, combined.clone())
            }
        } else {
            (CheckStatus::Failed, combined.clone())
        };

        let js_runner = if which::which("pnpm").is_ok() {
            "pnpm exec"
        } else {
            "npx"
        };
        let cmd_str = format!("{} vitest {}", js_runner, args.join(" "));
        Ok(CheckResult {
            name: self.name().to_string(),
            status,
            duration: start.elapsed(),
            output: output_text,
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

    #[test]
    fn a_scoped_vitest_decision_runs_only_the_related_tests() {
        let tree = reviewed_tree_with(&["src/a.ts", "src/b.ts"]);
        let config = config_with_vitest_scope(vitest_scoped(&["src/a.ts", "src/b.ts"]));

        let plan = plan_vitest_run(&config, tree.path());

        assert_eq!(
            vitest_run_args(&plan),
            [
                "related",
                "--run",
                "--maxWorkers",
                "1",
                "--passWithNoTests",
                "src/a.ts",
                "src/b.ts"
            ],
        );
        let Some(ExecutedScope::ChangeScoped { selected, selector }) = vitest_executed(&plan)
        else {
            panic!("a narrowed run reports a narrowed scope");
        };
        assert_eq!(*selected, 2);
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

        assert_eq!(
            vitest_run_args(&plan_vitest_run(&config, tree.path())),
            [
                "related",
                "--run",
                "--maxWorkers",
                "1",
                "--passWithNoTests",
                "--testNamePattern",
                "renders",
                "src/a.ts"
            ],
        );
    }

    #[test]
    fn an_empty_related_set_is_recognised_in_vitest_output() {
        assert!(vitest_found_no_test_files(
            "No test files found, exiting with code 0\n"
        ));
        assert!(vitest_found_no_test_files("no test files found"));
        assert!(!vitest_found_no_test_files(
            " Test Files  2 passed (2)\n      Tests  7 passed (7)\n"
        ));
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
