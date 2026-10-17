//! A target-tree check must not masquerade as review of an explicit self-diff.
mod support;

use prview::git::git_cmd;
use serde_json::Value;
use std::{fs, path::Path};
use tempfile::TempDir;

fn git(repo: &Path, args: &[&str]) {
    let output = git_cmd()
        .args(["-c", "commit.gpgsign=false"])
        .args(args)
        .current_dir(repo)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn fixture() -> TempDir {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path();
    git(repo, &["init"]);
    git(repo, &["config", "user.name", "Range Test"]);
    git(repo, &["config", "user.email", "range@example.test"]);
    fs::write(repo.join("README.md"), "base\n").unwrap();
    fs::write(
        repo.join(".prview-policy.yml"),
        "version: 1\nmode: warn\ndefault_severity: ignore\n",
    )
    .unwrap();
    git(repo, &["add", "."]);
    git(repo, &["commit", "-m", "base"]);
    git(repo, &["branch", "-M", "main"]);
    git(repo, &["checkout", "-b", "feature"]);
    fs::write(repo.join("README.md"), "base\nchange\n").unwrap();
    git(repo, &["commit", "-am", "change"]);
    git(repo, &["branch", "self-alias"]);
    dir
}

fn run(
    env: &support::ContractEnvironment,
    repo: &Path,
    args: &[&str],
    code: i32,
) -> (Value, Value, Value) {
    let output = env.command().current_dir(repo).args(args).output().unwrap();
    assert_eq!(
        output.status.code(),
        Some(code),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let cli: Value = serde_json::from_slice(&output.stdout).unwrap();
    let pack = Path::new(cli["output_dir"].as_str().unwrap());
    let read = |file: &str| -> Value {
        serde_json::from_slice(&fs::read(pack.join(file)).unwrap()).unwrap()
    };
    let gate = read("00_summary/MERGE_GATE.json");
    let report = read("report.json");
    let provenance = read("00_summary/PROVENANCE.json");
    assert_eq!(gate["review_range"], report["review_range"]);
    assert_eq!(gate["review_range"], provenance["review_range"]);
    assert_eq!(gate["decision"]["verdict"], report["gate"]["status"]);
    let metadata = fs::read_to_string(pack.join("00_summary/pr-metadata.txt")).unwrap();
    assert!(
        metadata.contains("HEAD: "),
        "empty comparisons still retain target identity"
    );
    let narrative = fs::read_to_string(pack.join("PR_REVIEW.md")).unwrap();
    if gate["review_range"]["comparisons"]
        .as_array()
        .unwrap()
        .iter()
        .any(|c| c["status"] == "self_diff")
    {
        assert!(narrative.contains("No change reviewed"));
        assert!(
            gate["decision"]["review_caveats"]
                .as_array()
                .unwrap()
                .iter()
                .any(|c| c.as_str().unwrap().contains("No change reviewed"))
        );
    }
    (cli, gate, provenance)
}

#[test]
fn explicit_self_diff_is_conditional_in_cli_and_all_pack_surfaces() {
    let repo = fixture();
    let env = support::ContractEnvironment::new();
    let (cli, gate, _) = run(
        &env,
        repo.path(),
        &[
            "HEAD",
            "self-alias",
            "--quick",
            "--skip-security",
            "--json",
            "--no-fetch",
            "--no-zip",
        ],
        0,
    );
    assert_eq!(cli["verdict"], "CONDITIONAL");
    assert_eq!(gate["decision"]["analysis_status"], "incomplete");
    assert_eq!(
        gate["decision"]["enforcement_disposition"],
        "review_required"
    );
    assert_eq!(
        gate["review_range"]["comparisons"][0]["status"],
        "self_diff"
    );
}

#[test]
fn gate_self_diff_strict_exit_uses_existing_review_required_contract() {
    let repo = fixture();
    for (args, code) in [
        (vec!["gate", "--base", "HEAD", "--json"], 0),
        (
            vec!["gate", "--base", "self-alias", "--strict", "--json"],
            2,
        ),
        (
            vec![
                "gate",
                "--base",
                "HEAD",
                "--exact-base",
                "--strict",
                "--json",
            ],
            2,
        ),
    ] {
        let env = support::ContractEnvironment::new();
        let (cli, gate, _) = run(&env, repo.path(), &args, code);
        assert_eq!(cli["verdict"], "CONDITIONAL");
        assert_eq!(gate["decision"]["allow_merge"], false);
        assert_eq!(
            gate["decision"]["enforcement_disposition"],
            "review_required"
        );
    }
}

#[test]
fn auto_self_current_only_and_distinct_equal_trees_are_not_explicit_self_diff() {
    let repo = fixture();
    git(repo.path(), &["checkout", "main"]);
    let env = support::ContractEnvironment::new();
    let (_, gate, _) = run(&env, repo.path(), &["gate", "--json", "--strict"], 0);
    assert_eq!(gate["decision"]["verdict"], "PASS");
    assert_eq!(gate["review_range"]["explicit_bases"], false);
    let env = support::ContractEnvironment::new();
    let (_, gate, _) = run(
        &env,
        repo.path(),
        &[
            "--current-only",
            "--quick",
            "--skip-security",
            "--json",
            "--no-fetch",
            "--no-zip",
        ],
        0,
    );
    assert_eq!(gate["review_range"]["current_only"], true);
    assert!(
        gate["review_range"]["comparisons"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(gate["decision"]["verdict"], "PASS");
    git(repo.path(), &["commit", "--allow-empty", "-m", "same tree"]);
    let env = support::ContractEnvironment::new();
    let (_, gate, _) = run(
        &env,
        repo.path(),
        &["gate", "--base", "HEAD^", "--strict", "--json"],
        0,
    );
    assert_eq!(
        gate["review_range"]["comparisons"][0]["status"],
        "equal_trees"
    );
    assert_eq!(gate["decision"]["verdict"], "PASS");
}

#[test]
fn mixed_ranges_keep_the_real_diff_and_name_the_unreviewed_range() {
    let repo = fixture();
    let env = support::ContractEnvironment::new();
    let (_, gate, prov) = run(
        &env,
        repo.path(),
        &[
            "HEAD",
            "self-alias",
            "main",
            "--quick",
            "--skip-security",
            "--json",
            "--no-fetch",
            "--no-zip",
        ],
        0,
    );
    assert_eq!(
        gate["review_range"]["comparisons"][0]["status"],
        "self_diff"
    );
    assert_eq!(gate["review_range"]["comparisons"][1]["status"], "compared");
    assert_eq!(prov["bases"].as_array().unwrap().len(), 2);
    assert_eq!(gate["decision"]["verdict"], "CONDITIONAL");
}

#[test]
fn descendant_base_names_effective_target_without_claiming_explicit_self_diff() {
    let repo = fixture();
    git(repo.path(), &["checkout", "-b", "descendant"]);
    fs::write(repo.path().join("README.md"), "base\nchange\nnext\n").unwrap();
    git(repo.path(), &["commit", "-am", "next"]);
    git(repo.path(), &["checkout", "feature"]);
    let env = support::ContractEnvironment::new();
    let (_, gate, prov) = run(
        &env,
        repo.path(),
        &["gate", "--base", "descendant", "--strict", "--json"],
        0,
    );
    let range = &gate["review_range"]["comparisons"][0];
    assert_eq!(range["status"], "merge_base_equals_target");
    assert_ne!(range["requested_base_sha"], range["target_sha"]);
    assert_eq!(range["effective_base_sha"], range["target_sha"]);
    assert_eq!(prov["base_sha"], range["target_sha"]);
    assert_eq!(gate["decision"]["verdict"], "PASS");
}

#[test]
fn update_cannot_reuse_auto_pass_for_explicit_self_diff() {
    let repo = fixture();
    git(repo.path(), &["checkout", "main"]);
    let env = support::ContractEnvironment::new();
    run(
        &env,
        repo.path(),
        &[
            "HEAD",
            "--update",
            "--quick",
            "--skip-security",
            "--json",
            "--no-fetch",
            "--no-zip",
        ],
        0,
    );
    let (cli, gate, _) = run(
        &env,
        repo.path(),
        &[
            "HEAD",
            "main",
            "--update",
            "--quick",
            "--skip-security",
            "--json",
            "--no-fetch",
            "--no-zip",
        ],
        0,
    );
    assert_eq!(cli["verdict"], "CONDITIONAL");
    assert_eq!(gate["review_range"]["explicit_bases"], true);
    let (_, gate, _) = run(
        &env,
        repo.path(),
        &[
            "HEAD",
            "--current-only",
            "--update",
            "--quick",
            "--skip-security",
            "--json",
            "--no-fetch",
            "--no-zip",
        ],
        0,
    );
    assert_eq!(gate["review_range"]["current_only"], true);
    assert_eq!(gate["decision"]["verdict"], "PASS");
    let (_, gate, _) = run(
        &env,
        repo.path(),
        &[
            "HEAD",
            "--update",
            "--quick",
            "--skip-security",
            "--json",
            "--no-fetch",
            "--no-zip",
        ],
        0,
    );
    assert_eq!(gate["review_range"]["current_only"], false);
    assert_eq!(gate["review_range"]["explicit_bases"], false);
    assert_eq!(gate["decision"]["verdict"], "PASS");
}

#[test]
fn update_reuses_matching_explicit_self_and_mixed_assessments() {
    let repo = fixture();
    git(repo.path(), &["checkout", "main"]);
    let env = support::ContractEnvironment::new();
    for bases in [vec!["main"], vec!["main", "feature"]] {
        let mut args = vec!["HEAD"];
        args.extend(bases);
        args.extend([
            "--update",
            "--quick",
            "--skip-security",
            "--json",
            "--no-fetch",
            "--no-zip",
        ]);
        let (first, gate, _) = run(&env, repo.path(), &args, 0);
        assert_eq!(gate["decision"]["verdict"], "CONDITIONAL");
        let (repeated, repeated_gate, _) = run(&env, repo.path(), &args, 0);
        assert_eq!(first["output_dir"], repeated["output_dir"]);
        assert_eq!(gate, repeated_gate);
    }
}

#[test]
fn update_rejects_a_self_pack_with_a_conflicting_canonical_pass() {
    let repo = fixture();
    git(repo.path(), &["checkout", "main"]);
    let env = support::ContractEnvironment::new();
    let args = [
        "HEAD",
        "main",
        "--update",
        "--quick",
        "--skip-security",
        "--json",
        "--no-fetch",
        "--no-zip",
    ];
    let (first, mut gate, _) = run(&env, repo.path(), &args, 0);
    gate["decision"]["verdict"] = Value::from("PASS");
    gate["decision"]["allow_merge"] = Value::from(true);
    gate["decision"]["analysis_status"] = Value::from("complete");
    gate["decision"]["enforcement_disposition"] = Value::from("clean");
    gate["decision"]["merge_recommendation"] = Value::from("approve");
    gate["decision"]["review_caveats"] = serde_json::json!([]);
    let gate_path =
        Path::new(first["output_dir"].as_str().unwrap()).join("00_summary/MERGE_GATE.json");
    fs::write(gate_path, serde_json::to_vec(&gate).unwrap()).unwrap();
    let output = env
        .command()
        .current_dir(repo.path())
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let repeated: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        repeated["verdict"],
        "CONDITIONAL",
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert_ne!(first["output_dir"], repeated["output_dir"]);
    let regenerated: Value = serde_json::from_slice(
        &fs::read(
            Path::new(repeated["output_dir"].as_str().unwrap()).join("00_summary/MERGE_GATE.json"),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(regenerated["decision"]["allow_merge"], false);
}

#[test]
fn update_compares_effective_base_without_disabling_identical_range_reuse() {
    let repo = fixture();
    git(repo.path(), &["checkout", "-b", "other", "main"]);
    fs::write(repo.path().join("README.md"), "base\nchange\n").unwrap();
    git(
        repo.path(),
        &["commit", "-am", "same tree on divergent base"],
    );
    git(repo.path(), &["checkout", "feature"]);
    let env = support::ContractEnvironment::new();
    let (exact, exact_gate, _) = run(
        &env,
        repo.path(),
        &["gate", "--base", "other", "--exact-base", "--json"],
        0,
    );
    let exact_range = &exact_gate["review_range"]["comparisons"][0];
    assert_eq!(exact_range["status"], "equal_trees");
    assert_ne!(exact_range["requested_base_sha"], exact_range["target_sha"]);
    let args = [
        "feature",
        "other",
        "--update",
        "--quick",
        "--skip-security",
        "--json",
        "--no-fetch",
        "--no-zip",
    ];
    let (normalized, normalized_gate, _) = run(&env, repo.path(), &args, 0);
    assert_ne!(
        normalized["output_dir"], exact["output_dir"],
        "an unchanged requested tip cannot reuse a different effective comparison"
    );
    let normalized_range = &normalized_gate["review_range"]["comparisons"][0];
    assert_eq!(normalized_range["status"], "compared");
    assert_eq!(
        normalized_range["requested_base_sha"],
        exact_range["requested_base_sha"]
    );
    assert_ne!(
        normalized_range["effective_base_sha"],
        exact_range["effective_base_sha"]
    );
    // Keep only the current seed eligible, independent of timestamp ordering.
    fs::remove_dir_all(Path::new(exact["output_dir"].as_str().unwrap())).unwrap();
    let (unchanged, _, _) = run(&env, repo.path(), &args, 0);
    assert_eq!(unchanged["output_dir"], normalized["output_dir"]);

    // Exact and normalized modes may still reuse when the effective range agrees.
    let env = support::ContractEnvironment::new();
    let (ancestor_exact, _, _) = run(
        &env,
        repo.path(),
        &["gate", "--base", "main", "--exact-base", "--json"],
        0,
    );
    let (ancestor_normalized, _, _) = run(
        &env,
        repo.path(),
        &[
            "feature",
            "main",
            "--update",
            "--quick",
            "--skip-security",
            "--json",
            "--no-fetch",
            "--no-zip",
        ],
        0,
    );
    assert_eq!(
        ancestor_normalized["output_dir"],
        ancestor_exact["output_dir"]
    );
}

#[test]
fn mcp_automatic_base_transport_keeps_automatic_self_diff_semantics() {
    let repo = fixture();
    git(repo.path(), &["checkout", "main"]);
    let env = support::ContractEnvironment::new();
    let (_, gate, _) = run(
        &env,
        repo.path(),
        &[
            "--auto-selected-bases",
            "HEAD",
            "main",
            "--quick",
            "--skip-security",
            "--json",
            "--no-fetch",
            "--no-zip",
        ],
        0,
    );
    assert_eq!(gate["review_range"]["explicit_bases"], false);
    assert_eq!(gate["decision"]["verdict"], "PASS");
}

#[test]
fn real_change_and_human_cli_preserve_the_review_boundary() {
    let repo = fixture();
    let env = support::ContractEnvironment::new();
    let (_, gate, _) = run(
        &env,
        repo.path(),
        &["gate", "--base", "main", "--strict", "--json"],
        0,
    );
    assert_eq!(gate["decision"]["verdict"], "PASS");
    assert_eq!(gate["review_range"]["comparisons"][0]["status"], "compared");
    let env = support::ContractEnvironment::new();
    let output = env
        .command()
        .current_dir(repo.path())
        .args([
            "HEAD",
            "self-alias",
            "--quick",
            "--skip-security",
            "--no-fetch",
            "--no-zip",
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("CONDITIONAL"), "{stdout}");
    assert!(stdout.contains("No change reviewed"), "{stdout}");
}

#[test]
fn self_diff_does_not_soften_an_existing_block() {
    let repo = fixture();
    fs::write(
        repo.path().join(".prview-policy.yml"),
        "version: 1\nmode: block\ndefault_severity: block\n",
    )
    .unwrap();
    git(repo.path(), &["commit", "-am", "block unavailable checks"]);
    let env = support::ContractEnvironment::new();
    fs::write(env.scanner_path(), "#!/bin/sh\nexit 1\n").unwrap();
    let (_, gate, _) = run(
        &env,
        repo.path(),
        &["gate", "--base", "HEAD", "--strict", "--json"],
        1,
    );
    assert_eq!(gate["decision"]["verdict"], "BLOCK");
    assert_eq!(gate["decision"]["enforcement_disposition"], "block");
}
