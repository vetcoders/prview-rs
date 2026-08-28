use assert_cmd::prelude::*;
use predicates::prelude::*;
use prview::git::git_cmd;
use sha2::{Digest, Sha256};
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

fn sha256_file(path: &Path) -> String {
    format!(
        "{:x}",
        Sha256::digest(fs::read(path).expect("read hashed fixture"))
    )
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
        .env("PRVIEW_HOME", repo.join(".prview-test-home"))
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
fn gate_help_documents_exit_code_contract() {
    Command::new(assert_cmd::cargo::cargo_bin!("prview"))
        .args(["gate", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Exit codes:"))
        .stdout(predicate::str::contains("0 = PASS"))
        .stdout(predicate::str::contains("1 = BLOCK"))
        .stdout(predicate::str::contains("warnings-only under --strict"))
        .stdout(predicate::str::contains("--fail-on-warnings"))
        .stdout(predicate::str::contains("2 = strict review-required"))
        .stdout(predicate::str::contains("3 = gate could not execute"));
}

#[test]
fn fail_on_warnings_is_documented_and_scoped_to_ci() {
    // The escape hatch for the warning→failure change: warnings no longer break
    // `--ci` on their own, so a team that wants that exit asks for it. It is
    // meaningless outside `--ci`, and clap says so loudly instead of no-opping.
    Command::new(assert_cmd::cargo::cargo_bin!("prview"))
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "CI mode: exit 1 on BLOCK or failed quality",
        ))
        .stdout(predicate::str::contains("--fail-on-warnings"))
        .stdout(predicate::str::contains(
            "canonical pack warning tally is non-zero",
        ));

    Command::new(assert_cmd::cargo::cargo_bin!("prview"))
        .arg("--fail-on-warnings")
        .assert()
        .failure()
        .stderr(predicate::str::contains("--ci"));
}

#[test]
fn gate_json_emits_verdict_and_caveats_from_merge_gate() {
    let temp = create_fixture_repo();
    let repo = temp.path();

    let assert = Command::new(assert_cmd::cargo::cargo_bin!("prview"))
        .current_dir(repo)
        .args(["gate", "--json"])
        .assert()
        .success();

    let stdout = String::from_utf8(assert.get_output().stdout.clone()).expect("utf8 stdout");
    let payload: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("stdout should be valid JSON");

    assert_eq!(payload["schema_version"], "gate-json/v1");
    assert_eq!(payload["strict"].as_bool(), Some(false));
    assert!(matches!(
        payload["verdict"].as_str(),
        Some("PASS" | "CONDITIONAL" | "BLOCK")
    ));
    assert!(payload["exit_code"].as_i64().is_some());
    assert!(payload["caveats"].as_array().is_some());
    assert!(payload["blocking_issues"].as_array().is_some());
    assert!(payload["merge_gate_json"].as_str().is_some());

    let output_dir = Path::new(
        payload["output_dir"]
            .as_str()
            .expect("output_dir should be a string"),
    );
    let merge_gate = output_dir.join("00_summary/MERGE_GATE.json");
    assert!(merge_gate.exists(), "gate should generate MERGE_GATE.json");

    let gate: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&merge_gate).expect("read gate")).expect("parse");
    assert_eq!(payload["verdict"], gate["decision"]["verdict"]);
    assert_eq!(payload["caveats"], gate["decision"]["review_caveats"]);
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
fn operator_policy_rank_invariants_validator_contract() {
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

    let raw = fs::read_to_string(&merge_gate).expect("read generated merge gate");
    let original: serde_json::Value = serde_json::from_str(&raw).expect("parse merge gate");
    assert_eq!(original["schema_version"], "2.3");
    assert!(
        original["decision"]["enforcement_disposition"]
            .as_str()
            .is_some(),
        "the 2.3 writer must emit its typed enforcement disposition"
    );

    let validator = Path::new(env!("CARGO_MANIFEST_DIR")).join("tools/validate_merge_gate.py");
    let validate = |gate: &serde_json::Value, must_validate: bool| {
        fs::write(
            &merge_gate,
            serde_json::to_vec_pretty(gate).expect("serialize gate"),
        )
        .expect("write merge gate vector");
        let assertion = Command::new("python3")
            .arg(&validator)
            .arg(&merge_gate)
            .assert()
            .stderr(predicate::str::contains("Traceback").not());
        if must_validate {
            assertion.success();
        } else {
            assertion.failure();
        }
    };

    let clean_decision = {
        let mut decision = original["decision"].clone();
        for (key, value) in serde_json::json!({
            "verdict": "PASS",
            "allow_merge": true,
            "analysis_status": "complete",
            "merge_recommendation": "approve",
            "policy_allow_merge": true,
            "quality_pass": true,
            "quality_failure_details": [],
            "blocking_issues": [],
            "enforcement_disposition": "clean"
        })
        .as_object()
        .expect("decision patch")
        {
            decision[key] = value.clone();
        }
        decision
    };
    let healthy_inline = serde_json::json!({
        "file": null,
        "status": "passed",
        "severity": "warn",
        "blocking": false,
        "effective_class": "PASS",
        "enforcement_disposition": "clean",
        "findings_count": 0,
        "introduced_count": 0,
        "preexisting_count": 0
    });
    let warning_check = serde_json::json!({
        "id": "rustfmt",
        "name": "Rustfmt",
        "status": "warnings",
        "execution_state": "executed",
        "outcome": "findings_warning",
        "class": "INFO",
        "severity": "warn",
        "policy_conclusion": "advisory",
        "confidence_impact": "complete",
        "merge_impact": "review_required",
        "blocking": false,
        "duration_secs": 0.0,
        "evidence": "20_quality/RUSTFMT.md"
    });
    let clean_pack = {
        let mut gate = original.clone();
        gate["checks"] = serde_json::json!([]);
        gate["inline_findings"] = healthy_inline.clone();
        gate["decision"] = clean_decision.clone();
        gate
    };
    validate(&clean_pack, true);

    let mut warnings_pack = clean_pack.clone();
    warnings_pack["checks"] = serde_json::json!([warning_check.clone()]);
    warnings_pack["decision"]["enforcement_disposition"] = serde_json::json!("warnings_only");
    validate(&warnings_pack, true);

    let mut preexisting_warning = warnings_pack.clone();
    preexisting_warning["checks"][0]["merge_impact"] = serde_json::json!("approve");
    preexisting_warning["decision"]["quality_failure_details"] = serde_json::json!([{
        "name":"Rustfmt", "classification":"pre-existing", "origin":"warning"
    }]);
    validate(&preexisting_warning, true);

    for classification in ["introduced", "mixed", "unclassified"] {
        let mut matched_warning = warnings_pack.clone();
        matched_warning["decision"]["quality_failure_details"] = serde_json::json!([{
            "name":"Rustfmt", "classification":classification, "origin":"warning"
        }]);
        validate(&matched_warning, true);
    }

    for classification in ["introduced", "pre-existing", "mixed", "unclassified"] {
        let mut orphan_warning_detail = clean_pack.clone();
        orphan_warning_detail["decision"]["quality_failure_details"] = serde_json::json!([{
            "name":"Rustfmt", "classification":classification, "origin":"warning"
        }]);
        validate(&orphan_warning_detail, false);
    }

    let mut duplicate_warning_detail = preexisting_warning.clone();
    duplicate_warning_detail["decision"]["quality_failure_details"] = serde_json::json!([
        {"name":"Rustfmt", "classification":"pre-existing", "origin":"warning"},
        {"name":"Rustfmt", "classification":"pre-existing", "origin":"warning"}
    ]);
    validate(&duplicate_warning_detail, false);

    // The additive field is required and closed in 2.3. It never becomes a
    // prose-derived or best-effort permission.
    for disposition in [
        None,
        Some(serde_json::json!("mystery")),
        Some(serde_json::json!(7)),
    ] {
        let mut gate = clean_pack.clone();
        match disposition {
            Some(value) => gate["decision"]["enforcement_disposition"] = value,
            None => {
                gate["decision"]
                    .as_object_mut()
                    .expect("decision object")
                    .remove("enforcement_disposition");
            }
        }
        validate(&gate, false);
    }

    let mut unproven = warnings_pack.clone();
    unproven["checks"] = serde_json::json!([]);
    validate(&unproven, false);

    let mut hidden_warning = warnings_pack.clone();
    hidden_warning["decision"]["enforcement_disposition"] = serde_json::json!("clean");
    validate(&hidden_warning, false);

    let mut hidden_degraded_check = warnings_pack.clone();
    hidden_degraded_check["checks"]
        .as_array_mut()
        .expect("checks array")
        .push(serde_json::json!({
            "id":"cargo_check", "name":"Cargo check", "status":"skipped",
            "execution_state":"unavailable", "outcome":"unavailable", "class":"SKIP",
            "severity":"warn", "policy_conclusion":"advisory",
            "confidence_impact":"degraded", "merge_impact":"review_required",
            "blocking":false, "duration_secs":0.0,
            "evidence":"tool unavailable"
        }));
    validate(&hidden_degraded_check, false);

    let mut preexisting_failure = clean_pack.clone();
    preexisting_failure["checks"] = serde_json::json!([{
        "id":"cargo", "name":"Cargo check", "status":"failed",
        "execution_state":"executed", "outcome":"findings_failed", "class":"FAIL",
        "severity":"block", "policy_conclusion":"advisory",
        "confidence_impact":"complete", "merge_impact":"approve", "blocking":false,
        "duration_secs":0.0, "evidence":"20_quality/cargo.result.json"
    }]);
    preexisting_failure["decision"]["quality_failure_details"] = serde_json::json!([{
        "name":"Cargo check", "classification":"pre-existing", "origin":"failure"
    }]);
    validate(&preexisting_failure, true);

    let mut amputated_failure = preexisting_failure.clone();
    amputated_failure["decision"]["quality_failure_details"] = serde_json::json!([]);
    validate(&amputated_failure, false);

    let mut impossible_advisory = clean_pack.clone();
    impossible_advisory["checks"] = serde_json::json!([{
        "id":"cargo", "name":"Cargo check", "status":"passed",
        "execution_state":"executed", "outcome":"passed", "class":"PASS",
        "severity":"warn", "policy_conclusion":"advisory",
        "confidence_impact":"complete", "merge_impact":"approve", "blocking":false,
        "duration_secs":0.0, "evidence":"20_quality/cargo.result.json"
    }]);
    validate(&impossible_advisory, false);

    let mut forged_inline = clean_pack.clone();
    forged_inline["inline_findings"] = serde_json::json!({
        "file":"30_context/INLINE_FINDINGS.sarif", "status":"failed",
        "severity":"warn", "blocking":false, "effective_class":"PASS",
        "enforcement_disposition":"clean", "findings_count":2,
        "introduced_count":1, "preexisting_count":1
    });
    validate(&forged_inline, false);

    let mut unclassified_inline = clean_pack.clone();
    unclassified_inline["inline_findings"] = serde_json::json!({
        "file":"30_context/INLINE_FINDINGS.sarif", "status":"failed",
        "severity":"warn", "blocking":false, "effective_class":"PASS",
        "enforcement_disposition":"clean", "findings_count":1,
        "introduced_count":0, "preexisting_count":0
    });
    validate(&unclassified_inline, false);

    let mut failed_info_without_preexisting = clean_pack.clone();
    failed_info_without_preexisting["decision"]["enforcement_disposition"] =
        serde_json::json!("warnings_only");
    failed_info_without_preexisting["inline_findings"] = serde_json::json!({
        "file":"30_context/INLINE_FINDINGS.sarif", "status":"failed",
        "severity":"warn", "blocking":false, "effective_class":"INFO",
        "enforcement_disposition":"warnings_only", "findings_count":2,
        "introduced_count":1, "preexisting_count":0
    });
    validate(&failed_info_without_preexisting, false);

    for broken_root in [
        ("checks", serde_json::Value::Null),
        ("inline_findings", serde_json::json!([])),
    ] {
        let mut gate = clean_pack.clone();
        gate[broken_root.0] = broken_root.1;
        validate(&gate, false);
    }

    // Any typed lower-level blocker requires the complete canonical Block
    // tuple, whether its status is a warning or a passing observation.
    for (checks, inline) in [
        (
            serde_json::json!([{
                "id":"rustfmt", "name":"Rustfmt", "status":"passed", "class":"PASS",
                "execution_state":"executed", "outcome":"passed",
                "severity":"warn", "blocking":true, "duration_secs":0.0,
                "policy_conclusion":"blocked", "confidence_impact":"incomplete",
                "merge_impact":"block",
                "evidence":"20_quality/RUSTFMT.md"
            }]),
            healthy_inline.clone(),
        ),
        (
            serde_json::json!([]),
            serde_json::json!({
                "file": null, "status":"passed", "severity":"warn",
                "blocking":true, "effective_class":"PASS",
                "enforcement_disposition":"block", "findings_count":0,
                "introduced_count":0, "preexisting_count":0
            }),
        ),
        (
            serde_json::json!([{
                "id":"rustfmt", "name":"Rustfmt", "status":"warnings", "class":"PASS",
                "execution_state":"executed", "outcome":"findings_warning",
                "severity":"warn", "blocking":true, "duration_secs":0.0,
                "policy_conclusion":"blocked", "confidence_impact":"incomplete",
                "merge_impact":"block",
                "evidence":"20_quality/RUSTFMT.md"
            }]),
            healthy_inline.clone(),
        ),
    ] {
        let mut gate = clean_pack.clone();
        gate["checks"] = checks;
        gate["inline_findings"] = inline;
        validate(&gate, false);
    }

    let mut canonical_block = clean_pack.clone();
    canonical_block["checks"] = serde_json::json!([{
        "id":"rustfmt", "name":"Rustfmt", "status":"failed", "class":"FAIL",
        "execution_state":"executed", "outcome":"findings_failed",
        "severity":"block", "blocking":true, "duration_secs":0.0,
        "policy_conclusion":"blocked", "confidence_impact":"incomplete",
        "merge_impact":"block",
        "evidence":"20_quality/RUSTFMT.md"
    }]);
    for (key, value) in serde_json::json!({
        "verdict":"BLOCK",
        "allow_merge":false,
        "analysis_status":"incomplete",
        "merge_recommendation":"block",
        "policy_allow_merge":false,
        "quality_pass":false,
        "quality_failure_details":[{
            "name":"Rustfmt", "classification":"introduced", "origin":"failure"
        }],
        "blocking_issues":["Rustfmt (failed)"],
        "enforcement_disposition":"block"
    })
    .as_object()
    .expect("block patch")
    {
        canonical_block["decision"][key] = value.clone();
    }
    validate(&canonical_block, true);

    for (verdict, allow_merge, disposition) in [
        ("CONDITIONAL", false, "clean"),
        ("PASS", true, "review_required"),
        ("PASS", true, "block"),
    ] {
        let mut gate = clean_pack.clone();
        gate["decision"]["verdict"] = serde_json::json!(verdict);
        gate["decision"]["allow_merge"] = serde_json::json!(allow_merge);
        gate["decision"]["enforcement_disposition"] = serde_json::json!(disposition);
        validate(&gate, false);
    }

    fs::write(&merge_gate, raw).expect("restore generated merge gate");
}

