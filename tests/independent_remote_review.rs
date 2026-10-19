//! Remote caller-regression controls. Every repository and child is fixture-owned.
#![cfg(unix)]
use assert_cmd::prelude::*;
use prview::git::git_cmd;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

fn git(root: &Path, args: &[&str]) -> String {
    let output = git_cmd()
        .args(["-c", "commit.gpgsign=false", "-c", "core.hooksPath="])
        .args(args)
        .current_dir(root)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

fn fixture(parent: &Path) -> (PathBuf, PathBuf) {
    let caller = parent.join("caller");
    fs::create_dir(&caller).unwrap();
    git(&caller, &["init", "-b", "main"]);
    git(&caller, &["config", "user.name", "Fixture"]);
    git(
        &caller,
        &["config", "user.email", "fixture@example.invalid"],
    );
    fs::write(caller.join("source.txt"), "base\n").unwrap();
    git(&caller, &["add", "source.txt"]);
    git(&caller, &["commit", "-m", "base"]);
    git(&caller, &["checkout", "-b", "topic"]);
    fs::write(caller.join("source.txt"), "target\n").unwrap();
    git(&caller, &["commit", "-am", "target"]);
    let base = git(&caller, &["rev-parse", "main"]);
    let target = git(&caller, &["rev-parse", "topic"]);
    git(&caller, &["update-ref", "refs/remotes/origin/main", &base]);
    git(
        &caller,
        &["update-ref", "refs/remotes/origin/topic", &target],
    );
    let foreign = caller.join(".claude/worktrees/agent-control");
    git(
        &caller,
        &[
            "worktree",
            "add",
            "-b",
            "foreign",
            foreign.to_str().unwrap(),
        ],
    );
    (caller, foreign)
}

fn command(caller: &Path, parent: &Path) -> Command {
    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("prview"));
    cmd.current_dir(caller)
        .env("PRVIEW_HOME", parent.join("home"))
        .args([
            "topic",
            "main",
            "--remote",
            "--quick",
            "--no-fetch",
            "--no-cache",
            "--no-zip",
            "--no-dashboard",
            "--profile",
            "generic",
            "--no-color",
            "--deadline",
            "30s",
            "--output-dir",
        ])
        .arg(parent.join("pack"));
    cmd
}

#[test]
fn remote_review_accepts_existing_nested_worktree_without_touching_caller_git() {
    let parent = tempfile::tempdir().unwrap();
    let (caller, foreign) = fixture(parent.path());
    let head = fs::read(caller.join(".git/HEAD")).unwrap();
    let index = fs::read(caller.join(".git/index")).unwrap();
    let inventory = git(&caller, &["worktree", "list", "--porcelain"]);
    command(&caller, parent.path()).assert().success();
    assert_eq!(fs::read(caller.join(".git/HEAD")).unwrap(), head);
    assert_eq!(fs::read(caller.join(".git/index")).unwrap(), index);
    assert_eq!(
        git(&caller, &["worktree", "list", "--porcelain"]),
        inventory
    );
    assert_eq!(fs::read(foreign.join("source.txt")).unwrap(), b"target\n");
    let roots: serde_json::Value = serde_json::from_slice(
        &fs::read(parent.path().join("pack/00_summary/EXECUTION_ROOTS.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        roots["caller_root"],
        fs::canonicalize(&caller).unwrap().to_str().unwrap()
    );
    assert_ne!(roots["execution_repo"], roots["caller_root"]);
    assert_eq!(roots["host_write_confinement"], false);
    let report: serde_json::Value =
        serde_json::from_slice(&fs::read(parent.path().join("pack/report.json")).unwrap()).unwrap();
    assert_eq!(report["diff"]["stats"]["files_changed"], 1);
}

#[test]
fn remote_review_keeps_parallel_foreign_edits_without_changing_its_patch() {
    let parent = tempfile::tempdir().unwrap();
    let (caller, foreign) = fixture(parent.path());
    let bin = parent.path().join("bin");
    fs::create_dir(&bin).unwrap();
    let real_git = std::env::split_paths(&std::env::var_os("PATH").unwrap())
        .map(|dir| dir.join("git"))
        .find(|path| path.is_file())
        .unwrap();
    let shim = bin.join("git");
    fs::write(
        &shim,
        r#"#!/bin/sh
case "$*" in
  *"worktree add"*)
    : > "$PRVIEW_TEST_READY"
    n=0
    while [ ! -f "$PRVIEW_TEST_RELEASE" ]; do
      n=$((n+1)); [ "$n" -lt 1000 ] || exit 99
      /bin/sleep 0.01
    done
    ;;
esac
exec "$PRVIEW_TEST_REAL_GIT" "$@"
"#,
    )
    .unwrap();
    fs::set_permissions(&shim, fs::Permissions::from_mode(0o755)).unwrap();
    let ready = parent.path().join("ready");
    let release = parent.path().join("release");
    let mut cmd = command(&caller, parent.path());
    cmd.env("PATH", &bin)
        .env("PRVIEW_TEST_REAL_GIT", &real_git)
        .env("PRVIEW_TEST_READY", &ready)
        .env("PRVIEW_TEST_RELEASE", &release);
    let output_path = parent.path().join("child.stdout");
    let error_path = parent.path().join("child.stderr");
    let mut child = cmd
        .stdout(fs::File::create(&output_path).unwrap())
        .stderr(fs::File::create(&error_path).unwrap())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    while !ready.exists() && Instant::now() < deadline {
        if child.try_wait().unwrap().is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    // Release even on a failed ready assertion so the fixture never leaves a child.
    let admitted = ready.exists();
    if admitted {
        fs::write(caller.join("source.txt"), "parallel operator edit\n").unwrap();
        fs::write(foreign.join("source.txt"), "parallel agent edit\n").unwrap();
        fs::write(caller.join("new-operator-file.txt"), "operator data\n").unwrap();
    }
    fs::write(&release, "continue\n").unwrap();
    let status = child.wait().unwrap();
    assert!(
        admitted,
        "child never reached snapshot: {}",
        fs::read_to_string(&error_path).unwrap()
    );
    assert!(
        status.success(),
        "{}",
        fs::read_to_string(&error_path).unwrap()
    );
    assert_eq!(
        fs::read(caller.join("source.txt")).unwrap(),
        b"parallel operator edit\n"
    );
    assert_eq!(
        fs::read(foreign.join("source.txt")).unwrap(),
        b"parallel agent edit\n"
    );
    assert_eq!(
        fs::read(caller.join("new-operator-file.txt")).unwrap(),
        b"operator data\n"
    );
    let patch = fs::read_to_string(parent.path().join("pack/10_diff/full.patch")).unwrap();
    assert!(patch.contains("+target"));
    assert!(!patch.contains("parallel"));
}
