//! Rust/Cargo checks

use super::{
    Check, CheckResult, CheckStatus, ProvenanceBuilder, TEST_TIMEOUT_SECS, has_tool_crash,
    run_command, run_command_with_timeout,
};
use crate::Config;
use crate::cache;
use anyhow::Result;
use async_trait::async_trait;
use chrono::Local;
use std::path::Path;

pub struct CargoCheck;
pub struct ClippyCheck;
pub struct CargoTestCheck;
pub struct RustfmtCheck;
pub struct CargoAuditCheck;
pub struct CargoGeigerCheck;

#[async_trait]
impl Check for CargoCheck {
    fn name(&self) -> &str {
        "Cargo check"
    }

    fn check_eligibility(&self, config: &Config) -> super::CheckEligibility {
        if !config.profile.has_cargo {
            return super::CheckEligibility::Skip(format!(
                "profile {}",
                config.profile.kind.as_str().to_lowercase()
            ));
        }
        super::CheckEligibility::Run
    }

    fn cache_key(&self, config: &Config) -> Option<String> {
        Some(cache::rust_hash(&config.repo_root))
    }

    async fn run(&self, config: &Config) -> Result<CheckResult> {
        let start = std::time::Instant::now();
        let started_at = Local::now().to_rfc3339();

        let cwd = config
            .profile
            .cargo_root
            .as_ref()
            .unwrap_or(&config.repo_root);

        let args = &["check", "--message-format=short"];
        let output = run_command("cargo", args, cwd).await?;
        let finished_at = Local::now().to_rfc3339();

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let combined = format!("{}\n{}", stdout, stderr);

        let status = if output.status.success() {
            CheckStatus::Passed
        } else {
            CheckStatus::Failed
        };

        Ok(CheckResult {
            name: self.name().to_string(),
            status,
            duration: start.elapsed(),
            output: combined.clone(),
            cached: false,
            provenance: Some(
                ProvenanceBuilder {
                    cmd: "cargo",
                    args,
                    cwd,
                    output: &output,
                    combined_output: &combined,
                    started_at: &started_at,
                    finished_at: &finished_at,
                    cache_key: self.cache_key(config),
                }
                .build_with_repo_root(Some(&config.repo_root)),
            ),
        })
    }
}

#[async_trait]
impl Check for ClippyCheck {
    fn name(&self) -> &str {
        "Clippy"
    }

    fn check_eligibility(&self, config: &Config) -> super::CheckEligibility {
        if !config.profile.has_cargo {
            return super::CheckEligibility::Skip(format!(
                "profile {}",
                config.profile.kind.as_str().to_lowercase()
            ));
        }
        if config.is_fast_remote_only_standard() && !config.should_run_heavy_rust_lint() {
            return super::CheckEligibility::Skip("fast remote-only preset".to_string());
        }
        if !config.run_lint {
            return super::CheckEligibility::Skip("lint disabled".to_string());
        }
        if !config.should_run_heavy_rust_lint() {
            return super::CheckEligibility::Skip("lint disabled".to_string());
        }
        super::CheckEligibility::Run
    }

    fn cache_key(&self, config: &Config) -> Option<String> {
        Some(format!("clippy-{}", cache::rust_hash(&config.repo_root)))
    }

    async fn run(&self, config: &Config) -> Result<CheckResult> {
        let start = std::time::Instant::now();
        let started_at = Local::now().to_rfc3339();

        let cwd = config
            .profile
            .cargo_root
            .as_ref()
            .unwrap_or(&config.repo_root);

        let args = &["clippy", "--message-format=short", "--", "-D", "warnings"];
        let output = run_command("cargo", args, cwd).await?;
        let finished_at = Local::now().to_rfc3339();

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let combined = format!("{}\n{}", stdout, stderr);

        let status = if output.status.success() {
            if clippy_has_real_warnings(&combined) {
                CheckStatus::Warnings
            } else {
                CheckStatus::Passed
            }
        } else {
            CheckStatus::Failed
        };

        Ok(CheckResult {
            name: self.name().to_string(),
            status,
            duration: start.elapsed(),
            output: combined.clone(),
            cached: false,
            provenance: Some(
                ProvenanceBuilder {
                    cmd: "cargo",
                    args,
                    cwd,
                    output: &output,
                    combined_output: &combined,
                    started_at: &started_at,
                    finished_at: &finished_at,
                    cache_key: self.cache_key(config),
                }
                .build_with_repo_root(Some(&config.repo_root)),
            ),
        })
    }
}

