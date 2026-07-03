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
        || (name_lower.contains("test") && !name_lower.contains("cargo"))
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

    // Pytest (Python)
    if name_lower.contains("pytest") {
        return extract_pytest_root_cause(output);
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

pub(crate) fn extract_pytest_root_cause(output: &str) -> Option<RootCause> {
    let summary = output.lines().find(|l| {
        l.contains("failed") && l.contains("passed")
            || l.starts_with("FAILED")
            || l.contains("error")
    });
    let first_failure = output
        .lines()
        .find(|l| l.starts_with("FAILED") || l.contains("ERRORS"));

    Some(RootCause {
        cause: summary.unwrap_or("Pytest failures").to_string(),
        evidence: first_failure.unwrap_or("").to_string(),
        hint: "Run pytest -x to reproduce first failure".into(),
    })
}
