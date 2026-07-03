use assert_cmd::prelude::*;
use predicates::prelude::*;
use prview::git::git_cmd;
use std::fs;
use std::path::Path;
use std::process::Command;
use tempfile::TempDir;

fn run_git(repo: &Path, args: &[&str]) {
    let status = git_cmd()
        .args(args)
        .current_dir(repo)
        .status()
        .expect("failed to run git command");
    assert!(status.success(), "git command failed: {:?}", args);
}

fn create_fixture_repo() -> TempDir {
    let temp = tempfile::tempdir().expect("tempdir");
    let repo = temp.path();

    run_git(repo, &["init"]);
    run_git(repo, &["config", "user.name", "Test User"]);
    run_git(repo, &["config", "user.email", "test@example.com"]);

    fs::write(repo.join("README.md"), "hello\n").expect("write file");
    run_git(repo, &["add", "README.md"]);
    run_git(repo, &["commit", "-m", "initial"]);
    run_git(repo, &["branch", "-M", "main"]);

    run_git(repo, &["checkout", "-b", "feature/json-contract"]);
    fs::write(repo.join("README.md"), "hello\nworld\n").expect("update file");
    run_git(repo, &["add", "README.md"]);
    run_git(repo, &["commit", "-m", "change"]);

    temp
}

fn run_json_quiet(repo: &Path, extra_args: &[&str]) -> serde_json::Value {
    let mut args = vec!["--json", "--quiet", "--no-zip", "--no-heuristics"];
    args.extend_from_slice(extra_args);

    let assert = Command::new(assert_cmd::cargo::cargo_bin!("prview"))
        .current_dir(repo)
        .args(args)
        .assert()
        .success();

    let output = assert.get_output();
    let stdout = String::from_utf8(output.stdout.clone()).expect("utf8 stdout");

    assert!(!stdout.contains("prview - PR Review"));
    assert!(!stdout.contains("Running quality checks"));

    serde_json::from_str(stdout.trim()).expect("stdout should be valid JSON")
}

#[test]
fn json_without_quiet_still_writes_only_json_to_stdout() {
    let temp = create_fixture_repo();
    let repo = temp.path();

    // No --quiet: --json alone must keep stdout parseable. Previously the human
    // banner and progress printed to stdout ahead of the JSON payload.
    let assert = Command::new(assert_cmd::cargo::cargo_bin!("prview"))
        .current_dir(repo)
        .args([
            "--json",
            "--no-zip",
            "--no-heuristics",
            "feature/json-contract",
            "main",
        ])
        .assert()
        .success();

    let stdout = String::from_utf8(assert.get_output().stdout.clone()).expect("utf8 stdout");
    assert!(
        !stdout.contains("prview - PR Review"),
        "human banner must not pollute --json stdout"
    );
    let payload: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("stdout should be valid JSON without --quiet");
    assert_eq!(payload["schema_version"], "cli-json/v1");
}

#[test]
fn quiet_without_json_suppresses_human_banner() {
    let temp = create_fixture_repo();
    let repo = temp.path();

    // --quiet must suppress the interactive human banner/progress even when
    // --json is absent. Previously the emit gate keyed only on --json, so a
    // quiet-but-not-json run still streamed the banner to stdout (PR #12 review).
    let output = Command::new(assert_cmd::cargo::cargo_bin!("prview"))
        .current_dir(repo)
        .args([
            "--quiet",
            "--no-zip",
            "--no-heuristics",
            "feature/json-contract",
            "main",
        ])
        .output()
        .expect("run prview");

    let stdout = String::from_utf8(output.stdout).expect("utf8 stdout");
    assert!(
        !stdout.contains("prview - PR Review"),
        "--quiet must suppress the human banner on stdout, got:\n{stdout}"
    );
    assert!(
        !stdout.contains("Running quality checks"),
        "--quiet must suppress human progress on stdout, got:\n{stdout}"
    );
}