/// Detect a cargo build-script (`build.rs`) warning print.
///
/// `cargo::warning=` / `cargo:warning=` output from a dependency's build script
/// is surfaced by the compiler driver as a line shaped like
/// `warning: <pkg>@<version>: <message>` (e.g.
/// `warning: codescribe-core@0.12.2: Embedding MiniLM model from: ...`). These
/// are diagnostic prints from a `build.rs`, not rustc/clippy lints, so they must
/// not flip an otherwise-clean clippy run to a WARN status. They are recognised
/// by the `name@version:` prefix that immediately follows the `warning: ` marker
/// — a real lint message never carries an `@version` token before its first
/// `": "` separator.
fn is_cargo_build_script_warning(line: &str) -> bool {
    let trimmed = line.trim_start();
    let Some(rest) = trimmed.strip_prefix("warning: ") else {
        return false;
    };
    match rest.split_once(": ") {
        Some((prefix, _)) => prefix.contains('@') && !prefix.contains(char::is_whitespace),
        None => false,
    }
}

/// True when clippy/rustc emitted at least one real lint warning, ignoring
/// build-script `cargo:warning=` noise emitted by dependencies' `build.rs`.
fn clippy_has_real_warnings(combined: &str) -> bool {
    combined
        .lines()
        .filter(|line| line.contains("warning:"))
        .any(|line| !is_cargo_build_script_warning(line))
}

#[async_trait]
impl Check for CargoTestCheck {
    fn name(&self) -> &str {
        "Cargo test"
    }

    fn check_eligibility(&self, config: &Config) -> super::CheckEligibility {
        if !config.profile.has_cargo {
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
        super::CheckEligibility::Run
    }

    fn cache_key(&self, _config: &Config) -> Option<String> {
        // Tests shouldn't be cached
        None
    }

    async fn run(&self, config: &Config) -> Result<CheckResult> {
        let start = std::time::Instant::now();
        let started_at = Local::now().to_rfc3339();

        let cwd = config
            .profile
            .cargo_root
            .as_ref()
            .unwrap_or(&config.repo_root);

        let args = &["test", "--all-targets", "--no-fail-fast"];
        let output = run_command_with_timeout("cargo", args, cwd, TEST_TIMEOUT_SECS).await?;
        let finished_at = Local::now().to_rfc3339();

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let combined = format!("{}\n{}", stdout, stderr);

        let status = if output.status.success() {
            CheckStatus::Passed
        } else {
            CheckStatus::Failed
        };

        Ok(CheckResult {
            name: self.name().to_string(),
            status,
            duration: start.elapsed(),
            output: combined.clone(),
            cached: false,
            provenance: Some(
                ProvenanceBuilder {
                    cmd: "cargo",
                    args,
                    cwd,
                    output: &output,
                    combined_output: &combined,
                    started_at: &started_at,
                    finished_at: &finished_at,
                    cache_key: self.cache_key(config),
                }
                .build_with_repo_root(Some(&config.repo_root)),
            ),
        })
    }
}

#[async_trait]
impl Check for RustfmtCheck {
    fn name(&self) -> &str {
        "Rustfmt"
    }

    fn check_eligibility(&self, config: &Config) -> super::CheckEligibility {
        if !config.profile.has_cargo {
            return super::CheckEligibility::Skip(format!(
                "profile {}",
                config.profile.kind.as_str().to_lowercase()
            ));
        }
        if config.is_fast_remote_only_standard() && !config.should_run_heavy_rust_lint() {
            return super::CheckEligibility::Skip("fast remote-only preset".to_string());
        }
        if !config.run_lint {
            return super::CheckEligibility::Skip("lint disabled".to_string());
        }
        if !config.should_run_heavy_rust_lint() {
            return super::CheckEligibility::Skip("lint disabled".to_string());
        }
        super::CheckEligibility::Run
    }

    fn cache_key(&self, config: &Config) -> Option<String> {
        Some(format!("rustfmt-{}", cache::rust_hash(&config.repo_root)))
    }

