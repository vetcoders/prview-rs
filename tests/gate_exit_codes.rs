//! End-to-end contract test for the `prview gate` process exit codes.
//!
//! The exit-code mapping (0 = PASS/non-strict CONDITIONAL, 1 = BLOCK,
//! 2 = strict CONDITIONAL, 3 = gate could not execute) is unit-tested at the
//! pure-function level in `src/gate.rs`. That does not prove the *binary*
//! actually exits with those codes — the composite GitHub Action decides
//! pass/fail solely from the process exit code, so the contract has to hold at
//! the process boundary, not just in a mapping function.
//!
//! These tests drive the real binary and assert the process exit code for each
//! contract branch. They are deterministic without depending on which quality
//! tools happen to be installed on the runner:
//!
//! * The gate profile disables tests/lint/security/heuristics, so the
//!   `heuristics_loctree` check is always *skipped* regardless of environment.
//! * Under the default policy that skip is advisory → CONDITIONAL (exit 0, or
//!   exit 2 with `--strict`).
//! * Under a `default_severity: block` policy the same skip becomes blocking →
//!   BLOCK (exit 1).
//! * Running the gate outside a git repository makes the review unable to
//!   execute → exit 3.

use assert_cmd::prelude::*;
use prview::git::git_cmd;
use std::fs;
use std::path::Path;
use std::process::Command;
use tempfile::TempDir;

fn run_git(repo: &Path, args: &[&str]) {
    let status = git_cmd()
        .args(["-c", "commit.gpgsign=false", "-c", "tag.gpgsign=false"])
        .args(args)
        .current_dir(repo)
        .status()
        .expect("failed to run git command");
    assert!(status.success(), "git command failed: {args:?}");
}

/// A minimal repo with a `main` base and a checked-out feature branch that
/// changes one file, so `prview gate` has a diff to review.
fn create_gate_fixture() -> TempDir {
    let temp = tempfile::tempdir().expect("tempdir");
    let repo = temp.path();

    run_git(repo, &["init"]);
    run_git(repo, &["config", "user.name", "Test User"]);
    run_git(repo, &["config", "user.email", "test@example.com"]);

    fs::write(repo.join("README.md"), "hello\n").expect("write file");
    run_git(repo, &["add", "README.md"]);
    run_git(repo, &["commit", "-m", "initial"]);
    run_git(repo, &["branch", "-M", "main"]);

    run_git(repo, &["checkout", "-b", "feature/gate-exit-codes"]);
    fs::write(repo.join("README.md"), "hello\nworld\n").expect("update file");
    run_git(repo, &["add", "README.md"]);
    run_git(repo, &["commit", "-m", "change"]);

    temp
}

#[test]
fn gate_exits_zero_for_non_strict_conditional() {
    let temp = create_gate_fixture();

    // Default policy: the skipped heuristics check is advisory → CONDITIONAL,
    // which is accepted (exit 0) without --strict.
    Command::new(assert_cmd::cargo::cargo_bin!("prview"))
        .current_dir(temp.path())
        .arg("gate")
        .assert()
        .code(0);
}

#[test]
fn gate_exits_two_for_strict_conditional() {
    let temp = create_gate_fixture();

    // Same CONDITIONAL verdict, but --strict rejects it with exit 2. This is the
    // exact code clap also uses for usage errors, which is why the action must
    // distinguish the two (see action.yml) — here we pin the contract value.
    Command::new(assert_cmd::cargo::cargo_bin!("prview"))
        .current_dir(temp.path())
        .args(["gate", "--strict"])
        .assert()
        .code(2);
}

#[test]
fn gate_exits_one_for_block_verdict() {
    let temp = create_gate_fixture();
    let repo = temp.path();

    // Escalate the skipped required check to blocking so the verdict is BLOCK.
    fs::write(
        repo.join(".prview-policy.yml"),
        "version: 1\nmode: block\ndefault_severity: block\n",
    )
    .expect("write policy");
    run_git(repo, &["add", ".prview-policy.yml"]);
    run_git(repo, &["commit", "-m", "block policy"]);

    Command::new(assert_cmd::cargo::cargo_bin!("prview"))
        .current_dir(repo)
        .arg("gate")
        .assert()
        .code(1);
}

#[test]
fn gate_exits_three_when_it_cannot_execute() {
    // Outside a git repository the review cannot run, so the gate reports an
    // execution error (exit 3) rather than a verdict.
    let temp = tempfile::tempdir().expect("tempdir");

    Command::new(assert_cmd::cargo::cargo_bin!("prview"))
        .current_dir(temp.path())
        .arg("gate")
        .assert()
        .code(3);
}
