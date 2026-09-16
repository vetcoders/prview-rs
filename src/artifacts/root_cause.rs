//! Root-cause extraction from failed check outputs (per-tool parsers).

use super::*;

// ── Root-cause extraction ──────────────────────────────────────────

pub(crate) struct RootCause {
    pub(crate) cause: String,
    pub(crate) evidence: String,
    pub(crate) hint: String,
}

pub(crate) fn extract_root_cause(check: &CheckResult) -> Option<RootCause> {
    let output = &check.output;
    let name_lower = check.name.to_lowercase();

    if name_lower.contains("cargo test")
        && let Some(root_cause) = extract_cargo_test_root_cause(output)
    {
        return Some(root_cause);
    }

    // Pytest progress and parameterized test names can contain words such as
    // "failed", "error", or "timed out" even when that test passed. Its
    // structured diagnostics must take precedence over generic signatures.
    if name_lower.contains("pytest") {
        return extract_pytest_root_cause(check);
    }

    // Timeout detection (universal)
    if let Some(ref prov) = check.provenance
        && (prov.exit_code == Some(-1)
            || output.contains("killed (>")
            || output.contains("timed out"))
    {
        return Some(RootCause {
            cause: "Process timed out".into(),
            evidence: format!("Exit code: {:?}", prov.exit_code),
            hint: "Consider increasing timeout or investigating infinite loops".into(),
        });
    }

    // Hard-fail signature detection (universal)
    if let Some(ref prov) = check.provenance
        && !prov.hard_fail_signatures.is_empty()
    {
        return Some(RootCause {
            cause: format!("Hard failure detected: {}", prov.hard_fail_signatures[0]),
            evidence: prov.hard_fail_signatures.join(", "),
            hint: "This indicates a crash or unhandled exception, not a normal check failure"
                .into(),
        });
    }

    // Cargo check / Clippy: parse error[EXXXX] + --> file:line
    if name_lower.contains("cargo")
        && !name_lower.contains("test")
        && !name_lower.contains("audit")
        && !name_lower.contains("geiger")
        || name_lower.contains("clippy")
    {
        return extract_rust_compiler_root_cause(output);
    }

    // Cargo audit: parse vulnerability info
    if name_lower.contains("audit") {
        return extract_cargo_audit_root_cause(output);
    }

    // Cargo geiger: typically timeout
    if name_lower.contains("geiger") {
        return Some(RootCause {
            cause: "Cargo geiger analysis issue".into(),
            evidence: output
                .lines()
                .find(|l| !l.trim().is_empty())
                .unwrap_or("(empty output)")
                .to_string(),
            hint: "Geiger scans can be slow on large dependency trees".into(),
        });
    }

    // TypeScript: parse TS\d{4}
    if name_lower.contains("typescript") || name_lower == "tsc" {
        return extract_typescript_root_cause(output);
    }

    // ESLint: parse "X problems"
    if name_lower.contains("eslint") {
        return extract_eslint_root_cause(output);
    }

    // Stylelint
    if name_lower.contains("stylelint") {
        let violation_count = output
            .lines()
            .filter(|l| l.contains("✖") || l.contains("error") || l.contains("warning"))
            .count();
        return Some(RootCause {
            cause: format!("{} style violation(s)", violation_count),
            evidence: output
                .lines()
                .find(|l| l.contains("✖") || l.contains("error"))
                .unwrap_or("")
                .to_string(),
            hint: "Run stylelint --fix to auto-fix what's possible".into(),
        });
    }

    // Vitest / tests (JS)
    if name_lower.contains("vitest")
        || (name_lower.contains("test")
            && !name_lower.contains("cargo")
            && !name_lower.contains("pytest"))
    {
        return extract_vitest_root_cause(output);
    }

    // Rustfmt
    if name_lower.contains("rustfmt") || name_lower.contains("fmt") {
        let diff_files: Vec<&str> = output
            .lines()
            .filter(|l| l.starts_with("Diff in ") || l.starts_with("--- ") || l.contains(".rs"))
            .take(3)
            .collect();
        return Some(RootCause {
            cause: "Formatting differences detected".into(),
            evidence: if diff_files.is_empty() {
                output
                    .lines()
                    .find(|l| !l.trim().is_empty())
                    .unwrap_or("")
                    .to_string()
            } else {
                diff_files.join("; ")
            },
            hint: "Run cargo fmt to auto-fix".into(),
        });
    }

    // Ruff (Python)
    if name_lower.contains("ruff") {
        let error_count = output
            .lines()
            .filter(|l| l.contains(" error") || l.contains(" E"))
            .count();
        let first_error = output
            .lines()
            .find(|l| l.contains(":") && (l.contains(" E") || l.contains(" error")));
        return Some(RootCause {
            cause: format!("{} linting issue(s)", error_count.max(1)),
            evidence: first_error.unwrap_or("").to_string(),
            hint: "Run ruff check --fix to auto-fix what's possible".into(),
        });
    }

    // Mypy (Python)
    if name_lower.contains("mypy") {
        // Missing tool: the failure is "could not launch mypy", not type errors.
        // Use the raw-output detector (narrow), not the runner-string one, so a
        // real type error mentioning "no such file or directory" is not mislabelled.
        if crate::checks::tool_spawn_failure_in_output(output) {
            let evidence = output
                .lines()
                .find(|l| {
                    let lower = l.to_ascii_lowercase();
                    lower.contains("failed to spawn") || lower.contains("no such file or directory")
                })
                .or_else(|| output.lines().find(|l| !l.trim().is_empty()))
                .unwrap_or("")
                .to_string();
            return Some(RootCause {
                cause: "mypy not installed / could not be launched".into(),
                evidence,
                hint: "Install mypy (e.g. `uv add --dev mypy` or `pip install mypy`) or scope it out of this run".into(),
            });
        }
        let error_line = output.lines().find(|l| l.contains(": error:"));
        let summary = output.lines().find(|l| l.starts_with("Found "));
        return Some(RootCause {
            cause: summary.unwrap_or("Type checking errors").to_string(),
            evidence: error_line.unwrap_or("").to_string(),
            hint: "Fix type annotations or add type: ignore comments".into(),
        });
    }

    // Fallback: first non-empty line
    let first_line = output
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("No output");
    Some(RootCause {
        cause: "Check failed".into(),
        evidence: first_line.to_string(),
        hint: format!(
            "See full log: 20_quality/{}.log",
            check_id_from_name(&check.name)
        ),
    })
}