    async fn run(&self, config: &Config) -> Result<CheckResult> {
        let start = std::time::Instant::now();
        let started_at = Local::now().to_rfc3339();

        let cwd = config
            .profile
            .cargo_root
            .as_ref()
            .unwrap_or(&config.repo_root);

        let args = &["fmt", "--check"];
        let output = run_command("cargo", args, cwd).await?;
        let finished_at = Local::now().to_rfc3339();

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let combined = format!("{}\n{}", stdout, stderr);

        let status = if output.status.success() {
            CheckStatus::Passed
        } else {
            // rustfmt --check exits non-zero if files need formatting
            CheckStatus::Warnings
        };

        Ok(CheckResult {
            name: self.name().to_string(),
            status,
            duration: start.elapsed(),
            output: combined.clone(),
            cached: false,
            provenance: Some(
                ProvenanceBuilder {
                    cmd: "cargo",
                    args,
                    cwd,
                    output: &output,
                    combined_output: &combined,
                    started_at: &started_at,
                    finished_at: &finished_at,
                    cache_key: self.cache_key(config),
                }
                .build_with_repo_root(Some(&config.repo_root)),
            ),
        })
    }
}

#[async_trait]
impl Check for CargoAuditCheck {
    fn name(&self) -> &str {
        "Cargo audit"
    }

    fn check_eligibility(&self, config: &Config) -> super::CheckEligibility {
        if !config.profile.has_cargo {
            return super::CheckEligibility::Skip(format!(
                "profile {}",
                config.profile.kind.as_str().to_lowercase()
            ));
        }
        if !config.run_security && !config.run_lint {
            return super::CheckEligibility::Skip("security disabled".to_string());
        }
        if which::which("cargo-audit").is_err() {
            return super::CheckEligibility::Skip(
                "tool not installed (cargo-audit is missing)".to_string(),
            );
        }
        super::CheckEligibility::Run
    }

    fn cache_key(&self, config: &Config) -> Option<String> {
        // Advisories change over time even when the code does not, so the audit
        // result must not be cached indefinitely — a freshly published RUSTSEC
        // advisory has to reach the gate. Key on the dependency manifest plus the
        // current day: repeated runs on the same day stay cached, but a new day
        // (or a Cargo.lock change) re-runs the audit. Source churn is irrelevant
        // to the advisory set, so it is deliberately excluded.
        let day = Local::now().format("%Y-%m-%d");
        // Hash the lock at the SAME directory the audit runs in — cargo_root,
        // which may be a workspace member with its own Cargo.lock — not the repo
        // root. Keying on the root lock while executing in a member meant a
        // member Cargo.lock change never invalidated the cache and a stale audit
        // was served (PR #12 review #22).
        let cargo_root = config
            .profile
            .cargo_root
            .as_ref()
            .unwrap_or(&config.repo_root);
        Some(format!(
            "audit-{}-{}",
            cache::cargo_lock_hash(cargo_root),
            day
        ))
    }

    async fn run(&self, config: &Config) -> Result<CheckResult> {
        let start = std::time::Instant::now();
        let started_at = Local::now().to_rfc3339();

        let cwd = config
            .profile
            .cargo_root
            .as_ref()
            .unwrap_or(&config.repo_root);

        let args = &["audit", "--json"];
        let output = run_command("cargo", args, cwd).await?;
        let finished_at = Local::now().to_rfc3339();

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let combined = format!("{}\n{}", stdout, stderr);
        let status = classify_cargo_audit_status(output.status.success(), &stdout, &combined);

        Ok(CheckResult {
            name: self.name().to_string(),
            status,
            duration: start.elapsed(),
            output: combined.clone(),
            cached: false,
            provenance: Some(
                ProvenanceBuilder {
                    cmd: "cargo",
                    args,
                    cwd,
                    output: &output,
                    combined_output: &combined,
                    started_at: &started_at,
                    finished_at: &finished_at,
                    cache_key: self.cache_key(config),
                }
                .build_with_repo_root(Some(&config.repo_root)),
            ),
        })
    }
}

fn classify_cargo_audit_status(
    command_succeeded: bool,
    stdout: &str,
    combined: &str,
) -> CheckStatus {
    if let Some(vulnerability_count) = cargo_audit_vulnerability_count(stdout) {
        if vulnerability_count > 0 {
            return CheckStatus::Failed;
        }

        if command_succeeded {
            return CheckStatus::Passed;
        }

        if cargo_audit_has_warnings(combined) {
            return CheckStatus::Warnings;
        }

        return CheckStatus::Failed;
    }

    if command_succeeded {
        return CheckStatus::Passed;
    }

    if cargo_audit_has_warnings(combined) {
        return CheckStatus::Warnings;
    }

    if combined.contains("RUSTSEC-") {
        return CheckStatus::Failed;
    }

    CheckStatus::Failed
}

fn cargo_audit_vulnerability_count(stdout: &str) -> Option<usize> {
    let parsed = serde_json::from_str::<serde_json::Value>(stdout).ok()?;
    let vulnerabilities = parsed.get("vulnerabilities")?;

    if let Some(count) = vulnerabilities
        .get("count")
        .and_then(|value| value.as_u64())
    {
        return Some(count as usize);
    }

    if let Some(list) = vulnerabilities
        .get("list")
        .and_then(|value| value.as_array())
    {
        return Some(list.len());
    }

    Some(0)
}

fn cargo_audit_has_warnings(output: &str) -> bool {
    output.to_ascii_lowercase().contains("warning")
}

#[async_trait]
impl Check for CargoGeigerCheck {
    fn name(&self) -> &str {
        "Cargo geiger"
    }