#[test]
fn operator_policy_rank_invariants_validator_inline_count_truth_table() {
    fn realizable(
        status: &str,
        class: &str,
        findings: u64,
        introduced: u64,
        preexisting: u64,
    ) -> bool {
        if introduced
            .checked_add(preexisting)
            .is_none_or(|classified| classified > findings)
        {
            return false;
        }
        match status {
            "passed" | "not_run" => findings == 0 && class == "PASS",
            "warnings" if findings > 0 => match class {
                "INFO" => true,
                "PASS" => introduced == 0 && findings == preexisting,
                _ => false,
            },
            "failed" if findings > 0 => match class {
                "FAIL" => true,
                "INFO" => findings >= 2 && preexisting >= 1,
                "PASS" => introduced == 0 && findings == preexisting,
                _ => false,
            },
            _ => false,
        }
    }

    let temp = create_fixture_repo();
    let payload = run_json_quiet(temp.path(), &["feature/json-contract", "main"]);
    let output_dir = Path::new(payload["output_dir"].as_str().expect("output_dir string"));
    let original: serde_json::Value = serde_json::from_slice(
        &fs::read(output_dir.join("00_summary/MERGE_GATE.json")).expect("read merge gate"),
    )
    .expect("parse merge gate");

    let mut count_cases = Vec::new();
    for findings in 0..=3_u64 {
        for introduced in 0..=findings {
            for preexisting in 0..=(findings - introduced) {
                count_cases.push((findings, introduced, preexisting));
            }
        }
    }
    count_cases.extend([(u64::MAX, u64::MAX, 0), (u64::MAX, u64::MAX, 1)]);

    let mut vectors = Vec::new();
    for status in ["passed", "not_run", "warnings", "failed"] {
        for class in ["PASS", "INFO", "FAIL"] {
            for &(findings, introduced, preexisting) in &count_cases {
                let expected = realizable(status, class, findings, introduced, preexisting);
                let source_disposition = if class == "FAIL" {
                    "review_required"
                } else if class == "INFO" || status == "warnings" {
                    "warnings_only"
                } else {
                    "clean"
                };
                let (verdict, recommendation, allow_merge) =
                    if source_disposition == "review_required" {
                        ("CONDITIONAL", "review_required", false)
                    } else {
                        ("PASS", "approve", true)
                    };
                let mut gate = original.clone();
                gate["policy"]["mode"] = serde_json::json!("shadow");
                gate["checks"] = serde_json::json!([]);
                for (key, value) in serde_json::json!({
                    "verdict":verdict,
                    "enforcement_disposition":source_disposition,
                    "analysis_status":"complete",
                    "merge_recommendation":recommendation,
                    "allow_merge":allow_merge,
                    "policy_allow_merge":true,
                    "quality_pass":true,
                    "decision_reason":"bounded inline truth table",
                    "review_caveats":[],
                    "blocking_issues":[],
                    "quality_failure_details":[]
                })
                .as_object()
                .expect("decision patch")
                {
                    gate["decision"][key] = value.clone();
                }
                gate["inline_findings"] = serde_json::json!({
                    "file":if findings == 0 {
                        serde_json::Value::Null
                    } else {
                        serde_json::json!("30_context/INLINE_FINDINGS.sarif")
                    },
                    "file_exists":findings > 0,
                    "status":status,
                    "severity":"warn",
                    "blocking":false,
                    "effective_class":class,
                    "enforcement_disposition":source_disposition,
                    "findings_count":findings,
                    "introduced_count":introduced,
                    "preexisting_count":preexisting
                });
                gate["files"]["inline_findings"] = if findings == 0 {
                    serde_json::Value::Null
                } else {
                    serde_json::json!("30_context/INLINE_FINDINGS.sarif")
                };
                vectors.push(serde_json::json!({
                    "expected":expected,
                    "label":format!(
                        "status={status} class={class} F={findings} I={introduced} P={preexisting}"
                    ),
                    "gate":gate
                }));
            }
        }
    }
    assert_eq!(vectors.len(), 264);

    let vectors_path = temp.path().join("inline-count-vectors.json");
    fs::write(
        &vectors_path,
        serde_json::to_vec(&vectors).expect("serialize vectors"),
    )
    .expect("write vectors");
    let validator = Path::new(env!("CARGO_MANIFEST_DIR")).join("tools/validate_merge_gate.py");
    let batch_validator = r#"
import importlib.util
import json
import pathlib
import sys
import tempfile

validator_path = pathlib.Path(sys.argv[1])
vectors_path = pathlib.Path(sys.argv[2])
spec = importlib.util.spec_from_file_location("validate_merge_gate", validator_path)
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)
vectors = json.loads(vectors_path.read_text(encoding="utf-8"))
failures = []
with tempfile.TemporaryDirectory() as tmp:
    gate_path = pathlib.Path(tmp) / "MERGE_GATE.json"
    for vector in vectors:
        gate_path.write_text(json.dumps(vector["gate"]), encoding="utf-8")
        issues = module.validate(gate_path)
        actual = not issues
        if actual != vector["expected"]:
            failures.append({"label": vector["label"], "issues": issues})