pub(crate) fn extract_rust_compiler_root_cause(output: &str) -> Option<RootCause> {
    // Find first error[EXXXX]: message
    let error_line = output
        .lines()
        .find(|l| l.contains("error[E") || (l.starts_with("error") && l.contains("aborting")));
    // Find first --> file:line:col
    let location = output.lines().find(|l| l.trim_start().starts_with("-->"));

    let cause = error_line.unwrap_or("Compilation error");
    let evidence = location.map(|l| l.trim().to_string()).unwrap_or_default();

    Some(RootCause {
        cause: cause.to_string(),
        evidence,
        hint: "Fix the compilation error(s) listed above".into(),
    })
}

pub(crate) fn extract_cargo_test_root_cause(output: &str) -> Option<RootCause> {
    let failed_tests = parsers::cargo_test::extract_failed_test_names(output);

    // Find the summary line with FAILED (not the passing "test result: ok" lines)
    let failed_summary = output
        .lines()
        .find(|l| l.starts_with("test result:") && l.contains("FAILED"));
    // Fallback: last "test result:" line if none has FAILED
    let last_summary = output.lines().fold(None, |acc, l| {
        if l.starts_with("test result:") {
            Some(l)
        } else {
            acc
        }
    });

    if failed_tests.is_empty() {
        let error_line = output.lines().find(|line| {
            let trimmed = line.trim();
            trimmed.starts_with("error:")
                || trimmed.starts_with("error[")
                || trimmed.contains(": error:")
        });

        return error_line.map(|line| RootCause {
            cause: "cargo test failed before named test failures were reported".to_string(),
            evidence: line.trim().to_string(),
            hint: failed_summary
                .or(last_summary)
                .unwrap_or("Inspect cargo_test.log for compiler or doctest output")
                .to_string(),
        });
    }

    let cause = if failed_tests.len() == 1 {
        format!("1 test failed: {}", failed_tests[0])
    } else {
        format!("{} tests failed", failed_tests.len().max(1))
    };

    let hint = failed_summary
        .or(last_summary)
        .unwrap_or("Run cargo test to reproduce")
        .to_string();

    Some(RootCause {
        cause,
        evidence: if failed_tests.len() <= 3 {
            failed_tests.join(", ")
        } else {
            format!(
                "{}, ... and {} more",
                failed_tests[..3].join(", "),
                failed_tests.len() - 3
            )
        },
        hint,
    })
}

