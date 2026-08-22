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
//! * The gate profile disables tests/lint/heuristics, and the fixture PATH
//!   exposes git but not semgrep, so the `Semgrep scan` check is always skipped
//!   regardless of runner tooling.
//! * Under the default policy that skip is advisory → CONDITIONAL (exit 0, or
//!   exit 2 with `--strict`).
//! * Under a `default_severity: block` policy the same skip becomes blocking →
//!   BLOCK (exit 1).
//! * Running the gate outside a git repository makes the review unable to
//!   execute → exit 3.

use assert_cmd::prelude::*;
use prview::git::git_cmd;
use std::ffi::OsString;
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

/// A repo whose feature branch removes a public Rust function — a genuine
/// breaking API change the gate must surface. Used to prove the breaking-change
/// escalation holds at the process boundary under `--strict`.
fn create_breaking_gate_fixture() -> TempDir {
    let temp = tempfile::tempdir().expect("tempdir");
    let repo = temp.path();

    run_git(repo, &["init"]);
    run_git(repo, &["config", "user.name", "Test User"]);
    run_git(repo, &["config", "user.email", "test@example.com"]);

    fs::write(repo.join("lib.rs"), "pub fn old_api() -> u32 {\n    1\n}\n").expect("write lib.rs");
    run_git(repo, &["add", "lib.rs"]);
    run_git(repo, &["commit", "-m", "initial"]);
    run_git(repo, &["branch", "-M", "main"]);

    run_git(repo, &["checkout", "-b", "feature/remove-public-api"]);
    // Remove the public function: a RemovedSymbol breaking finding.
    fs::write(repo.join("lib.rs"), "// old_api removed\n").expect("update lib.rs");
    run_git(repo, &["add", "lib.rs"]);
    run_git(repo, &["commit", "-m", "remove public api"]);

    temp
}

fn path_without_semgrep(repo: &Path) -> OsString {
    let bin_dir = repo.join(".test-bin");
    fs::create_dir_all(&bin_dir).expect("create fixture bin dir");

    let git_path = which::which("git").expect("git must be available for gate fixtures");
    let git_file_name = git_path.file_name().expect("git path has file name");
    fs::copy(&git_path, bin_dir.join(git_file_name)).expect("copy git into fixture PATH");

    OsString::from(bin_dir)
}

fn prview_gate_command(repo: &Path) -> Command {
    let mut command = Command::new(assert_cmd::cargo::cargo_bin!("prview"));
    command
        .current_dir(repo)
        .env("PATH", path_without_semgrep(repo));
    command
}

#[test]
fn gate_exits_zero_for_non_strict_conditional() {
    let temp = create_gate_fixture();

    // Default policy: the skipped Semgrep check is advisory → CONDITIONAL,
    // which is accepted (exit 0) without --strict.
    prview_gate_command(temp.path())
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
    prview_gate_command(temp.path())
        .args(["gate", "--strict"])
        .assert()
        .code(2);
}

#[test]
fn gate_exits_two_for_strict_conditional_with_breaking_change() {
    let temp = create_breaking_gate_fixture();

    // A diff that removes a public Rust function is a breaking API change. With
    // the default `[gate] breaking_escalation` knob on, that escalates the
    // verdict to CONDITIONAL, which `--strict` rejects with the contract exit 2.
    // (The skipped Semgrep check is also advisory here; either way the process
    // must exit 2 with a real breaking finding present in the pack.)
    prview_gate_command(temp.path())
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

    prview_gate_command(repo).arg("gate").assert().code(1);
}

/// A pack whose `MERGE_GATE.json` is gone carries no verdict. `prview --ci` used
/// to paper over that by re-deriving the decision from the in-memory policy
/// engine — the one path where `allow_merge: true` could sit beside a
/// `CONDITIONAL` verdict. The reader now fails loud with the same execution-error
/// exit code the gate uses, and the contract has to hold at the process boundary.
#[test]
fn ci_run_exits_three_when_pack_has_no_merge_gate() {
    let temp = create_gate_fixture();
    let repo = temp.path();
    let home = tempfile::tempdir().expect("prview home");
    // Built once: the helper copies git into the fixture bin dir, which is not
    // writable a second time.
    let path = path_without_semgrep(repo);

    // 1. A real run, so the pack on disk is a genuine one (metadata included).
    let assert = Command::new(assert_cmd::cargo::cargo_bin!("prview"))
        .current_dir(repo)
        .env("PATH", &path)
        .env("PRVIEW_HOME", home.path())
        .args(["--ci", "--quiet", "--no-zip", "--no-heuristics"])
        .assert();
    let first_code = assert.get_output().status.code();
    assert!(
        matches!(first_code, Some(0) | Some(1)),
        "seeding run must produce a verdict, got exit {first_code:?}"
    );

    // 2. Amputate the verdict artifact, leaving an otherwise complete pack.
    let mut removed = 0usize;
    for gate in walk_merge_gate_json(home.path()) {
        fs::remove_file(&gate).expect("remove MERGE_GATE.json");
        removed += 1;
    }
    assert_eq!(
        removed, 1,
        "seeding run must write exactly one MERGE_GATE.json"
    );

    // 3. `--update` re-reads that pack (HEAD is unchanged). No verdict is
    //    readable, so the process must report an execution error, not a guess.
    Command::new(assert_cmd::cargo::cargo_bin!("prview"))
        .current_dir(repo)
        .env("PATH", &path)
        .env("PRVIEW_HOME", home.path())
        .args(["--ci", "--update", "--quiet", "--no-zip", "--no-heuristics"])
        .assert()
        .code(3);
}

/// Every `00_summary/MERGE_GATE.json` under a prview home.
fn walk_merge_gate_json(root: &Path) -> Vec<std::path::PathBuf> {
    let mut found = Vec::new();
    let Ok(entries) = fs::read_dir(root) else {
        return found;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        // `latest` is a symlink to the newest run; following it would visit the
        // same pack twice.
        if path.is_symlink() {
            continue;
        }
        if path.is_dir() {
            found.extend(walk_merge_gate_json(&path));
        } else if path.file_name().is_some_and(|n| n == "MERGE_GATE.json") {
            found.push(path);
        }
    }
    found
}

#[test]
fn gate_exits_three_when_it_cannot_execute() {
    // Outside a git repository the review cannot run, so the gate reports an
    // execution error (exit 3) rather than a verdict.
    let temp = tempfile::tempdir().expect("tempdir");

    Command::new(assert_cmd::cargo::cargo_bin!("prview"))
        .current_dir(temp.path())
        .env("GIT_CEILING_DIRECTORIES", temp.path())
        .arg("gate")
        .assert()
        .code(3);
}
