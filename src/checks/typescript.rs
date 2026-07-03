//! TypeScript and JavaScript checks (tsc, eslint, vitest)

use super::{
    Check, CheckProvenance, CheckResult, CheckStatus, TEST_TIMEOUT_SECS, find_hard_fail_signatures,
    js_tool_available, run_js_command, run_js_command_with_timeout,
};
use crate::Config;
use crate::cache;
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

fn is_generated_artifact_path(path: &str, config: &Config) -> bool {
    let normalized = path.replace('\\', "/");
    let mut is_gen = normalized.contains("/node_modules/")
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
            && (normalized.contains("/debug/") || normalized.contains("/release/")));

    for pattern in &config.lint_ignore_patterns {
        let stripped = pattern.trim_matches('*').trim_matches('/');
        if !stripped.is_empty()
            && (normalized.contains(stripped) || normalized.starts_with(stripped))
        {
            is_gen = true;
        }
    }
    is_gen
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

    fn cache_key(&self, config: &Config) -> Option<String> {
        Some(format!("tsc-{}", cache::ts_hash(&config.repo_root)))
    }

    async fn run(&self, config: &Config) -> Result<CheckResult> {
        let start = std::time::Instant::now();
        let started_at = Local::now().to_rfc3339();

        let output = run_js_command("tsc", &["--noEmit"], &config.repo_root).await?;
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
            provenance: Some(CheckProvenance {
                command: format!("{} tsc --noEmit", js_runner),
                tool_version: None,
                cwd: config.repo_root.display().to_string(),
                exit_code: output.status.code(),
                started_at,
                finished_at,
                hard_fail_signatures: find_hard_fail_signatures(&combined),
                cache_key: self.cache_key(config),
            }),
        })
    }
}

#[async_trait]
impl Check for ESLintCheck {
    fn name(&self) -> &str {
        "ESLint"
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

    fn cache_key(&self, config: &Config) -> Option<String> {
        Some(format!("eslint-{}", cache::ts_hash(&config.repo_root)))
    }

    async fn run(&self, config: &Config) -> Result<CheckResult> {
        let start = std::time::Instant::now();
        let started_at = Local::now().to_rfc3339();

        let args = eslint_args(config);
        let args_ref: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        let output = run_js_command("eslint", &args_ref, &config.repo_root).await?;
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
            provenance: Some(CheckProvenance {
                command: format!("{} eslint {}", js_runner, args.join(" ")),
                tool_version: None,
                cwd: config.repo_root.display().to_string(),
                exit_code: output.status.code(),
                started_at,
                finished_at,
                hard_fail_signatures: find_hard_fail_signatures(&combined),
                cache_key: self.cache_key(config),
            }),
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

        // Build args
        let mut args = vec!["run"];

        // Add pattern filter if specified
        let pattern_args: Vec<String>;
        if let Some(pattern) = &config.tests_pattern {
            pattern_args = vec!["--grep".to_string(), pattern.clone()];
            args.extend(pattern_args.iter().map(|s| s.as_str()));
        }

        // Use longer timeout for tests
        let output =
            run_js_command_with_timeout("vitest", &args, &config.repo_root, TEST_TIMEOUT_SECS)
                .await?;
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
        let cmd_str = format!("{} vitest {}", js_runner, args.join(" "));
        Ok(CheckResult {
            name: self.name().to_string(),
            status,
            duration: start.elapsed(),
            output: combined.clone(),
            cached: false,
            provenance: Some(CheckProvenance {
                command: cmd_str,
                tool_version: None,
                cwd: config.repo_root.display().to_string(),
                exit_code: output.status.code(),
                started_at,
                finished_at,
                hard_fail_signatures: find_hard_fail_signatures(&combined),
                cache_key: self.cache_key(config),
            }),
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

    fn cache_key(&self, config: &Config) -> Option<String> {
        Some(format!(
            "stylelint-{}",
            cache::stylelint_hash(&config.repo_root)
        ))
    }

    async fn run(&self, config: &Config) -> Result<CheckResult> {
        let start = std::time::Instant::now();
        let started_at = Local::now().to_rfc3339();

        let args = stylelint_args(config);
        let args_ref: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        let output = run_js_command("stylelint", &args_ref, &config.repo_root).await?;
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
            provenance: Some(CheckProvenance {
                command: format!("{} stylelint {}", js_runner, args.join(" ")),
                tool_version: None,
                cwd: config.repo_root.display().to_string(),
                exit_code: output.status.code(),
                started_at,
                finished_at,
                hard_fail_signatures: find_hard_fail_signatures(&combined),
                cache_key: self.cache_key(config),
            }),
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
    fn test_typescript_check_cache_key() {
        let config = create_test_config(true);
        let check = TypeScriptCheck;
        let key = check.cache_key(&config);
        assert!(key.is_some());
        // Verify it has prefix
        assert!(key.unwrap().starts_with("tsc-"));
    }

    #[test]
    fn test_eslint_cache_key_has_prefix() {
        let config = create_test_config(true);
        let check = ESLintCheck;
        let key = check.cache_key(&config);
        assert!(key.is_some());
        assert!(key.unwrap().starts_with("eslint-"));
    }

    #[test]
    fn test_stylelint_cache_key_has_prefix() {
        let config = create_test_config(true);
        let check = StylelintCheck;
        let key = check.cache_key(&config);
        assert!(key.is_some());
        assert!(key.unwrap().starts_with("stylelint-"));
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
}