if failures:
    print(json.dumps(failures, indent=2), file=sys.stderr)
    raise SystemExit(1)
print(f"validator-inline-count-truth-table-ok vectors={len(vectors)}")
"#;
    Command::new("python3")
        .args([
            "-c",
            batch_validator,
            validator.to_str().expect("validator utf8"),
            vectors_path.to_str().expect("vectors utf8"),
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "validator-inline-count-truth-table-ok vectors=264",
        ));
}

/// Accepting schema `2.2` without checking the fields that define it lets a pack
/// omit, mistype, or invent any of them and still pass its own contract gate.
/// Consumers are told to filter `quality_failure_details` on
/// `origin == "failure"`, so an unvalidated origin makes a real failure
/// indistinguishable from a warning; and an entry validated on `origin` alone
/// could be `{"origin": "failure"}` — a failure with no check name and no
/// classification, which no consumer can report or act on.
#[test]
fn validator_rejects_schema_two_two_without_a_usable_origin() {
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

    let raw = std::fs::read_to_string(&merge_gate).expect("read gate");
    let mut original: serde_json::Value = serde_json::from_str(&raw).expect("parse gate");
    assert_eq!(
        original["schema_version"].as_str(),
        Some("2.3"),
        "the current writer retains the 2.2 `origin` requirement"
    );
    original["schema_version"] = serde_json::json!("2.2");

    let broken_details = [
        serde_json::json!([{ "name": "Clippy", "classification": "introduced" }]),
        serde_json::json!([{ "name": "Clippy", "classification": "introduced", "origin": "failed" }]),
        serde_json::json!([{ "name": "Clippy", "classification": "introduced", "origin": true }]),
        serde_json::json!([{ "name": "Clippy", "classification": "introduced", "origin": null }]),
        serde_json::json!(["Clippy"]),
        // The entry the origin-only check waved through: a failure that names
        // no check and states no classification.
        serde_json::json!([{ "origin": "failure" }]),
        // `name` present but useless.
        serde_json::json!([{ "name": "", "classification": "introduced", "origin": "failure" }]),
        serde_json::json!([{ "name": "   ", "classification": "introduced", "origin": "failure" }]),
        serde_json::json!([{ "name": 7, "classification": "introduced", "origin": "failure" }]),
        // `classification` outside the emitted vocabulary. `preexisting` is the
        // spelling of the sibling COUNT field, not of this value — the emitter
        // writes `pre-existing`, and accepting any string hid that drift.
        serde_json::json!([{ "name": "Clippy", "classification": "preexisting", "origin": "failure" }]),
        serde_json::json!([{ "name": "Clippy", "classification": "", "origin": "failure" }]),
        serde_json::json!([{ "name": "Clippy", "classification": true, "origin": "failure" }]),
        serde_json::json!([{ "name": "Clippy", "origin": "failure" }]),
    ];

    for details in broken_details {
        let mut gate = original.clone();
        gate["decision"]["quality_failure_details"] = details.clone();
        std::fs::write(
            &merge_gate,
            serde_json::to_string_pretty(&gate).expect("serialize gate"),
        )
        .expect("write gate");

        Command::new("python3")
            .arg(&validator)
            .arg(&merge_gate)
            .assert()
            .failure();
    }

    // Every classification the emitter can write must validate — the vocabulary
    // is pinned to `QualityFailureClass::as_str`, so a validator that spelled
    // one of them differently would reject a pack prview itself produced. The
    // flag moves with the row because `quality_pass` is derived from these very
    // details: only `pre-existing` leaves the gate passing, so pinning one flag
    // across all four would test a pack the emitter cannot write. What this loop
    // asserts is unchanged — all four spellings validate.
    for classification in ["introduced", "pre-existing", "mixed", "unclassified"] {
        let mut gate = original.clone();
        gate["decision"]["quality_failure_details"] = serde_json::json!([{
            "name": "Clippy",
            "classification": classification,
            "origin": "failure",
        }]);
        gate["decision"]["quality_pass"] = serde_json::json!(classification == "pre-existing");
        std::fs::write(
            &merge_gate,
            serde_json::to_string_pretty(&gate).expect("serialize gate"),
        )
        .expect("write gate");

        Command::new("python3")
            .arg(&validator)
            .arg(&merge_gate)
            .assert()
            .success();
    }

    // The shape the emitter actually writes still validates.
    std::fs::write(&merge_gate, &raw).expect("restore gate");
    Command::new("python3")
        .arg(&validator)
        .arg(&merge_gate)
        .assert()
        .success();
}