#[test]
fn json_quiet_writes_machine_safe_json_to_stdout() {
    let temp = create_fixture_repo();
    let repo = temp.path();

    let payload = run_json_quiet(repo, &["feature/json-contract", "main"]);

    assert_eq!(payload["schema_version"], "cli-json/v1");
    assert!(payload["status"].as_str().is_some());
    assert!(payload["verdict"].as_str().is_some());
    assert!(payload["allow_merge"].is_boolean());
    assert!(payload["quality_pass"].is_boolean());
    assert!(payload["duration_secs"].is_number());
    assert_eq!(payload["target"], "feature/json-contract");
    assert_eq!(payload["bases"], serde_json::json!(["main"]));
    assert!(payload["output_dir"].as_str().is_some());
    assert!(payload["mode"].is_object());
    assert!(payload["checks_summary"].is_object());
    assert!(payload["checks_summary"]["total"].as_u64().is_some());
    assert!(payload["top_failures"].as_array().is_some());
    assert!(payload["context_artifacts"].as_array().is_some());
    assert!(payload["artifacts"]["report_json"].as_str().is_some());
    assert!(payload.get("diffs").is_none());
    assert!(payload.get("heuristics").is_none());
}

#[test]
fn update_json_quiet_without_new_commits_still_returns_json_payload() {
    let temp = create_fixture_repo();
    let repo = temp.path();

    let _first = run_json_quiet(repo, &["--update", "feature/json-contract", "main"]);
    let second = run_json_quiet(repo, &["--update", "feature/json-contract", "main"]);

    assert_eq!(second["target"], "feature/json-contract");
    assert_eq!(second["bases"], serde_json::json!(["main"]));
    assert!(second["output_dir"].as_str().is_some());
    assert!(second["checks_summary"].is_object());
    assert!(second["top_failures"].is_array());
    assert!(second["context_artifacts"].is_array());
}

#[test]
fn update_without_json_exits_zero_when_unchanged() {
    let temp = create_fixture_repo();
    let repo = temp.path();

    // First --update run generates artifacts; the second sees no new commits.
    // The human (non-JSON) path must exit 0 for that unchanged run, matching
    // the JSON contract path.
    Command::new(assert_cmd::cargo::cargo_bin!("prview"))
        .current_dir(repo)
        .args([
            "--update",
            "--no-zip",
            "--no-heuristics",
            "feature/json-contract",
            "main",
        ])
        .assert()
        .success();

    Command::new(assert_cmd::cargo::cargo_bin!("prview"))
        .current_dir(repo)
        .args([
            "--update",
            "--no-zip",
            "--no-heuristics",
            "feature/json-contract",
            "main",
        ])
        .assert()
        .code(0);
}

#[test]
fn json_quiet_stdout_omits_full_report_payload_fields() {
    let temp = create_fixture_repo();
    let repo = temp.path();

    let payload = run_json_quiet(repo, &["feature/json-contract", "main"]);
    let serialized = serde_json::to_string(&payload).expect("serialize payload");

    assert!(payload.get("diffs").is_none());
    assert!(payload.get("checks_summary").is_some());
    assert!(
        !payload["checks_summary"].is_array(),
        "checks_summary should be a summary object"
    );
    assert!(
        !serialized.contains("\"output\":"),
        "stdout JSON should not include raw check output blobs"
    );
}

#[test]
fn generated_merge_gate_passes_repo_validator() {
    let temp = create_fixture_repo();
    let repo = temp.path();

    let payload = run_json_quiet(repo, &["feature/json-contract", "main"]);
    let output_dir = Path::new(
        payload["output_dir"]
            .as_str()
            .expect("output_dir should be a string"),
    );
    let merge_gate = output_dir.join("00_summary/MERGE_GATE.json");
    let validator = Path::new(env!("CARGO_MANIFEST_DIR")).join("tools/validate_merge_gate.py");

    Command::new("python3")
        .arg(validator)
        .arg(&merge_gate)
        .assert()
        .success();
}