    fn check_eligibility(&self, config: &Config) -> super::CheckEligibility {
        if !config.profile.has_cargo {
            return super::CheckEligibility::Skip(format!(
                "profile {}",
                config.profile.kind.as_str().to_lowercase()
            ));
        }
        if !config.security_full {
            return super::CheckEligibility::Skip("requires --security-full".to_string());
        }
        if which::which("cargo-geiger").is_err() {
            return super::CheckEligibility::Skip(
                "tool not installed (cargo-geiger is missing)".to_string(),
            );
        }
        super::CheckEligibility::Run
    }

    fn cache_key(&self, config: &Config) -> Option<String> {
        Some(format!("geiger-{}", cache::rust_hash(&config.repo_root)))
    }

    async fn run(&self, config: &Config) -> Result<CheckResult> {
        let start = std::time::Instant::now();
        let started_at = Local::now().to_rfc3339();

        let cwd = config
            .profile
            .cargo_root
            .as_ref()
            .unwrap_or(&config.repo_root);

        if cargo_metadata_is_virtual_manifest(cwd).await {
            return Ok(CheckResult {
                name: self.name().to_string(),
                status: CheckStatus::Skipped,
                duration: start.elapsed(),
                output: "Cargo geiger skipped: cargo metadata reports a virtual workspace manifest; cargo-geiger requires a concrete package. Configure package selection or run geiger per workspace member.".to_string(),
                cached: false,
                provenance: None,
            });
        }

        let args = &["geiger", "--output-format", "Ratio"];
        let output = match run_command_with_timeout("cargo", args, cwd, 600).await {
            Ok(output) => output,
            Err(err) if super::is_timeout_error(&err) => {
                // `cargo geiger` can take many minutes on large dependency trees
                // and is a non-blocking advisory signal. A timeout is a tooling
                // limitation, not a quality failure — degrade to Skipped instead
                // of a hard Error so it does not pollute the merge gate.
                return Ok(CheckResult {
                    name: self.name().to_string(),
                    status: CheckStatus::Skipped,
                    duration: start.elapsed(),
                    output: format!("cargo geiger skipped: {err}"),
                    cached: false,
                    provenance: None,
                });
            }
            Err(err) => return Err(err),
        };
        let finished_at = Local::now().to_rfc3339();

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let combined = format!("{}\n{}", stdout, stderr);

        let status = classify_cargo_geiger_status(output.status.success(), &combined);

        Ok(CheckResult {
            name: self.name().to_string(),
            status,
            duration: start.elapsed(),
            output: combined.clone(),
            cached: false,
            provenance: Some(
                ProvenanceBuilder {
                    cmd: "cargo",
                    args,
                    cwd,
                    output: &output,
                    combined_output: &combined,
                    started_at: &started_at,
                    finished_at: &finished_at,
                    cache_key: self.cache_key(config),
                }
                .build_with_repo_root(Some(&config.repo_root)),
            ),
        })
    }
}

fn classify_cargo_geiger_status(command_succeeded: bool, output: &str) -> CheckStatus {
    if output.contains("is a virtual manifest")
        && output.contains("requires running against an actual package")
    {
        return CheckStatus::Skipped;
    }

    if has_tool_crash(output) {
        return CheckStatus::Error;
    }

    // A virtual workspace manifest (the root `Cargo.toml` of a `[workspace]`
    // with no `[package]`) cannot be scanned by `cargo geiger` directly — cargo
    // refuses with "requires running against an actual package". That is a
    // workspace-shape limitation, not an unsafe-code signal or a tool crash, so
    // degrade to a clean Skipped status instead of a permanent gate error.
    if is_virtual_manifest_error(output) {
        return CheckStatus::Skipped;
    }

    let dependency_scan_warnings = output
        .lines()
        .any(|line| line.starts_with("WARNING: Dependency file was never scanned:"));
    let warning_summary = output.lines().any(|line| {
        line.starts_with("error: Found ")
            && line
                .split_whitespace()
                .last()
                .is_some_and(|token| token == "warnings")
    });

    if !command_succeeded {
        if dependency_scan_warnings && warning_summary {
            return CheckStatus::Warnings;
        }
        return CheckStatus::Error;
    }

    // Command succeeded: read the actual `used/total=pct%` ratio table.
    //
    // The old `contains("0/0") || !contains("unsafe")` heuristic was structurally
    // blind: geiger's legend ALWAYS contains the word "unsafe" (dead second
    // branch), and a `0/0=100.00%` cell appears in the Impls/Traits/Methods
    // columns of nearly every crate — including crates that DO use unsafe — so
    // the first branch painted real unsafe green. Classify from the numbers.
    match geiger_unsafe_found(output) {
        Some(true) => CheckStatus::Warnings,
        Some(false) => CheckStatus::Passed,
        // Exit 0 but no ratio table parsed — an unexpected output shape we must
        // not report as a clean Passed (fail-open on a security signal).
        None => CheckStatus::Warnings,
    }
}

/// Read `cargo geiger --output-format Ratio` output for unsafe usage.
///
/// Each ratio cell is `used/total=pct%` where `used` counts SAFE code out of
/// `total`; a cell with `total > used` means unsafe items were found. Legend
/// lines are prose (their literal `x/y=z%` is non-numeric) and are skipped.
///
/// Returns `Some(true)` if any cell reports unsafe, `Some(false)` if a table was
/// found and every cell is clean, and `None` if no ratio cell was parsed at all.
fn geiger_unsafe_found(output: &str) -> Option<bool> {
    let mut saw_row = false;
    let mut unsafe_found = false;
    for token in output.split_whitespace() {
        let Some((ratio, _pct)) = token.split_once('=') else {
            continue;
        };
        let Some((safe, total)) = ratio.split_once('/') else {
            continue;
        };
        let (Ok(safe), Ok(total)) = (safe.parse::<u64>(), total.parse::<u64>()) else {
            continue;
        };
        saw_row = true;
        if total > safe {
            unsafe_found = true;
        }
    }
    saw_row.then_some(unsafe_found)
}

/// Detect cargo's "virtual manifest" refusal, emitted when `cargo geiger` is
/// run at the root of a `[workspace]` whose manifest declares no package.
fn is_virtual_manifest_error(output: &str) -> bool {
    output.contains("virtual manifest")
        && output.contains("requires running against an actual package")
}

async fn cargo_metadata_is_virtual_manifest(cwd: &Path) -> bool {
    // Async with a hard timeout: a synchronous `cargo metadata` here blocked
    // the whole FuturesUnordered check pool whenever cargo sat on a file lock
    // (e.g. a parallel build in the same repo) — the in-process cousin of the
    // npx hang class.
    let Ok(output) = crate::checks::run_command_with_timeout(
        "cargo",
        &["metadata", "--no-deps", "--format-version", "1"],
        cwd,
        60,
    )
    .await
    else {
        return false;
    };

    if !output.status.success() {
        return false;
    }

    let Ok(metadata) = serde_json::from_slice::<serde_json::Value>(&output.stdout) else {
        return false;
    };

    metadata.get("root_package").is_some_and(|v| v.is_null())
        && metadata
            .get("workspace_members")
            .and_then(|v| v.as_array())
            .is_some_and(|members| !members.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::ExecutionMode;
    use crate::config::{test_config_builder, test_rust_profile};

    fn create_test_config(has_cargo: bool, run_lint: bool, run_tests: bool) -> Config {
        test_config_builder()
            .profile(test_rust_profile(has_cargo))
            .execution_mode(ExecutionMode::Standard)
            .run_lint(run_lint)
            .run_tests(run_tests)
            .do_fetch(false)
            .use_cache(false)
            .create_zip(false)
            .build()
    }

    #[test]
    fn test_cargo_check_name() {
        let check = CargoCheck;
        assert_eq!(check.name(), "Cargo check");
    }

    #[test]
    fn test_clippy_check_name() {
        let check = ClippyCheck;
        assert_eq!(check.name(), "Clippy");
    }

    #[test]
    fn test_cargo_check_can_run_with_cargo() {
        let config = create_test_config(true, false, false);
        let check = CargoCheck;
        assert_eq!(
            check.check_eligibility(&config),
            super::super::CheckEligibility::Run
        );
    }

    #[test]
    fn test_cargo_check_cannot_run_without_cargo() {
        let config = create_test_config(false, false, false);
        let check = CargoCheck;
        assert!(matches!(
            check.check_eligibility(&config),
            super::super::CheckEligibility::Skip(_)
        ));
    }

    #[test]
    fn test_clippy_check_can_run() {
        let config = create_test_config(true, true, false);
        let check = ClippyCheck;
        assert_eq!(
            check.check_eligibility(&config),
            super::super::CheckEligibility::Run
        );
    }

    #[test]
    fn test_clippy_check_cannot_run_without_lint() {
        let config = create_test_config(true, false, false);
        let check = ClippyCheck;
        assert!(matches!(
            check.check_eligibility(&config),
            super::super::CheckEligibility::Skip(_)
        ));
    }

    #[test]
    fn test_clippy_check_cannot_run_without_cargo() {
        let config = create_test_config(false, true, false);
        let check = ClippyCheck;
        assert!(matches!(
            check.check_eligibility(&config),
            super::super::CheckEligibility::Skip(_)
        ));
    }

    #[test]
    fn test_clippy_check_skips_fast_remote_only_by_default() {
        let mut config = create_test_config(true, true, false);
        config.remote_only = true;
        let check = ClippyCheck;
        assert!(matches!(
            check.check_eligibility(&config),
            super::super::CheckEligibility::Skip(_)
        ));
    }

    #[test]
    fn test_clippy_check_can_run_fast_remote_only_when_forced() {
        let mut config = create_test_config(true, true, false);
        config.remote_only = true;
        config.lint_forced = true;
        let check = ClippyCheck;
        assert_eq!(
            check.check_eligibility(&config),
            super::super::CheckEligibility::Run
        );
    }

    #[test]
    fn test_cargo_test_check_can_run() {
        let config = create_test_config(true, false, true);
        let check = CargoTestCheck;
        assert_eq!(
            check.check_eligibility(&config),
            super::super::CheckEligibility::Run
        );
    }

    #[test]
    fn test_cargo_test_check_cannot_run_without_tests() {
        let config = create_test_config(true, false, false);
        let check = CargoTestCheck;
        assert!(matches!(
            check.check_eligibility(&config),
            super::super::CheckEligibility::Skip(_)
        ));
    }

    #[test]
    fn test_cargo_geiger_requires_security_full() {
        // Without --security-full geiger is not eligible; it is opt-in and must
        // stay out of the default profile rather than fabricate a caveat.
        let config = create_test_config(true, true, false);
        assert!(!config.security_full);
        let check = CargoGeigerCheck;
        assert!(matches!(
            check.check_eligibility(&config),
            super::super::CheckEligibility::Skip(_)
        ));
    }

    #[test]
    fn test_cargo_check_cache_key() {
        let config = create_test_config(true, false, false);
        let check = CargoCheck;
        let key = check.cache_key(&config);
        assert!(key.is_some());
    }

    #[test]
    fn test_clippy_check_cache_key() {
        let config = create_test_config(true, true, false);
        let check = ClippyCheck;
        let key = check.cache_key(&config);
        assert!(key.is_some());
        assert!(key.unwrap().starts_with("clippy-"));
    }

    #[test]
    fn test_cargo_audit_cache_key_is_day_scoped() {
        // The audit key must carry the current day so a freshly published
        // advisory invalidates a cached "passed" within a day, rather than being
        // pinned forever to an unchanged Cargo.lock.
        let config = create_test_config(true, true, false);
        let check = CargoAuditCheck;
        let key = check.cache_key(&config).expect("audit cache key");
        assert!(key.starts_with("audit-"), "unexpected key: {key}");
        let today = Local::now().format("%Y-%m-%d").to_string();
        assert!(
            key.ends_with(&today),
            "audit key must be scoped to the current day ({today}), got: {key}"
        );
    }

    #[test]
    fn test_cargo_audit_cache_key_follows_cargo_root_not_repo_root() {
        // PR #12 review #22: the audit runs in cargo_root (which may be a
        // workspace member), so the cache key must hash THAT directory's
        // Cargo.lock. Keying on the repo root while executing in a member let a
        // member lock change go unnoticed and served a stale audit. Two configs
        // that differ ONLY in cargo_root (root vs member, with different locks)
        // must therefore produce different keys.
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        let member = root.join("member");
        std::fs::create_dir_all(&member).unwrap();
        std::fs::write(root.join("Cargo.toml"), "[workspace]\n").unwrap();
        std::fs::write(root.join("Cargo.lock"), "# root lock\n").unwrap();
        std::fs::write(member.join("Cargo.toml"), "[package]\nname = \"m\"\n").unwrap();
        std::fs::write(member.join("Cargo.lock"), "# member lock DIFFERENT\n").unwrap();

        let mut root_profile = test_rust_profile(true);
        root_profile.cargo_root = Some(root.to_path_buf());
        let config_root = test_config_builder()
            .repo_root(root)
            .profile(root_profile)
            .build();

        let mut member_profile = test_rust_profile(true);
        member_profile.cargo_root = Some(member.clone());
        let config_member = test_config_builder()
            .repo_root(root)
            .profile(member_profile)
            .build();

        let check = CargoAuditCheck;
        let key_root = check.cache_key(&config_root).expect("root key");
        let key_member = check.cache_key(&config_member).expect("member key");
        assert_ne!(
            key_root, key_member,
            "audit key must follow cargo_root, not the shared repo root"
        );
    }

    #[test]
    fn test_cargo_audit_vulnerabilities_are_failed() {
        let stdout = r#"{
  "vulnerabilities": {
    "found": true,
    "count": 2,
    "list": [
      {"advisory": {"id": "RUSTSEC-2023-0001"}},
      {"advisory": {"id": "RUSTSEC-2023-0002"}}
    ]
  }
}"#;

        let status = classify_cargo_audit_status(false, stdout, stdout);
        assert_eq!(status, CheckStatus::Failed);
    }

    #[test]
    fn test_cargo_audit_clean_report_is_passed() {
        let stdout = r#"{
  "vulnerabilities": {
    "found": false,
    "count": 0,
    "list": []
  }
}"#;

        let status = classify_cargo_audit_status(true, stdout, stdout);
        assert_eq!(status, CheckStatus::Passed);
    }

