//! Hermetic proof that `git_cmd()` isolates child git processes from a poisoned
//! parent environment. Without the `GIT_*` strip, a `GIT_DIR`/`GIT_WORK_TREE`
//! inherited from a hook/editor/worktree can silently retarget git at the wrong
//! repository — a P1 data-safety hazard.

use assert_cmd::prelude::*;
use prview::git::git_cmd;
use std::path::Path;
use std::process::Command;
use tempfile::TempDir;

/// The variables `git_cmd()` must strip. Kept in sync with `src/git/cmd.rs`.
const GIT_LEAK_VARS: &[&str] = &[
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_COMMON_DIR",
];

fn run_git(repo: &Path, args: &[&str]) {
    let status = git_cmd()
        .args(args)
        .current_dir(repo)
        .status()
        .expect("failed to run git");
    assert!(status.success(), "git {:?} failed", args);
}

/// Create a repo with a single commit on `branch`, return its short HEAD sha.
fn make_repo(branch: &str, content: &str) -> (TempDir, String) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path();

    run_git(path, &["init", "-b", branch]);
    run_git(path, &["config", "user.email", "test@test.com"]);
    run_git(path, &["config", "user.name", "Test"]);
    std::fs::write(path.join("file.txt"), content).expect("write");
    run_git(path, &["add", "."]);
    run_git(path, &["commit", "-m", content]);

    let out = git_cmd()
        .args(["rev-parse", "--short", "HEAD"])
        .current_dir(path)
        .output()
        .expect("rev-parse");
    let sha = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (dir, sha)
}

/// Builder-contract: `git_cmd()` schedules removal of every leak var, without
/// mutating the process environment.
#[test]
fn git_cmd_strips_all_leak_vars() {
    let cmd = git_cmd();
    let removed: Vec<&str> = cmd
        .get_envs()
        .filter(|(_, v)| v.is_none())
        .map(|(k, _)| k.to_str().unwrap_or(""))
        .collect();

    for var in GIT_LEAK_VARS {
        assert!(
            removed.contains(var),
            "git_cmd() must env_remove {var}; removed = {removed:?}"
        );
    }
}

/// End-to-end: with a poisoned `GIT_DIR`/`GIT_WORK_TREE` pointing at a victim
/// repo, `prview state` must still operate on the working-tree repo (cwd) and
/// must NOT touch the victim. `find_repo_root()` resolves via
/// `git rev-parse --show-toplevel`, so an un-stripped `GIT_DIR` would retarget
/// it at the victim — this test fails loudly if the strip ever regresses.
#[test]
fn poisoned_git_env_does_not_leak_into_prview() {
    let (victim, victim_sha) = make_repo("victim-branch", "victim\n");
    let (work, work_sha) = make_repo("work-branch", "work\n");
    assert_ne!(victim_sha, work_sha, "fixtures must differ");

    let victim_git_dir = victim.path().join(".git");

    let assert = Command::new(assert_cmd::cargo::cargo_bin!("prview"))
        .current_dir(work.path())
        .args(["state", "--json", "--fast"])
        .env("GIT_DIR", &victim_git_dir)
        .env("GIT_WORK_TREE", victim.path())
        .assert()
        .success();

    let stdout = String::from_utf8(assert.get_output().stdout.clone()).expect("utf8");
    let json: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("state --json should emit JSON");

    let reported = json["commit"].as_str().unwrap_or("");
    assert_eq!(
        reported, work_sha,
        "prview must report the work repo's HEAD, not the poisoned victim's ({victim_sha})"
    );
    assert_eq!(json["branch"].as_str().unwrap_or(""), "work-branch");

    // Victim repo must be untouched: HEAD still points at its original commit.
    let after = git_cmd()
        .args(["rev-parse", "--short", "HEAD"])
        .current_dir(victim.path())
        .output()
        .expect("victim rev-parse");
    assert_eq!(
        String::from_utf8_lossy(&after.stdout).trim(),
        victim_sha,
        "victim repo HEAD must not move"
    );
}