/// Resolve an executable by scanning `PATH` (test helper; no external crate).
#[cfg(unix)]
fn resolve_in_path(bin: &str) -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(bin))
        .find(|candidate| candidate.is_file())
}

/// Regression: when a quality tool is missing on the runner, its check is
/// skipped-by-unavailable. The generated MERGE_GATE.json must still satisfy its
/// own contract validator — a skipped check previously emitted `null` evidence
/// and `null` duration_secs, and a degraded run emitted the legacy `HOLD`
/// verdict, both of which failed `validate_merge_gate.py` on CI runners that
/// lack semgrep (P1 self-signal: the artifact failed its own gate).
///
/// Hermetic: run prview against a `PATH` that contains `git` but NOT `semgrep`,
/// forcing the semgrep check to skip regardless of what is installed locally.
#[cfg(unix)]
#[test]
fn merge_gate_validates_when_a_quality_tool_is_missing() {
    let temp = create_fixture_repo();
    let repo = temp.path();

    let git_path = resolve_in_path("git").expect("git must be resolvable on PATH");
    let bin = tempfile::tempdir().expect("bin tempdir");
    std::os::unix::fs::symlink(&git_path, bin.path().join("git")).expect("symlink git");

    let assert = Command::new(assert_cmd::cargo::cargo_bin!("prview"))
        .current_dir(repo)
        .env("PATH", bin.path())
        .args([
            "--json",
            "--quiet",
            "--no-zip",
            "--no-heuristics",
            "feature/json-contract",
            "main",
        ])
        .assert()
        .success();

    let stdout = String::from_utf8(assert.get_output().stdout.clone()).expect("utf8 stdout");
    let payload: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("stdout should be valid JSON");
    let output_dir = Path::new(
        payload["output_dir"]
            .as_str()
            .expect("output_dir should be a string"),
    );
    let merge_gate = output_dir.join("00_summary/MERGE_GATE.json");

    // The skipped check must carry contract-valid fields, not nulls.
    let gate: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&merge_gate).expect("read gate")).expect("parse");
    for check in gate["checks"].as_array().expect("checks array") {
        assert!(
            check["duration_secs"].as_f64().map(|d| d >= 0.0) == Some(true),
            "every check (incl. skipped) needs a non-negative duration_secs: {check}"
        );
        let evidence = check["evidence"].as_str().unwrap_or("");
        assert!(
            !evidence.trim().is_empty(),
            "every check (incl. skipped) needs non-empty evidence: {check}"
        );
    }

    // And the whole artifact must pass its own schema validator.
    let validator = Path::new(env!("CARGO_MANIFEST_DIR")).join("tools/validate_merge_gate.py");
    Command::new("python3")
        .arg(validator)
        .arg(&merge_gate)
        .assert()
        .success();
}

#[test]
fn generated_merge_gate_nulls_inline_findings_path_when_sarif_is_absent() {
    let temp = create_fixture_repo();
    let repo = temp.path();

    let payload = run_json_quiet(repo, &["feature/json-contract", "main"]);
    let output_dir = Path::new(
        payload["output_dir"]
            .as_str()
            .expect("output_dir should be a string"),
    );
    let merge_gate = output_dir.join("00_summary/MERGE_GATE.json");
    let raw = fs::read_to_string(&merge_gate).expect("read merge gate");
    let gate: serde_json::Value = serde_json::from_str(&raw).expect("parse merge gate");

    assert_eq!(gate["inline_findings"]["findings_count"].as_u64(), Some(0));
    assert!(
        gate["inline_findings"]["file"].is_null(),
        "inline_findings.file should be null when no SARIF file is written"
    );
    assert!(
        gate["files"]["inline_findings"].is_null(),
        "files.inline_findings should be null when no SARIF file is written"
    );
}