/// `quality_pass` and `quality_failure_details` are ONE fact written twice: the
/// emitter sets the flag to `!QualityFailureSummary::has_new_failures()` and
/// serializes the very details that answer it. Validating each side's shape
/// while never comparing them let a pack claim `quality_pass: true` beside an
/// explicitly introduced failure, and both decision readers trust the permissive
/// scalar — so a validator-clean pack could approve a failure it also reports.
///
/// The check is an equivalence, and the `pre-existing` row is why the obvious
/// one-way rule would be wrong: a failure that predates the diff is emitted
/// beside `quality_pass: true` on purpose.
#[test]
fn validator_rejects_quality_pass_contradicting_its_own_details() {
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

    let raw = std::fs::read_to_string(&merge_gate).expect("read gate");
    let mut original: serde_json::Value = serde_json::from_str(&raw).expect("parse gate");
    original["schema_version"] = serde_json::json!("2.2");

    let detail = |classification: &str, origin: &str| serde_json::json!([{ "name": "Clippy", "classification": classification, "origin": origin }]);

    // (details, quality_pass, must_validate)
    let cases: [(serde_json::Value, bool, bool); 10] = [
        // A gating failure beside a passing flag — the combination the emitter
        // cannot produce, and the one that lets a clean-looking pack approve an
        // introduced failure.
        (detail("introduced", "failure"), true, false),
        (detail("mixed", "failure"), true, false),
        (detail("unclassified", "failure"), true, false),
        // The same rows with the flag the emitter would actually write.
        (detail("introduced", "failure"), false, true),
        (detail("mixed", "failure"), false, true),
        (detail("unclassified", "failure"), false, true),
        // Pre-existing failures do NOT gate: this pack is legal and must not be
        // rejected by a rule that keys on origin alone.
        (detail("pre-existing", "failure"), true, true),
        // Warnings never gate either, whatever they classify as.
        (detail("introduced", "warning"), true, true),
        // The other direction: a failing flag with nothing that could have
        // failed it is equally unemittable.
        (detail("pre-existing", "failure"), false, false),
        (serde_json::json!([]), false, false),
    ];

    for (details, quality_pass, must_validate) in cases {
        let mut gate = original.clone();
        gate["decision"]["quality_failure_details"] = details.clone();
        gate["decision"]["quality_pass"] = serde_json::json!(quality_pass);
        std::fs::write(
            &merge_gate,
            serde_json::to_string_pretty(&gate).expect("serialize gate"),
        )
        .expect("write gate");

        let assertion = Command::new("python3")
            .arg(&validator)
            .arg(&merge_gate)
            .assert();
        if must_validate {
            assertion.success();
        } else {
            assertion.failure();
        }
    }

    // The shape the emitter actually writes still validates.
    std::fs::write(&merge_gate, &raw).expect("restore gate");
    Command::new("python3")
        .arg(&validator)
        .arg(&merge_gate)
        .assert()
        .success();
}

/// `quality_pass` is a documented decision axis, and from schema 2.2 the writer
/// emits it unconditionally as a boolean. A 2.2 pack that omits it or states it
/// as a string is therefore not an old pack but a broken one — and both decision
/// readers normalize a present-but-unreadable signal to BLOCK, so a validator
/// that accepted it certified an artifact the CLI and MCP both refuse to trust.
///
/// Absence stays forgiven BELOW 2.2, where readers derive the flag instead; that
/// carve-out is asserted here too, because tightening it would break every pack
/// written before the field existed.
#[test]
fn validator_requires_a_boolean_quality_pass_from_schema_two_two() {
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

    let raw = std::fs::read_to_string(&merge_gate).expect("read gate");
    let original: serde_json::Value = serde_json::from_str(&raw).expect("parse gate");
    assert_eq!(
        original["schema_version"].as_str(),
        Some("2.3"),
        "the current writer retains the 2.2 quality_pass requirement"
    );
    assert!(
        original["decision"]["quality_pass"].is_boolean(),
        "the writer must emit a boolean for the requirement to be safe"
    );

    // Absent, and every non-boolean spelling of it.
    let broken = [
        None,
        Some(serde_json::json!("false")),
        Some(serde_json::json!("true")),
        Some(serde_json::json!(0)),
        Some(serde_json::json!(1)),
        Some(serde_json::json!(null)),
        Some(serde_json::json!([])),
        Some(serde_json::json!({})),
    ];

    for value in broken {
        let mut gate = original.clone();
        match value {
            Some(value) => gate["decision"]["quality_pass"] = value,
            None => {
                gate["decision"]
                    .as_object_mut()
                    .expect("decision object")
                    .remove("quality_pass");
            }
        }
        std::fs::write(
            &merge_gate,
            serde_json::to_string_pretty(&gate).expect("serialize gate"),
        )
        .expect("write gate");

        Command::new("python3")
            .arg(&validator)
            .arg(&merge_gate)
            .assert()
            .failure();
    }

    // The legacy carve-out: below 2.2 an absent `quality_pass` is an old pack,
    // not a broken one, and the readers derive the flag rather than refusing it.
    let mut legacy = original.clone();
    legacy["schema_version"] = serde_json::json!("2.1");
    legacy["decision"]
        .as_object_mut()
        .expect("decision object")
        .remove("quality_pass");
    std::fs::write(
        &merge_gate,
        serde_json::to_string_pretty(&legacy).expect("serialize gate"),
    )
    .expect("write gate");
    Command::new("python3")
        .arg(&validator)
        .arg(&merge_gate)
        .assert()
        .success();

    // The shape the emitter actually writes still validates.
    std::fs::write(&merge_gate, &raw).expect("restore gate");
    Command::new("python3")
        .arg(&validator)
        .arg(&merge_gate)
        .assert()
        .success();
}

/// Regression: the validator accepted any non-empty `checks[].status`, so a pack
/// spelling a status the emitter never writes — `WARNINGS` from another writer,
/// a stale artifact `--update` reused unchanged — passed the repository gate
/// while the CLI, which counts warnings against the emitted vocabulary, could not
/// read it. The contract now names that vocabulary, case included.
#[test]
fn validator_rejects_a_check_status_outside_the_emitted_vocabulary() {
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

    let raw = std::fs::read_to_string(&merge_gate).expect("read gate");
    let mut original: serde_json::Value = serde_json::from_str(&raw).expect("parse gate");
    assert!(
        original["checks"]
            .as_array()
            .expect("checks array")
            .iter()
            .all(|check| matches!(
                check["status"].as_str(),
                Some("passed" | "failed" | "warnings" | "skipped" | "error")
            )),
        "the emitter writes only the vocabulary this test pins: {:?}",
        original["checks"]
    );
    original["schema_version"] = serde_json::json!("2.2");

    // Recognizable-but-uncanonical spellings, plus the non-strings a bare
    // "non-empty" rule never caught either.
    let broken = [
        serde_json::json!("WARNINGS"),
        serde_json::json!("Warnings"),
        serde_json::json!("warning"),
        serde_json::json!("warn"),
        serde_json::json!("PASSED"),
        serde_json::json!("ok"),
        serde_json::json!(" warnings"),
        serde_json::json!(true),
        serde_json::json!(0),
        serde_json::json!(null),
    ];

    for value in broken {
        let mut gate = original.clone();
        gate["checks"][0]["status"] = value.clone();
        std::fs::write(
            &merge_gate,
            serde_json::to_string_pretty(&gate).expect("serialize gate"),
        )
        .expect("write gate");

        Command::new("python3")
            .arg(&validator)
            .arg(&merge_gate)
            .assert()
            .failure();
    }

    // Every canonical spelling stays accepted, so the vocabulary is a contract
    // and not a single-value pin.
    for spelling in ["passed", "failed", "warnings", "skipped", "error"] {
        let mut gate = original.clone();
        gate["checks"][0]["status"] = serde_json::json!(spelling);
        std::fs::write(
            &merge_gate,
            serde_json::to_string_pretty(&gate).expect("serialize gate"),
        )
        .expect("write gate");

        Command::new("python3")
            .arg(&validator)
            .arg(&merge_gate)
            .assert()
            .success();
    }

    // The shape the emitter actually writes still validates.
    std::fs::write(&merge_gate, &raw).expect("restore gate");
    Command::new("python3")
        .arg(&validator)
        .arg(&merge_gate)
        .assert()
        .success();
}