pub(crate) fn extract_cargo_audit_root_cause(output: &str) -> Option<RootCause> {
    let findings = parse_cargo_audit_findings(output);
    if !findings.is_empty() {
        return Some(RootCause {
            cause: cargo_audit_summary_cause(&findings),
            evidence: cargo_audit_advisory_ids(&findings, 3),
            hint: "Update the affected crates and refresh `Cargo.lock` (for example with `cargo update -p <crate>` or a dependency bump)".into(),
        });
    }

    let vuln_line = output
        .lines()
        .find(|l| l.contains("vulnerabilit") || l.contains("RUSTSEC"));
    let advisory = output.lines().find(|l| l.contains("RUSTSEC-"));

    Some(RootCause {
        cause: vuln_line
            .unwrap_or("Security vulnerabilities found")
            .to_string(),
        evidence: advisory.unwrap_or("").to_string(),
        hint: "Run cargo audit fix or update affected dependencies".into(),
    })
}

pub(crate) fn extract_typescript_root_cause(output: &str) -> Option<RootCause> {
    // Find TS error: file(line,col): error TSXXXX: message
    let ts_error = output
        .lines()
        .find(|l| l.contains("error TS") || l.contains(": error TS"));
    let error_count = output.lines().filter(|l| l.contains("error TS")).count();

    Some(RootCause {
        cause: format!("{} TypeScript error(s)", error_count.max(1)),
        evidence: ts_error.unwrap_or("").to_string(),
        hint: "Fix type errors or update tsconfig.json".into(),
    })
}

pub(crate) fn extract_eslint_root_cause(output: &str) -> Option<RootCause> {
    // Look for "X problems (Y errors, Z warnings)"
    let problems_line = output.lines().find(|l| l.contains(" problem"));
    let first_error = output
        .lines()
        .find(|l| l.contains("error") && l.contains(":") && !l.contains("problem"));

    Some(RootCause {
        cause: problems_line.unwrap_or("ESLint errors").to_string(),
        evidence: first_error.unwrap_or("").to_string(),
        hint: "Run eslint --fix to auto-fix what's possible".into(),
    })
}

pub(crate) fn extract_vitest_root_cause(output: &str) -> Option<RootCause> {
    let mut failed_tests = Vec::new();

    for line in output.lines() {
        let trimmed = line.trim();
        // Vitest uses FAIL or × for failed tests
        if trimmed.starts_with("FAIL") || trimmed.starts_with("×") || trimmed.contains("✗") {
            failed_tests.push(trimmed.to_string());
        }
    }

    let summary = output
        .lines()
        .find(|l| l.contains("Tests") && (l.contains("failed") || l.contains("passed")));

    let cause = if !failed_tests.is_empty() {
        format!("{} test(s) failed", failed_tests.len())
    } else {
        "Test suite failed".into()
    };

    Some(RootCause {
        cause,
        evidence: if failed_tests.len() <= 3 {
            failed_tests.join("; ")
        } else {
            format!(
                "{}, ... and {} more",
                failed_tests[..3].join("; "),
                failed_tests.len() - 3
            )
        },
        hint: summary.unwrap_or("Run test suite to reproduce").to_string(),
    })
}