#[test]
fn generated_artifacts_include_valid_inline_findings_sarif() {
    let temp = create_fixture_repo();
    let repo = temp.path();

    let payload = run_json_quiet(repo, &["feature/json-contract", "main"]);
    let output_dir = Path::new(
        payload["output_dir"]
            .as_str()
            .expect("output_dir should be a string"),
    );
    let sarif_path = output_dir.join("30_context/INLINE_FINDINGS.sarif");

    // SARIF file is only generated when there are actual findings.
    // In the fixture repo (no failing checks), the file may not exist.
    if sarif_path.exists() {
        let raw = fs::read_to_string(&sarif_path).expect("read sarif");
        let sarif: serde_json::Value = serde_json::from_str(&raw).expect("parse sarif");

        assert_eq!(sarif["version"].as_str(), Some("2.1.0"));
        let runs = sarif["runs"].as_array().expect("runs should be an array");
        assert!(
            !runs.is_empty(),
            "SARIF file should only exist with findings"
        );
        for run in runs {
            assert!(
                run["tool"]["driver"]["name"].is_string(),
                "each run must have tool.driver.name"
            );
            assert!(run["results"].is_array(), "each run must have results[]");
        }
    }
    // If file doesn't exist, that's correct — no findings means no SARIF file
}

#[test]
fn default_run_generates_human_html_entrypoints() {
    let temp = create_fixture_repo();
    let repo = temp.path();

    let payload = run_json_quiet(repo, &["feature/json-contract", "main"]);
    let output_dir = Path::new(
        payload["output_dir"]
            .as_str()
            .expect("output_dir should be a string"),
    );

    assert!(
        output_dir.join("review.html").exists(),
        "review.html should always be generated as the standard human export"
    );
    assert!(
        output_dir.join("dashboard.html").exists(),
        "dashboard.html should be generated by default"
    );
    assert_eq!(
        payload["artifacts"]["review_html"].as_str(),
        Some("review.html")
    );
    assert_eq!(
        payload["artifacts"]["dashboard_html"].as_str(),
        Some("dashboard.html")
    );

    let index = fs::read_to_string(output_dir.join("AI_INDEX.md")).expect("read ai index");
    let dashboard_pos = index
        .find("dashboard.html")
        .expect("dashboard should be listed in AI_INDEX");
    let gate_pos = index
        .find("00_summary/MERGE_GATE.md")
        .expect("gate should be listed in AI_INDEX");
    assert!(
        dashboard_pos < gate_pos,
        "human dashboard should be the first recommended reading surface"
    );
}

#[test]
fn report_json_includes_directory_aggregation_for_diff() {
    let temp = create_fixture_repo();
    let repo = temp.path();

    let payload = run_json_quiet(repo, &["feature/json-contract", "main"]);
    let output_dir = Path::new(
        payload["output_dir"]
            .as_str()
            .expect("output_dir should be a string"),
    );
    let report_path = output_dir.join("report.json");
    let report_raw = fs::read_to_string(&report_path).expect("read report json");
    let report: serde_json::Value = serde_json::from_str(&report_raw).expect("parse report json");

    let directories = report["diff"]["directories"]
        .as_array()
        .expect("diff.directories should be an array");
    assert!(
        !directories.is_empty(),
        "diff.directories should contain aggregated entries"
    );

    let root_dir = directories
        .iter()
        .find(|entry| entry["path"].as_str() == Some("."))
        .expect("root directory aggregation should exist for README.md");
    assert_eq!(root_dir["files_changed"].as_u64(), Some(1));
    assert_eq!(root_dir["insertions"].as_u64(), Some(1));
    assert_eq!(root_dir["deletions"].as_u64(), Some(0));
    assert_eq!(root_dir["churn"].as_u64(), Some(1));
    assert_eq!(
        report["quality"]["breaking_changes"]["signature_changes_count"].as_u64(),
        Some(0)
    );
}