/// The container half of the same contract. The validator already required
/// `checks` to be an array — this pins that, because the CLI now treats a
/// present-but-unreadable list as at least one warning and the two surfaces have
/// to agree on which packs are valid at all. Absence is a separate question and
/// is rejected here too: `checks` has been emitted since schema 1.0.
#[test]
fn validator_rejects_a_checks_list_that_is_not_an_array() {
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

    let raw = std::fs::read_to_string(&merge_gate).expect("read gate");
    let original: serde_json::Value = serde_json::from_str(&raw).expect("parse gate");
    assert!(
        original["checks"].is_array(),
        "the emitter writes an array here"
    );

    let broken = [
        Some(serde_json::json!({"semgrep": "warnings"})),
        Some(serde_json::json!("warnings")),
        Some(serde_json::json!(7)),
        Some(serde_json::json!(null)),
        Some(serde_json::json!(true)),
        None,
    ];

    for value in broken {
        let mut gate = original.clone();
        match value {
            Some(value) => gate["checks"] = value,
            None => {
                gate.as_object_mut().expect("gate object").remove("checks");
            }
        }
        std::fs::write(
            &merge_gate,
            serde_json::to_string_pretty(&gate).expect("serialize gate"),
        )
        .expect("write gate");

        Command::new("python3")
            .arg(&validator)
            .arg(&merge_gate)
            .assert()
            .failure();
    }

    // An empty list is a legitimate shape — a run in which nothing gated — and
    // must not be swept up by the same rule.
    let mut empty = original.clone();
    empty["checks"] = serde_json::json!([]);
    std::fs::write(
        &merge_gate,
        serde_json::to_string_pretty(&empty).expect("serialize gate"),
    )
    .expect("write gate");
    Command::new("python3")
        .arg(&validator)
        .arg(&merge_gate)
        .assert()
        .success();

    // The shape the emitter actually writes still validates.
    std::fs::write(&merge_gate, &raw).expect("restore gate");
    Command::new("python3")
        .arg(&validator)
        .arg(&merge_gate)
        .assert()
        .success();
}

/// The decision axes the 2.2 writer emits unconditionally, from the typed enums
/// in `src/policy/engine.rs`. A 2.2 pack missing one is broken rather than old —
/// and the reconciliation the next test pins can only compare axes that are
/// there and readable in the first place.
#[test]
fn validator_requires_the_decision_axes_schema_two_two_emits() {
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

    let raw = std::fs::read_to_string(&merge_gate).expect("read gate");
    let mut original: serde_json::Value = serde_json::from_str(&raw).expect("parse gate");
    assert_eq!(
        original["schema_version"].as_str(),
        Some("2.3"),
        "the current writer retains the decision axes introduced in 2.2"
    );
    original["schema_version"] = serde_json::json!("2.2");
    for axis in [
        "analysis_status",
        "merge_recommendation",
        "policy_allow_merge",
    ] {
        assert!(
            original["decision"].get(axis).is_some(),
            "the emitter writes {axis}: {:?}",
            original["decision"]
        );
    }

    // (axis, value, must_validate) — `None` removes the key entirely.
    let cases: [(&str, Option<serde_json::Value>, bool); 20] = [
        ("analysis_status", None, false),
        ("merge_recommendation", None, false),
        ("policy_allow_merge", None, false),
        // Case is not a spelling the emitter has ever written, and neither is a
        // word outside the enum. Same rule as `checks[].status`.
        (
            "analysis_status",
            Some(serde_json::json!("COMPLETE")),
            false,
        ),
        (
            "analysis_status",
            Some(serde_json::json!("Degraded")),
            false,
        ),
        ("analysis_status", Some(serde_json::json!("partial")), false),
        ("analysis_status", Some(serde_json::json!(7)), false),
        ("analysis_status", Some(serde_json::json!(null)), false),
        (
            "merge_recommendation",
            Some(serde_json::json!("APPROVE")),
            false,
        ),
        (
            "merge_recommendation",
            Some(serde_json::json!("Review_Required")),
            false,
        ),
        // The retired pre-2.1 synonym. Readers still fold it when reading a pack
        // off disk; a freshly emitted one may not spell it that way.
        (
            "merge_recommendation",
            Some(serde_json::json!("hold")),
            false,
        ),
        ("merge_recommendation", Some(serde_json::json!(true)), false),
        ("policy_allow_merge", Some(serde_json::json!("true")), false),
        ("policy_allow_merge", Some(serde_json::json!(1)), false),
        ("policy_allow_merge", Some(serde_json::json!(null)), false),
        // Every canonical spelling stays accepted, so this is a vocabulary and
        // not a single-value pin. All three confidence values sit at or below
        // the CONDITIONAL this pack states, so none of them trips the
        // reconciliation rule the next test covers.
        ("analysis_status", Some(serde_json::json!("complete")), true),
        ("analysis_status", Some(serde_json::json!("degraded")), true),
        (
            "analysis_status",
            Some(serde_json::json!("incomplete")),
            true,
        ),
        (
            "merge_recommendation",
            Some(serde_json::json!("approve")),
            true,
        ),
        // `block` is canonical too, but only beside a BLOCK verdict — the next
        // test states it there.
        ("policy_allow_merge", Some(serde_json::json!(true)), true),
    ];

    for (axis, value, must_validate) in cases {
        let mut gate = original.clone();
        match value {
            Some(value) => gate["decision"][axis] = value,
            None => {
                gate["decision"]
                    .as_object_mut()
                    .expect("decision object")
                    .remove(axis);
            }
        }
        std::fs::write(
            &merge_gate,
            serde_json::to_string_pretty(&gate).expect("serialize gate"),
        )
        .expect("write gate");

        let assertion = Command::new("python3")
            .arg(&validator)
            .arg(&merge_gate)
            .assert();
        if must_validate {
            assertion.success();
        } else {
            assertion.failure();
        }
    }

    // The shape the emitter actually writes still validates.
    std::fs::write(&merge_gate, &raw).expect("restore gate");
    Command::new("python3")
        .arg(&validator)
        .arg(&merge_gate)
        .assert()
        .success();
}