pub(crate) fn extract_pytest_root_cause(check: &CheckResult) -> Option<RootCause> {
    use regex::Regex;
    use std::sync::LazyLock;

    if !matches!(check.status, CheckStatus::Failed | CheckStatus::Error) {
        return None;
    }

    static RUNNER_TIMEOUT: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"^(?:pytest|uv) timed out after [0-9]+s$").expect("pytest runner timeout regex")
    });
    if check.status == CheckStatus::Error && RUNNER_TIMEOUT.is_match(check.output.trim()) {
        return Some(RootCause {
            cause: "Process timed out".into(),
            evidence: check.output.trim().to_string(),
            hint: "Inspect the timeout budget and full Pytest log.".into(),
        });
    }

    if check.provenance.as_ref().and_then(|p| p.exit_code) == Some(-1) {
        return Some(RootCause {
            cause: "Process timed out".into(),
            evidence: "PrView recorded a runner timeout (exit code -1).".into(),
            hint: "Inspect the timeout budget and full Pytest log.".into(),
        });
    }

    static FAILURE_COUNT: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?:^|,\s*)[1-9][0-9]* (?:failed|errors?)\b")
            .expect("pytest failure summary regex")
    });
    let output = &check.output;
    let excerpt = findings::pytest_failure_excerpt(output);
    let summary = output
        .lines()
        .rev()
        .map(str::trim)
        .find(|line| {
            line.starts_with('=')
                && line.ends_with('=')
                && FAILURE_COUNT.is_match(line.trim_matches('=').trim())
        })
        .map(|line| line.trim_matches('=').trim());

    if excerpt.is_none() && summary.is_none() {
        let runner_errors: Vec<_> = output
            .lines()
            .map(str::trim)
            .filter(|line| line.starts_with("ERROR:"))
            .take(3)
            .collect();
        let no_tests = check.provenance.as_ref().and_then(|p| p.exit_code) == Some(5)
            && output
                .lines()
                .map(str::trim)
                .any(|line| line.trim_matches('=').trim().starts_with("no tests ran"));
        if !runner_errors.is_empty() || no_tests {
            return Some(RootCause {
                cause: if no_tests && runner_errors.is_empty() {
                    "Pytest collected no tests".into()
                } else {
                    "Pytest reported a runner error".into()
                },
                evidence: if runner_errors.is_empty() {
                    "No tests ran; Pytest exited with code 5.".into()
                } else {
                    runner_errors.join("\n")
                },
                hint: "Inspect test discovery, command arguments and Pytest configuration.".into(),
            });
        }
        let exit = check
            .provenance
            .as_ref()
            .and_then(|provenance| provenance.exit_code)
            .map(|code| format!(" (exit code {code})"))
            .unwrap_or_default();
        return Some(RootCause {
            cause: format!(
                "Pytest did not complete successfully{exit}. No failed-test diagnostic was captured; the cause is unknown."
            ),
            evidence: String::new(),
            hint: "Inspect the full Pytest log and runner details to determine why the process stopped. The exit code alone does not establish the cause."
                .into(),
        });
    }

    Some(RootCause {
        cause: summary
            .unwrap_or("Pytest reported test failure or error diagnostics")
            .to_string(),
        evidence: excerpt.unwrap_or_default(),
        hint: "Inspect the captured test diagnostics and their inputs in the full Pytest log before choosing a fix."
            .into(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn interrupted_pytest() -> CheckResult {
        CheckResult {
            name: "Pytest".to_string(),
            status: crate::checks::CheckStatus::Failed,
            duration: std::time::Duration::from_secs(111),
            output: "===== test session starts =====\n\
                rootdir: /tmp/project\n\
                collecting ... collected 3293 items\n\
                tests/test_parser.py::test_failed_setup_is_reported PASSED [ 25%]\n\
                tests/test_parser.py::test_error[2026-02-30T00:00:00Z] PASSED [ 25%]\n\
                tests/test_parser.py::test_next_case "
                .to_string(),
            cached: false,
            provenance: Some(crate::checks::CheckProvenance {
                command: "uv run pytest -v".to_string(),
                tool_version: None,
                cwd: "/tmp/project".to_string(),
                target_sha: None,
                tree_state: None,
                exit_code: Some(137),
                started_at: String::new(),
                finished_at: String::new(),
                hard_fail_signatures: vec![],
                cache_key: None,
                executed_scope: None,
            }),
        }
    }

    #[test]
    fn pytest_runner_timeout_without_exit_code_is_explicit() {
        for launcher in ["pytest", "uv"] {
            let mut check = interrupted_pytest();
            check.status = CheckStatus::Error;
            check.provenance.as_mut().unwrap().exit_code = None;
            check.output = format!("{launcher} timed out after 600s");
            assert_eq!(
                extract_root_cause(&check).unwrap().cause,
                "Process timed out"
            );
            assert_eq!(findings::check_failure_excerpt(&check), check.output);
            check.output = format!("tests/test_x.py::test_{launcher} timed out after 600s PASSED");
            assert_ne!(
                extract_root_cause(&check).unwrap().cause,
                "Process timed out"
            );
        }
    }

    #[test]
    fn pytest_recorded_timeout_precedes_progress_text() {
        let mut check = interrupted_pytest();
        check.provenance.as_mut().unwrap().exit_code = Some(-1);
        let diagnostic = extract_root_cause(&check).unwrap();
        assert_eq!(diagnostic.cause, "Process timed out");
        assert!(findings::check_failure_excerpt(&check).contains("runner timeout"));
        check.provenance.as_mut().unwrap().exit_code = Some(137);
        check
            .output
            .push_str("\ntests/test_x.py::test_timed_out PASSED");
        assert!(
            extract_root_cause(&check)
                .unwrap()
                .cause
                .contains("unknown")
        );
    }

    #[test]
    fn pytest_retains_explicit_runner_diagnostics_without_inventing_test_failures() {
        for (code, output, expected) in [
            (5, "===== no tests ran in 0.01s =====", "No tests ran"),
            (
                4,
                "ERROR: usage: pytest [options]\nERROR: unrecognized arguments: --bad",
                "unrecognized arguments",
            ),
        ] {
            let mut check = interrupted_pytest();
            check.output = output.into();
            check.provenance.as_mut().unwrap().exit_code = Some(code);
            let diagnostic = extract_root_cause(&check).unwrap();
            assert!(!diagnostic.cause.contains("unknown"));
            assert!(diagnostic.evidence.contains(expected));
            assert!(findings::check_failure_excerpt(&check).contains(expected));
            assert!(findings::parse_pytest_failures(output).is_empty());
        }
        let mut check = interrupted_pytest();
        check.provenance.as_mut().unwrap().exit_code = Some(5);
        assert!(
            extract_root_cause(&check)
                .unwrap()
                .cause
                .contains("unknown")
        );
    }

    #[test]
    fn pytest_interrupted_process_does_not_invent_a_failed_test() {
        let check = interrupted_pytest();
        let diagnostic = extract_root_cause(&check).expect("process diagnostic");
        assert!(diagnostic.cause.contains("exit code 137"));
        assert!(
            diagnostic
                .cause
                .contains("No failed-test diagnostic was captured")
        );
        assert!(diagnostic.cause.contains("cause is unknown"));
        assert!(diagnostic.evidence.is_empty());
        assert!(!diagnostic.hint.contains("reported test failure"));
        assert!(!diagnostic.cause.contains("SIGKILL"));
        assert!(!diagnostic.cause.contains("memory"));
        assert_eq!(findings::check_failure_excerpt(&check), diagnostic.cause);
    }

    #[test]
    fn pytest_failure_summary_uses_diagnostics_and_plain_exit_code() {
        let tmp = tempfile::tempdir().expect("summary directory");
        crate::artifacts::generate_failures_summary(tmp.path(), &[interrupted_pytest()])
            .expect("failure summary");
        let summary = std::fs::read_to_string(tmp.path().join("FAILURES_SUMMARY.md")).unwrap();
        assert!(summary.contains("**Status:** failed"));
        assert!(summary.contains("**Exit code:** 137\n"));
        assert!(summary.contains("### Failure details"));
        assert!(summary.contains("cause is unknown"));
        for misleading in [
            "Some(137)",
            "Root Cause",
            "PASSED",
            "test_failed_setup_is_reported",
            "test_error[",
            "test session starts",
            "rootdir:",
            "reported test failure and its inputs",
        ] {
            assert!(
                !summary.contains(misleading),
                "unexpected {misleading}: {summary}"
            );
        }
    }

    #[test]
    fn pytest_dispatch_reports_python_evidence_and_final_summary() {
        let check = CheckResult {
            name: "Pytest".to_string(),
            status: crate::checks::CheckStatus::Failed,
            duration: std::time::Duration::ZERO,
            output: "===== test session starts =====\n\
                tests/test_parser.py::test_failed_input_is_handled PASSED\n\
                tests/test_parser.py::test_roundtrip FAILED\n\
                ===== FAILURES =====\n\
                _____ test_roundtrip _____\n\
                E   AssertionError: unexpected message\n\
                tests/test_parser.py:42: AssertionError\n\
                ===== short test summary info =====\n\
                FAILED tests/test_parser.py::test_roundtrip\n\
                ===== 1 failed, 12 passed in 0.10s ====="
                .to_string(),
            cached: false,
            provenance: None,
        };
        let diagnostic = extract_root_cause(&check).expect("diagnostic");
        assert_eq!(diagnostic.cause, "1 failed, 12 passed in 0.10s");
        assert!(
            diagnostic
                .evidence
                .contains("AssertionError: unexpected message")
        );
        assert!(diagnostic.evidence.contains("tests/test_parser.py:42"));
        assert!(!diagnostic.evidence.contains("test session starts"));
        assert!(!diagnostic.hint.contains("cargo"));
        assert!(!diagnostic.hint.contains("Run test suite"));
        let tmp = tempfile::tempdir().expect("summary directory");
        crate::artifacts::generate_failures_summary(tmp.path(), &[check]).expect("failure summary");
        let summary = std::fs::read_to_string(tmp.path().join("FAILURES_SUMMARY.md")).unwrap();
        assert!(summary.contains("tests/test_parser.py:42"));
        assert!(summary.contains("AssertionError: unexpected message"));
        assert!(!summary.contains("PASSED"));
        assert!(!summary.contains("test session starts"));
    }
}