#[test]
fn no_dashboard_flag_skips_dashboard_file_and_surfaces_in_run_json() {
    let temp = create_fixture_repo();
    let repo = temp.path();

    let payload = run_json_quiet(repo, &["--no-dashboard", "feature/json-contract", "main"]);
    let output_dir = Path::new(
        payload["output_dir"]
            .as_str()
            .expect("output_dir should be a string"),
    );

    assert!(
        !output_dir.join("dashboard.html").exists(),
        "dashboard.html should not be generated when --no-dashboard is set"
    );
    assert!(
        output_dir.join("review.html").exists(),
        "review.html should still be generated when interactive dashboard is disabled"
    );
    assert_eq!(
        payload["artifacts"]["review_html"].as_str(),
        Some("review.html")
    );
    assert!(
        payload["artifacts"].get("dashboard_html").is_none(),
        "dashboard_html should be absent from compact JSON when --no-dashboard is set"
    );

    let run_path = output_dir.join("00_summary/RUN.json");
    let run_raw = fs::read_to_string(&run_path).expect("read run json");
    let run: serde_json::Value = serde_json::from_str(&run_raw).expect("parse run json");
    assert_eq!(run["flags"]["dashboard"].as_bool(), Some(false));
}

#[test]
fn doctor_surfaces_config_error_cause_instead_of_blanket_message() {
    // Outside a git repo, Config::from_cli fails. Doctor must report the real
    // reason with a colon, not the old blanket "(maybe not in a project?)".
    let temp = tempfile::tempdir().expect("tempdir");

    Command::new(assert_cmd::cargo::cargo_bin!("prview"))
        .current_dir(temp.path())
        .arg("doctor")
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "Could not determine active profile:",
        ))
        .stdout(predicate::str::contains("maybe not in a project?").not());
}

#[test]
fn completions_generates_valid_output_with_known_subcommands() {
    for shell in &["bash", "zsh", "fish"] {
        let output = Command::new(assert_cmd::cargo::cargo_bin!("prview"))
            .args(["completions", shell])
            .output()
            .expect("run completions");

        assert!(
            output.status.success(),
            "completions {} should succeed",
            shell
        );

        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            stdout.contains("prview"),
            "completions {} should reference the binary name",
            shell
        );
        assert!(
            stdout.contains("state"),
            "completions {} should include 'state' subcommand",
            shell
        );
        assert!(
            stdout.contains("completions"),
            "completions {} should include 'completions' subcommand",
            shell
        );
    }
}

#[test]
fn init_command_creates_policy_and_updates_gitignore() {
    let temp = tempfile::tempdir().expect("tempdir");
    let repo = temp.path();

    // 1. Setup git repo
    run_git(repo, &["init"]);
    fs::write(repo.join(".gitignore"), "target/\n").expect("write gitignore");

    // 2. Run prview init
    Command::new(assert_cmd::cargo::cargo_bin!("prview"))
        .current_dir(repo)
        .arg("init")
        .assert()
        .success()
        .stdout(predicate::str::contains("Initializing prview"))
        .stdout(predicate::str::contains(
            "Detected project profile: Generic",
        ))
        .stdout(predicate::str::contains("Created .prview-policy.yml"))
        .stdout(predicate::str::contains("Updated .gitignore"));

    // 3. Verify files
    let policy = repo.join(".prview-policy.yml");
    assert!(policy.exists());
    let policy_content = fs::read_to_string(policy).expect("read policy");
    assert!(policy_content.contains("mode: warn"));
    assert!(policy_content.contains("Generic profile"));

    let gitignore = repo.join(".gitignore");
    let gitignore_content = fs::read_to_string(gitignore).expect("read gitignore");
    assert!(gitignore_content.contains("prview-artifacts/"));
    assert!(gitignore_content.contains("target/"));

    // 4. Running init again should be idempotent (skipping)
    Command::new(assert_cmd::cargo::cargo_bin!("prview"))
        .current_dir(repo)
        .arg("init")
        .assert()
        .success()
        .stdout(predicate::str::contains(
            ".prview-policy.yml already exists",
        ))
        .stdout(predicate::str::contains(
            "prview-artifacts already in .gitignore",
        ));
}