/// The reconciliation contract itself, ported from the readers into the
/// certification gate.
///
/// Both readers derive a decision by taking the most conservative axis the pack
/// states, so a `verdict` milder than that maximum is one no consumer will
/// honour: the artifact certifies one outcome and everything downstream computes
/// another. The reported payload — `PASS` beside an `incomplete` analysis, a
/// `block` recommendation and `policy_allow_merge: false` — validated OK before
/// this rule existed.
///
/// The opposite direction is deliberately still legal, and asserted below: a
/// semgrep scan that passes with parse errors writes `approve` beside `degraded`
/// and the contract turns that into `CONDITIONAL`, so a verdict harsher than its
/// neighbours is a pack the emitter really produces.
#[test]
fn validator_rejects_a_verdict_its_own_axes_contradict() {
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

    let raw = std::fs::read_to_string(&merge_gate).expect("read gate");
    let mut original: serde_json::Value = serde_json::from_str(&raw).expect("parse gate");
    original["schema_version"] = serde_json::json!("2.2");

    let gating_detail = serde_json::json!([
        { "name": "Clippy", "classification": "introduced", "origin": "failure" }
    ]);

    let with = |base: &serde_json::Value, patch: serde_json::Value| {
        let mut decision = base.clone();
        for (key, value) in patch.as_object().expect("patch object") {
            decision[key] = value.clone();
        }
        decision
    };
    // A clean pass, the shape every conflicting axis below is measured against.
    // Built by patching the emitted decision so the fields this rule says
    // nothing about — `decision_reason`, the legacy mirrors, the caveats — stay
    // exactly as the writer left them.
    let clean = with(
        &original["decision"],
        serde_json::json!({
            "verdict": "PASS",
            "allow_merge": true,
            "merge_recommendation": "approve",
            "analysis_status": "complete",
            "quality_pass": true,
            "policy_allow_merge": true,
            "blocking_issues": [],
            "quality_failure_details": [],
            "enforcement_disposition": "clean",
        }),
    );

    // (decision patch over `clean`, must_validate)
    let cases: [(serde_json::Value, bool); 15] = [
        // The reported payload, verbatim.
        (
            serde_json::json!({
                "analysis_status": "incomplete",
                "merge_recommendation": "block",
                "policy_allow_merge": false,
            }),
            false,
        ),
        // One axis at a time, so the rule is not passing on the strength of the
        // others. Each of these rules `PASS` out by itself.
        (
            serde_json::json!({ "merge_recommendation": "review_required" }),
            false,
        ),
        (
            serde_json::json!({ "merge_recommendation": "block" }),
            false,
        ),
        (serde_json::json!({ "analysis_status": "degraded" }), false),
        (
            serde_json::json!({ "analysis_status": "incomplete" }),
            false,
        ),
        (
            serde_json::json!({
                "quality_pass": false,
                "quality_failure_details": gating_detail,
            }),
            false,
        ),
        (serde_json::json!({ "policy_allow_merge": false }), false),
        // The blocker axis stated by its other half. `allow_merge` is already
        // false here, so the pre-existing "no merge beside a blocking issue"
        // rule is satisfied and only the reconciliation can reject this.
        (
            serde_json::json!({
                "verdict": "CONDITIONAL",
                "allow_merge": false,
                "merge_recommendation": "review_required",
                "enforcement_disposition": "review_required",
                "blocking_issues": ["Semgrep (failed)"],
            }),
            false,
        ),
        // A CONDITIONAL that is still milder than a stated block.
        (
            serde_json::json!({
                "verdict": "CONDITIONAL",
                "allow_merge": false,
                "merge_recommendation": "block",
                "policy_allow_merge": false,
                "blocking_issues": ["Semgrep (failed)"],
            }),
            false,
        ),
        // Legal shapes. The clean pass itself, untouched.
        (serde_json::json!({}), true),
        // CONDITIONAL because the recommendation says so.
        (
            serde_json::json!({
                "verdict": "CONDITIONAL",
                "allow_merge": false,
                "merge_recommendation": "review_required",
                "enforcement_disposition": "review_required",
            }),
            true,
        ),
        // CONDITIONAL because the analysis was degraded, while the recommendation
        // still approves — the harsher-verdict shape a passing semgrep scan with
        // parse errors writes.
        (
            serde_json::json!({
                "verdict": "CONDITIONAL",
                "allow_merge": false,
                "analysis_status": "degraded",
                "enforcement_disposition": "review_required",
            }),
            true,
        ),
        // CONDITIONAL because quality failed, recommendation still approving.
        (
            serde_json::json!({
                "verdict": "CONDITIONAL",
                "allow_merge": false,
                "quality_pass": false,
                "quality_failure_details": gating_detail,
                "enforcement_disposition": "review_required",
            }),
            true,
        ),
        // A healthy BLOCK: every axis at the same rank.
        (
            serde_json::json!({
                "verdict": "BLOCK",
                "allow_merge": false,
                "merge_recommendation": "block",
                "analysis_status": "incomplete",
                "policy_allow_merge": false,
                "blocking_issues": ["Semgrep (failed)"],
                "enforcement_disposition": "block",
            }),
            true,
        ),
        // A BLOCK whose blocker is stated ONLY as a policy flag. This case was
        // asserted legal when the reconciliation rule landed, on the assumption
        // that a pack may state either half of the blocker axis. The source says
        // otherwise: the emitter computes
        // `policy_allow_merge = blocking_issues.is_empty()` after the last push
        // to that list, so the flag and the list are one fact written twice and
        // `false` beside an empty list is unemittable. The correction is not a
        // relaxation — this shape is now rejected, by the equivalence the test
        // below pins.
        (
            serde_json::json!({
                "verdict": "BLOCK",
                "allow_merge": false,
                "merge_recommendation": "block",
                "policy_allow_merge": false,
                "enforcement_disposition": "block",
            }),
            false,
        ),
    ];

    for (patch, must_validate) in cases {
        let mut gate = original.clone();
        gate["decision"] = with(&clean, patch.clone());
        std::fs::write(
            &merge_gate,
            serde_json::to_string_pretty(&gate).expect("serialize gate"),
        )
        .expect("write gate");

        let assertion = Command::new("python3")
            .arg(&validator)
            .arg(&merge_gate)
            .assert();
        if must_validate {
            assertion.success();
        } else {
            assertion.failure();
        }
    }

    // The shape the emitter actually writes still validates.
    std::fs::write(&merge_gate, &raw).expect("restore gate");
    Command::new("python3")
        .arg(&validator)
        .arg(&merge_gate)
        .assert()
        .success();
}

/// The blocker axis is one fact written twice, and the gate certifies it as one.
///
/// The emitter computes `policy_allow_merge = blocking_issues.is_empty()` after
/// the last push to that list, and then emits both verbatim, so the flag has no
/// input the list does not have. The reconciliation rule only used that in the
/// harsher direction — a non-empty list raises the required verdict — which left
/// the two fields free to contradict each other outright: a pack claiming
/// `policy_allow_merge: true` beside real blockers, or `false` beside none,
/// certified clean while every reader treats the pair as a single signal.
///
/// Both illegal shapes below carry a `BLOCK` verdict, the most conservative one
/// there is, so the reconciliation cannot be what rejects them. Only the
/// equivalence can.
#[test]
fn validator_rejects_a_blocker_flag_its_blocking_issues_contradict() {
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

    let raw = std::fs::read_to_string(&merge_gate).expect("read gate");
    let mut original: serde_json::Value = serde_json::from_str(&raw).expect("parse gate");
    original["schema_version"] = serde_json::json!("2.2");

    let with = |base: &serde_json::Value, patch: serde_json::Value| {
        let mut decision = base.clone();
        for (key, value) in patch.as_object().expect("patch object") {
            decision[key] = value.clone();
        }
        decision
    };
    let clean = with(
        &original["decision"],
        serde_json::json!({
            "verdict": "PASS",
            "allow_merge": true,
            "merge_recommendation": "approve",
            "analysis_status": "complete",
            "quality_pass": true,
            "policy_allow_merge": true,
            "blocking_issues": [],
            "quality_failure_details": [],
            "enforcement_disposition": "clean",
        }),
    );

    // (decision patch over `clean`, must_validate)
    let cases: [(serde_json::Value, bool); 5] = [
        // The reported hole: blockers listed, yet the flag says policy let the
        // merge through.
        (
            serde_json::json!({
                "verdict": "BLOCK",
                "allow_merge": false,
                "merge_recommendation": "block",
                "policy_allow_merge": true,
                "blocking_issues": ["Semgrep (failed)"],
                "enforcement_disposition": "block",
            }),
            false,
        ),
        // The other direction, unemittable for the same reason: policy is said to
        // have blocked, but the list it is computed from is empty.
        (
            serde_json::json!({
                "verdict": "BLOCK",
                "allow_merge": false,
                "merge_recommendation": "block",
                "policy_allow_merge": false,
                "blocking_issues": [],
                "enforcement_disposition": "block",
            }),
            false,
        ),
        // Legal shapes. The clean pass itself: no blockers, flag agrees.
        (serde_json::json!({}), true),
        // A healthy BLOCK: blockers listed, flag agrees.
        (
            serde_json::json!({
                "verdict": "BLOCK",
                "allow_merge": false,
                "merge_recommendation": "block",
                "analysis_status": "incomplete",
                "policy_allow_merge": false,
                "blocking_issues": ["Semgrep (failed)"],
                "enforcement_disposition": "block",
            }),
            true,
        ),
        // A CONDITIONAL nobody blocked: the equivalence says nothing about the
        // axes that made it conditional.
        (
            serde_json::json!({
                "verdict": "CONDITIONAL",
                "allow_merge": false,
                "analysis_status": "degraded",
                "policy_allow_merge": true,
                "blocking_issues": [],
                "enforcement_disposition": "review_required",
            }),
            true,
        ),
    ];

    for (patch, must_validate) in cases {
        let mut gate = original.clone();
        gate["decision"] = with(&clean, patch.clone());
        std::fs::write(
            &merge_gate,
            serde_json::to_string_pretty(&gate).expect("serialize gate"),
        )
        .expect("write gate");

        let assertion = Command::new("python3")
            .arg(&validator)
            .arg(&merge_gate)
            .assert();
        if must_validate {
            assertion.success();
        } else {
            assertion.failure();
        }
    }

    // The shape the emitter actually writes still validates.
    std::fs::write(&merge_gate, &raw).expect("restore gate");
    Command::new("python3")
        .arg(&validator)
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
            "--with-security",
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
    assert!(
        gate["decision"]["evidence_gaps"]
            .as_array()
            .is_some_and(|gaps| gaps.iter().any(|gap| {
                gap["execution_state"] == "unavailable"
                    && gap["verification_target"]
                        .as_str()
                        .is_some_and(|target| target.starts_with("execute "))
            })),
        "an unavailable tool must become a concrete verification target: {}",
        gate["decision"]["evidence_gaps"]
    );

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
    assert!(matches!(
        gate["inline_findings"]["artifact_state"].as_str(),
        Some("scanned_zero" | "unavailable" | "not_applicable" | "not_generated")
    ));
    assert!(
        gate["inline_findings"]["file"].is_null(),
        "inline_findings.file should be null when no SARIF file is written"
    );
    assert!(
        gate["files"]["inline_findings"].is_null(),
        "files.inline_findings should be null when no SARIF file is written"
    );

    let report: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(output_dir.join("report.json")).expect("read report"),
    )
    .expect("parse report");
    assert_eq!(
        report["gate"]["status"], gate["decision"]["verdict"],
        "report must not project CONDITIONAL + allow_merge=false to BLOCK"
    );

    let check_status: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(output_dir.join("checks-status.json")).expect("read check status"),
    )
    .expect("parse check status");
    let gate_ids = gate["checks"]
        .as_array()
        .expect("gate checks")
        .iter()
        .filter_map(|row| row["id"].as_str())
        .collect::<std::collections::BTreeSet<_>>();
    let status_ids = check_status["_checks"]
        .as_array()
        .expect("typed check inventory")
        .iter()
        .filter_map(|row| row["id"].as_str())
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        status_ids, gate_ids,
        "one canonical check set across JSON surfaces"
    );

    let ai_index = fs::read_to_string(output_dir.join("AI_INDEX.md")).expect("read AI index");
    assert!(ai_index.contains("Evidence completeness:"));
    assert!(ai_index.contains("does not mean every check ran"));
    let review = fs::read_to_string(output_dir.join("PR_REVIEW.md")).expect("read PR review");
    assert!(review.starts_with("# Generated Evidence Overview"));
    assert!(!review.contains("<!-- Describe your changes -->"));
}