    #[test]
    fn test_cargo_audit_warning_only_is_warnings() {
        let stdout = r#"{
  "vulnerabilities": {
    "found": false,
    "count": 0,
    "list": []
  }
}"#;
        let stderr = "warning: advisory database is stale";
        let combined = format!("{}\n{}", stdout, stderr);

        let status = classify_cargo_audit_status(false, stdout, &combined);
        assert_eq!(status, CheckStatus::Warnings);
    }

    #[test]
    fn test_cargo_audit_non_json_failure_is_failed() {
        let combined = "error: failed to fetch advisory db";
        let status = classify_cargo_audit_status(false, "not-json", combined);
        assert_eq!(status, CheckStatus::Failed);
    }

    #[test]
    fn test_cargo_geiger_warning_flood_is_non_blocking_warning() {
        let output = "\
WARNING: Dependency file was never scanned: /tmp/dep.rs
WARNING: Dependency file was never scanned: /tmp/dep2.rs
error: Found 2 warnings
";

        let status = classify_cargo_geiger_status(false, output);
        assert_eq!(status, CheckStatus::Warnings);
    }

    #[test]
    fn test_cargo_geiger_real_failures_stay_errors() {
        let output = "error: cargo-geiger panicked";
        let status = classify_cargo_geiger_status(false, output);
        assert_eq!(status, CheckStatus::Error);
    }

    #[test]
    fn test_cargo_geiger_virtual_manifest_degrades_to_skipped() {
        let output = "manifest path `/repo/Cargo.toml` is a virtual manifest, \
            but this command requires running against an actual package in this workspace";
        let status = classify_cargo_geiger_status(false, output);
        assert_eq!(status, CheckStatus::Skipped);
    }

    // Real `cargo geiger 0.13.0 --output-format Ratio` output for a crate that
    // uses unsafe (`!` marker, one Expressions cell at 1/2). Note it contains
    // BOTH "0/0" and the word "unsafe" (legend) — the exact input the old
    // `contains("0/0") || !contains("unsafe")` heuristic mis-classified as
    // Passed. The blind security signal must now surface as Warnings.
    const GEIGER_RATIO_WITH_UNSAFE: &str = "\
Metric output format: x/y=z%
    x = safe code found in the crate
    y = total code found in the crate
    z = percentage of safe ratio as defined by x/y

Symbols:
    :) = No `unsafe` usage found, declares #![forbid(unsafe_code)]
    ?  = No `unsafe` usage found, missing #![forbid(unsafe_code)]
    !  = `unsafe` usage found

Functions  Expressions  Impls  Traits  Methods  Dependency

    2/2=100.00%     1/2=50.00%         0/0=100.00%        0/0=100.00%     0/0=100.00%  !  geigertest 0.1.0

    2/2=100.00%     1/2=50.00%         0/0=100.00%        0/0=100.00%     0/0=100.00%
";

    // Same shape, but every category is fully safe (`?` marker, all n/n). Still
    // contains "0/0" and the legend word "unsafe" — must classify as Passed
    // without the legend tripping a false Warning.
    const GEIGER_RATIO_ALL_SAFE: &str = "\
Symbols:
    :) = No `unsafe` usage found, declares #![forbid(unsafe_code)]
    ?  = No `unsafe` usage found, missing #![forbid(unsafe_code)]
    !  = `unsafe` usage found

Functions  Expressions  Impls  Traits  Methods  Dependency

    5/5=100.00%     3/3=100.00%     0/0=100.00%     0/0=100.00%     1/1=100.00%  ?  safecrate 0.1.0

    5/5=100.00%     3/3=100.00%     0/0=100.00%     0/0=100.00%     1/1=100.00%