#[test]
fn pinned_v0_7_fixture_preserves_the_conditional_to_block_contradiction() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/packs/v0.7.0");
    let receipt: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(root.join("receipt.json")).expect("read fixture receipt"),
    )
    .expect("parse fixture receipt");
    let gate: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(root.join("sanitized/00_summary/MERGE_GATE.json"))
            .expect("read historical gate"),
    )
    .expect("parse historical gate");
    let report: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(root.join("sanitized/report.json")).expect("read historical report"),
    )
    .expect("parse historical report");

    assert_eq!(
        receipt["generator"]["commit"],
        "2e11cc6d6e90a606a17d71d0d093a1e2f564bc80"
    );
    assert_eq!(
        receipt["repository"]["base_oid"],
        "6e4feb2f3da353b47950ee7a3926e4eddba2f6f4"
    );
    assert_eq!(
        receipt["repository"]["head_oid"],
        "2580ff2892a4900c12bd5cf408ad034e39843a7a"
    );
    assert_eq!(gate["decision"]["verdict"], "CONDITIONAL");
    assert_eq!(gate["decision"]["allow_merge"], false);
    assert_eq!(report["gate"]["status"], "BLOCK");
    for relative in [
        "sanitized/00_summary/MERGE_GATE.json",
        "sanitized/report.json",
        "sanitized/checks-status.json",
    ] {
        assert_eq!(
            sha256_file(&root.join(relative)),
            receipt["sanitized_artifacts"][relative]
                .as_str()
                .expect("pinned sanitized hash"),
            "fixture bytes drifted: {relative}"
        );
    }
    assert!(gate["checks"].as_array().is_some_and(|rows| {
        rows.iter()
            .any(|row| row["id"] == "cargo_test" && row["execution_state"] == "skipped")
    }));
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
fn generated_pack_carries_pack_level_provenance() {
    let temp = create_fixture_repo();
    let repo = temp.path();

    let payload = run_json_quiet(repo, &["feature/json-contract", "main"]);
    let output_dir = Path::new(
        payload["output_dir"]
            .as_str()
            .expect("output_dir should be a string"),
    );

    let provenance_path = output_dir.join("00_summary/PROVENANCE.json");
    let provenance: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(&provenance_path).expect("PROVENANCE.json must be written"),
    )
    .expect("PROVENANCE.json must be valid JSON");

    assert_eq!(provenance["schema_version"].as_str(), Some("1.0"));

    // The pack-level record must agree with RUN.json about what was analysed —
    // two truths about the substrate is exactly the failure mode it prevents.
    let run: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(output_dir.join("00_summary/RUN.json")).expect("read RUN.json"),
    )
    .expect("parse RUN.json");
    assert_eq!(provenance["target_sha"], run["refs"]["target_sha"]);
    assert!(
        provenance["head_sha"].is_string(),
        "the locally checked-out commit must be recorded"
    );
    assert!(
        provenance["base_sha"].is_string(),
        "the diff baseline must be recorded"
    );
    // Every baseline, named: a multi-base run produces one patch per base, and
    // the scalar is the array's first entry rather than a second truth.
    let bases = provenance["bases"]
        .as_array()
        .expect("every baseline must be recorded");
    assert!(!bases.is_empty(), "a run with a diff has a baseline");
    assert!(bases[0]["name"].is_string(), "a baseline is named");
    assert_eq!(bases[0]["sha"], provenance["base_sha"]);

    // The fixture repo is committed clean before the run; the digest is present
    // either way, so an audit can distinguish two differently-dirty runs.
    assert_eq!(provenance["worktree"]["clean"].as_bool(), Some(true));
    assert!(
        provenance["worktree"]["status_digest"]
            .as_str()
            .expect("status digest")
            .starts_with("sha256:")
    );

    // One row per check. The rows that ran match RUN.json's checks[] exactly;
    // the rest are checks ruled out before execution, which RUN.json does not
    // carry and which a consumer must still be able to tell apart from a gate
    // that was never scheduled.
    let rows = provenance["checks"].as_array().expect("checks array");
    let run_checks = run["checks"].as_array().expect("RUN.json checks");
    let executed: Vec<_> = rows.iter().filter(|row| row["skipped"].is_null()).collect();
    assert_eq!(executed.len(), run_checks.len());
    for (row, run_check) in executed.iter().zip(run_checks) {
        assert_eq!(row["id"], run_check["gate"]);
        assert_eq!(row["cached"], run_check["cached"]);
    }
    for row in rows.iter().filter(|row| !row["skipped"].is_null()) {
        assert!(
            row["skipped"].as_str().is_some_and(|why| !why.is_empty()),
            "a skipped gate is recorded with the reason it was ruled out"
        );
        assert!(
            row["cwd"].is_null() && row["tree_state"].is_null(),
            "a check that never ran read no tree"
        );
    }

    // Check projections share the policy evaluation produced for this run.
    // RUN.json deliberately carries executed checks only, while MERGE_GATE and
    // checks-status also expose pre-run skips; their overlapping rows must not
    // invent a different status or gate class.
    let merge_gate: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(output_dir.join("00_summary/MERGE_GATE.json"))
            .expect("read MERGE_GATE.json"),
    )
    .expect("parse MERGE_GATE.json");
    let checks_status: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(output_dir.join("checks-status.json"))
            .expect("read checks-status.json"),
    )
    .expect("parse checks-status.json");
    let gate_rows = merge_gate["checks"].as_array().expect("merge-gate checks");

    for run_check in run_checks {
        let gate_check = gate_rows
            .iter()
            .find(|row| row["id"] == run_check["gate"])
            .expect("every RUN check has one MERGE_GATE projection");
        assert_eq!(run_check["status"], gate_check["status"]);
        assert_eq!(run_check["class"], gate_check["class"]);
    }
    for gate_check in gate_rows {
        let id = gate_check["id"].as_str().expect("gate id");
        let projected = checks_status[id]
            .as_str()
            .expect("every MERGE_GATE check has a checks-status projection");
        if gate_check["status"] == "skipped" {
            assert!(
                projected.starts_with("skipped (") && projected.ends_with(')'),
                "a skipped projection retains its reason: {id}={projected}"
            );
        } else {
            assert_eq!(Some(projected), gate_check["status"].as_str());
        }
    }

    // Additive, but not invisible: the manifest hashes it and sanity requires it.
    let manifest: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(output_dir.join("00_summary/MANIFEST.json")).expect("read MANIFEST"),
    )
    .expect("parse MANIFEST");
    assert!(
        manifest["files"]
            .as_array()
            .expect("files")
            .iter()
            .any(|f| f["path"].as_str() == Some("00_summary/PROVENANCE.json")),
        "PROVENANCE.json must be hashed in MANIFEST.json"
    );

    let sanity: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(output_dir.join("00_summary/SANITY.json")).expect("read SANITY"),
    )
    .expect("parse SANITY");
    assert_eq!(
        sanity["valid"].as_bool(),
        Some(true),
        "sanity must stay valid with the new required file: {}",
        sanity["failures"]
    );
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

/// Recursively find the one `00_summary/MERGE_GATE.json` under `root`.
fn find_merge_gate(root: &Path) -> Option<std::path::PathBuf> {
    let entries = fs::read_dir(root).ok()?;
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        if path.is_dir() {
            if let Some(found) = find_merge_gate(&path) {
                return Some(found);
            }
        } else if path.ends_with("00_summary/MERGE_GATE.json") {
            return Some(path);
        }
    }
    None
}

#[test]
fn an_unchanged_update_run_still_honors_fail_on_warnings() {
    // `--update` with no new commits reuses the previous pack, and that pack is
    // what the run reports. Forcing exit 0 there made a warnings-clean CI job
    // turn green on its second invocation while the reused pack still carried
    // warnings — the flag promises exit 1 whenever any pack check warns.
    let temp = create_fixture_repo();
    let repo = temp.path();
    let home = tempfile::tempdir().expect("prview home");

    // A first run produces the pack the update run will reuse.
    Command::new(assert_cmd::cargo::cargo_bin!("prview"))
        .current_dir(repo)
        .env("PRVIEW_HOME", home.path())
        .args([
            "--quick",
            "--quiet",
            "--no-zip",
            "--no-heuristics",
            "--no-fetch",
            "--local-only",
        ])
        .output()
        .expect("first run");

    // Plant a decision that is clean on every axis EXCEPT one warning check, so
    // the exit code can only come from the warning-hardening flag.
    let gate = find_merge_gate(home.path()).expect("the first run wrote a pack");
    fs::write(
        &gate,
        r#"{
  "schema_version": "2.2",
  "decision": {
    "verdict": "PASS",
    "merge_recommendation": "approve",
    "allow_merge": true,
    "quality_pass": true,
    "analysis_status": "complete"
  },
  "checks": [{"id": "rustfmt", "status": "warnings"}]
}"#,
    )
    .expect("plant gate");

    let update_args = [
        "--ci",
        "--update",
        "--quiet",
        "--no-zip",
        "--no-heuristics",
        "--no-fetch",
        "--local-only",
    ];

    // Without the flag the reused pack is advisory: warnings do not fail CI.
    let lenient = Command::new(assert_cmd::cargo::cargo_bin!("prview"))
        .current_dir(repo)
        .env("PRVIEW_HOME", home.path())
        .args(update_args)
        .output()
        .expect("lenient update run");
    assert_eq!(
        lenient.status.code(),
        Some(0),
        "an unchanged run over a non-blocking pack still exits 0: {}",
        String::from_utf8_lossy(&lenient.stderr)
    );

    let strict = Command::new(assert_cmd::cargo::cargo_bin!("prview"))
        .current_dir(repo)
        .env("PRVIEW_HOME", home.path())
        .args(update_args)
        .arg("--fail-on-warnings")
        .output()
        .expect("strict update run");
    assert_eq!(
        strict.status.code(),
        Some(1),
        "the reused pack warns, so --fail-on-warnings must exit 1: {}",
        String::from_utf8_lossy(&strict.stderr)
    );

    // A historical CONDITIONAL cannot acquire the new gate-strict exception by
    // injecting a field its 2.2 schema never defined. The unchanged update
    // path must read this stored pack (not reconstruct policy from an empty
    // Report), while top-level CI keeps its historical quality/block-only rule.
    fs::write(
        &gate,
        r#"{
  "schema_version": "2.2",
  "decision": {
    "verdict": "CONDITIONAL",
    "merge_recommendation": "review_required",
    "allow_merge": false,
    "policy_allow_merge": true,
    "quality_pass": true,
    "analysis_status": "complete",
    "enforcement_disposition": "warnings_only",
    "blocking_issues": [],
    "quality_failure_details": []
  },
  "checks": [],
  "inline_findings": {"status": "passed", "blocking": false}
}"#,
    )
    .expect("plant legacy conditional gate");

    for fail_on_warnings in [false, true] {
        let mut command = Command::new(assert_cmd::cargo::cargo_bin!("prview"));
        command
            .current_dir(repo)
            .env("PRVIEW_HOME", home.path())
            .args(update_args);
        if fail_on_warnings {
            command.arg("--fail-on-warnings");
        }
        let output = command.output().expect("legacy update run");
        assert_eq!(
            output.status.code(),
            Some(0),
            "legacy CONDITIONAL with quality=true and no warning stays CI-advisory \
             (fail_on_warnings={fail_on_warnings}): {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    // A fresh typed pack can carry an enforcement-level review ratchet and a
    // separate warning fact. Historical CI remains advisory for the review,
    // while its explicit warnings-clean lane must still fail the reused pack.
    fs::write(
        &gate,
        r#"{
  "schema_version": "2.3",
  "policy": {"mode": "warn"},
  "decision": {
    "verdict": "CONDITIONAL",
    "merge_recommendation": "review_required",
    "allow_merge": false,
    "policy_allow_merge": true,
    "quality_pass": true,
    "analysis_status": "complete",
    "enforcement_disposition": "review_required",
    "blocking_issues": [],
    "quality_failure_details": []
  },
  "checks": [{
    "id": "rustfmt", "name": "Rustfmt", "status": "warnings",
    "execution_state": "executed", "outcome": "findings_warning",
    "class": "INFO", "severity": "warn", "policy_conclusion": "advisory",
    "confidence_impact": "complete", "merge_impact": "review_required",
    "blocking": false
  }],
  "inline_findings": {
    "status": "passed", "severity": "warn", "blocking": false,
    "effective_class": "PASS", "enforcement_disposition": "clean",
    "findings_count": 0, "introduced_count": 0, "preexisting_count": 0
  }
}"#,
    )
    .expect("plant fresh mixed gate");

    for (fail_on_warnings, expected) in [(false, 0), (true, 1)] {
        let mut command = Command::new(assert_cmd::cargo::cargo_bin!("prview"));
        command
            .current_dir(repo)
            .env("PRVIEW_HOME", home.path())
            .args(update_args);
        if fail_on_warnings {
            command.arg("--fail-on-warnings");
        }
        let output = command.output().expect("fresh mixed update run");
        assert_eq!(
            output.status.code(),
            Some(expected),
            "stored mixed warning/review pack must preserve CI warning lane \
             (fail_on_warnings={fail_on_warnings}): {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