";

    #[test]
    fn cargo_geiger_unsafe_ratio_is_warnings_not_passed() {
        // Regression: blind != healthy. A crate that uses unsafe must never be
        // painted green just because "0/0" and "unsafe" appear in the output.
        let status = classify_cargo_geiger_status(true, GEIGER_RATIO_WITH_UNSAFE);
        assert_eq!(status, CheckStatus::Warnings);
    }

    #[test]
    fn cargo_geiger_all_safe_ratio_is_passed() {
        let status = classify_cargo_geiger_status(true, GEIGER_RATIO_ALL_SAFE);
        assert_eq!(status, CheckStatus::Passed);
    }

    #[test]
    fn cargo_geiger_success_without_ratio_table_is_not_green() {
        // Exit 0 but no ratio table at all — an unexpected shape that the old
        // heuristic would have painted Passed (no "unsafe", no "0/0"). Honesty
        // gate: do not fake a clean security result.
        let status = classify_cargo_geiger_status(true, "some unexpected geiger output\n");
        assert_eq!(status, CheckStatus::Warnings);
    }

    #[test]
    fn geiger_unsafe_found_reads_ratio_cells() {
        assert_eq!(geiger_unsafe_found(GEIGER_RATIO_WITH_UNSAFE), Some(true));
        assert_eq!(geiger_unsafe_found(GEIGER_RATIO_ALL_SAFE), Some(false));
        assert_eq!(geiger_unsafe_found("no ratios here"), None);
        // 0/0 must not count as unsafe (no code in that category).
        assert_eq!(geiger_unsafe_found("0/0=100.00%"), Some(false));
    }

    #[test]
    fn clippy_build_script_warnings_do_not_trip_warn_status() {
        // Real-world clippy log from a clean run where a dependency's build.rs
        // prints `cargo:warning=` lines. Clippy itself is clean (exit 0, no
        // lints), so the check must report Passed, not Warnings.
        let combined = "\n\
warning: codescribe-core@0.12.2: Embedding MiniLM model from: /Users/x/models\n\
warning: codescribe-core@0.12.2: Embedded models for codescribe: Whisper=runtime_load_from_cache\n   \
Compiling codescribe v0.12.2 (/repo)\n    \
Finished `dev` profile [unoptimized + debuginfo] target(s) in 3.46s\n";
        assert!(!clippy_has_real_warnings(combined));
    }

    #[test]
    fn clippy_real_lint_warning_trips_warn_status() {
        let combined =
            "src/main.rs:10:5: warning: unused variable `x`\nwarning: 1 warning emitted\n";
        assert!(clippy_has_real_warnings(combined));
    }

    #[test]
    fn clippy_mixed_build_script_and_real_warning_is_real() {
        let combined = "\
warning: dep-crate@1.2.3: building native lib\n\
src/lib.rs:3:1: warning: function `foo` is never used\n";
        assert!(clippy_has_real_warnings(combined));
    }

    #[test]
    fn is_cargo_build_script_warning_detection() {
        assert!(is_cargo_build_script_warning(
            "warning: codescribe-core@0.12.2: Embedding MiniLM model from: /x"
        ));
        assert!(is_cargo_build_script_warning(
            "warning: some-crate@1.0.0-beta.1: doing native build work"
        ));
        // Real clippy lints are never build-script warnings.
        assert!(!is_cargo_build_script_warning(
            "src/main.rs:10:5: warning: unused variable `x`"
        ));
        assert!(!is_cargo_build_script_warning(
            "warning: unused import: `std::io`"
        ));
        assert!(!is_cargo_build_script_warning(
            "warning: 2 warnings emitted"
        ));
    }

    #[test]
    fn test_is_virtual_manifest_error_detection() {
        assert!(is_virtual_manifest_error(
            "Cargo.toml is a virtual manifest, but this command requires running against an actual package",
        ));
        assert!(!is_virtual_manifest_error("error: Found 3 warnings"));
        assert!(!is_virtual_manifest_error("a virtual manifest"));
    }
}
