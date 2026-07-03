//! Integration tests for `prview scope` against real temp repositories.
//!
//! Covers the QC-risk surfaces: include/exclude semantics end-to-end, the
//! exclude-only WIP path, merge-commit skipping, and output-dir safety.

use assert_cmd::prelude::*;
use prview::git::git_cmd;
use std::path::Path;
use std::process::Command;
use tempfile::TempDir;

fn run_git(repo: &Path, args: &[&str]) {
    let status = git_cmd()
        .args(args)
        .current_dir(repo)
        .status()
        .expect("failed to run git");
    assert!(status.success(), "git {:?} failed", args);
}

fn write(repo: &Path, rel: &str, content: &str) {
    let path = repo.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
}

fn init_repo() -> TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    run_git(repo, &["init", "-b", "main"]);
    run_git(repo, &["config", "user.email", "test@test.com"]);
    run_git(repo, &["config", "user.name", "Test"]);
    dir
}

fn commit_all(repo: &Path, msg: &str) {
    run_git(repo, &["add", "-A"]);
    run_git(repo, &["commit", "-m", msg]);
}

fn prview_scope(repo: &Path, args: &[&str]) -> assert_cmd::assert::Assert {
    Command::new(assert_cmd::cargo::cargo_bin!("prview"))
        .current_dir(repo)
        .arg("scope")
        .args(args)
        .assert()
}

/// `--include 'src/**'` keeps src files and drops docs.
#[test]
fn include_only_keeps_matching_files() {
    let dir = init_repo();
    let repo = dir.path();
    write(repo, "src/app.rs", "fn main() {}\n");
    write(repo, "docs/readme.md", "# docs\n");
    commit_all(repo, "init");

    run_git(repo, &["checkout", "-b", "feature"]);
    write(repo, "src/app.rs", "fn main() { println!(\"hi\"); }\n");
    write(repo, "docs/readme.md", "# docs\nmore\n");
    commit_all(repo, "feature change");

    let out = repo.join("pack");
    prview_scope(
        repo,
        &[
            "--include",
            "src/**",
            "--base",
            "main",
            "-o",
            out.to_str().unwrap(),
        ],
    )
    .success();

    let full = std::fs::read_to_string(out.join("full.patch")).expect("full.patch");
    assert!(full.contains("src/app.rs"), "src must be in pack:\n{full}");
    assert!(
        !full.contains("docs/readme.md"),
        "docs must be excluded:\n{full}"
    );
    assert!(out.join("SCOPE.md").exists());
}

/// `--exclude 'docs/**'` drops docs, keeps everything else.
#[test]
fn exclude_only_drops_matching_files() {
    let dir = init_repo();
    let repo = dir.path();
    write(repo, "src/app.rs", "a\n");
    write(repo, "docs/readme.md", "d\n");
    commit_all(repo, "init");

    run_git(repo, &["checkout", "-b", "feature"]);
    write(repo, "src/app.rs", "a\nb\n");
    write(repo, "docs/readme.md", "d\ne\n");
    commit_all(repo, "feature change");

    let out = repo.join("pack");
    prview_scope(
        repo,
        &[
            "--exclude",
            "docs/**",
            "--base",
            "main",
            "-o",
            out.to_str().unwrap(),
        ],
    )
    .success();

    let full = std::fs::read_to_string(out.join("full.patch")).expect("full.patch");
    assert!(full.contains("src/app.rs"));
    assert!(!full.contains("docs/readme.md"), "docs excluded:\n{full}");
}

/// Regression: a WIP-only change to a tracked file that is NOT in the committed
/// diff must still be captured under an exclude-only scope. The previous
/// implementation fed the exclude glob to git as a positive pathspec and missed
/// this entirely.
#[test]
fn wip_exclude_only_captures_wip_only_file() {
    let dir = init_repo();
    let repo = dir.path();
    write(repo, "src/stable.rs", "stable\n");
    write(repo, "docs/readme.md", "d\n");
    commit_all(repo, "init");

    // Feature commits touch ONLY docs — committed diff vs main has no src files.
    run_git(repo, &["checkout", "-b", "feature"]);
    write(repo, "docs/readme.md", "d\nmore\n");
    commit_all(repo, "docs only commit");

    // WIP: modify a tracked src file that is unchanged across main..feature.
    write(repo, "src/stable.rs", "stable\nWIP EDIT\n");

    let out = repo.join("pack");
    prview_scope(
        repo,
        &[
            "--exclude",
            "docs/**",
            "--wip",
            "--base",
            "main",
            "-o",
            out.to_str().unwrap(),
        ],
    )
    .success();

    let wip = std::fs::read_to_string(out.join("wip.patch"))
        .expect("wip.patch must exist for exclude-only WIP-only change");
    assert!(
        wip.contains("src/stable.rs"),
        "WIP src change must be captured:\n{wip}"
    );
    assert!(wip.contains("WIP EDIT"));
    assert!(
        !wip.contains("docs/readme.md"),
        "docs WIP must be excluded:\n{wip}"
    );
}

/// Regression (#2/#7): `prview scope` must work when launched from a
/// subdirectory of the repo. The repo root was previously taken from the process
/// working directory, so any invocation below the top level failed to open the
/// repository.
#[test]
fn scope_runs_from_subdirectory() {
    let dir = init_repo();
    let repo = dir.path();
    write(repo, "src/app.rs", "a\n");
    commit_all(repo, "init");
    run_git(repo, &["checkout", "-b", "feature"]);
    write(repo, "src/app.rs", "a\nb\n");
    commit_all(repo, "change");

    let subdir = repo.join("src");
    let assert = Command::new(assert_cmd::cargo::cargo_bin!("prview"))
        .current_dir(&subdir)
        .arg("scope")
        .args(["--include", "src/**", "--base", "main", "-o", "pack"])
        .assert()
        .success();
    let _ = assert;

    // Relative `-o` resolves against the working dir (the subdirectory).
    let full = std::fs::read_to_string(subdir.join("pack/full.patch")).expect("full.patch");
    assert!(full.contains("src/app.rs"), "src change captured:\n{full}");
}

/// Regression (#5): when the base branch moves on after divergence, the scope
/// pack must diff from the merge-base, not the base tip. A tip-to-tip diff would
/// surface the base-only change as a spurious (reversed) hunk.
#[test]
fn scope_diffs_from_merge_base_not_base_tip() {
    let dir = init_repo();
    let repo = dir.path();
    write(repo, "src/app.rs", "orig\n");
    commit_all(repo, "init");

    // Feature diverges and changes app.rs.
    run_git(repo, &["checkout", "-b", "feature"]);
    write(repo, "src/app.rs", "orig\nfeature edit\n");
    commit_all(repo, "feature change");

    // main moves on independently, adding a file feature never saw.
    run_git(repo, &["checkout", "main"]);
    write(
        repo,
        "src/base_only.rs",
        "landed on base after divergence\n",
    );
    commit_all(repo, "base-only change");

    run_git(repo, &["checkout", "feature"]);

    let out = repo.join("pack");
    prview_scope(
        repo,
        &[
            "--include",
            "src/**",
            "--base",
            "main",
            "-o",
            out.to_str().unwrap(),
        ],
    )
    .success();

    let full = std::fs::read_to_string(out.join("full.patch")).expect("full.patch");
    assert!(
        full.contains("src/app.rs"),
        "feature's own change must be present:\n{full}"
    );
    assert!(
        !full.contains("base_only.rs"),
        "base-only change must NOT leak into the scope pack (merge-base diff):\n{full}"
    );
}

/// Regression (#4): an empty committed scope combined with `--wip` must NOT
/// pull in every commit. Previously the empty file list became an empty
/// pathspec that matched all files, so unrelated commits (and their diffs)
/// leaked into the pack.
#[test]
fn empty_committed_scope_with_wip_does_not_leak_all_commits() {
    let dir = init_repo();
    let repo = dir.path();
    write(repo, "src/app.rs", "a\n");
    write(repo, "docs/readme.md", "d\n");
    commit_all(repo, "init");

    // Feature commit touches ONLY docs; `--include src/**` yields an empty
    // committed scope.
    run_git(repo, &["checkout", "-b", "feature"]);
    write(repo, "docs/readme.md", "d\nmore\n");
    commit_all(repo, "docs only commit");

    // WIP change to a src file keeps the run alive.
    write(repo, "src/app.rs", "a\nWIP\n");

    let out = repo.join("pack");
    prview_scope(
        repo,
        &[
            "--include",
            "src/**",
            "--wip",
            "--base",
            "main",
            "-o",
            out.to_str().unwrap(),
        ],
    )
    .success();

    // No committed commit is in scope: per-commit dir must be empty and the
    // docs change must not appear anywhere in the committed artifacts.
    let per_commit_count = std::fs::read_dir(out.join("per-commit"))
        .map(|rd| rd.filter_map(|e| e.ok()).count())
        .unwrap_or(0);
    assert_eq!(
        per_commit_count, 0,
        "empty committed scope must yield zero per-commit patches"
    );
    let commits_log = std::fs::read_to_string(out.join("commits.log")).unwrap_or_default();
    assert!(
        !commits_log.contains("docs only commit"),
        "unrelated commit must not leak into commits.log:\n{commits_log}"
    );
    let full = std::fs::read_to_string(out.join("full.patch")).unwrap_or_default();
    assert!(
        !full.contains("docs/readme.md"),
        "docs must not leak into full.patch:\n{full}"
    );

    // The WIP src change is still captured.
    let wip = std::fs::read_to_string(out.join("wip.patch")).expect("wip.patch");
    assert!(
        wip.contains("src/app.rs"),
        "WIP src change captured:\n{wip}"
    );
}

/// Regression (#6): a rename within scope must carry the deletion of the old
/// path. Per-path diffing showed only the added new path and dropped the old
/// one, so a reviewer never saw the file move.
#[test]
fn scoped_full_patch_preserves_rename_old_path() {
    let dir = init_repo();
    let repo = dir.path();
    write(repo, "src/old_name.rs", "fn a() {}\nfn b() {}\nfn c() {}\n");
    commit_all(repo, "init");

    run_git(repo, &["checkout", "-b", "feature"]);
    run_git(repo, &["mv", "src/old_name.rs", "src/new_name.rs"]);
    commit_all(repo, "rename module");

    let out = repo.join("pack");
    prview_scope(
        repo,
        &[
            "--include",
            "src/**",
            "--base",
            "main",
            "-o",
            out.to_str().unwrap(),
        ],
    )
    .success();

    let full = std::fs::read_to_string(out.join("full.patch")).expect("full.patch");
    assert!(
        full.contains("new_name.rs"),
        "new path must be present:\n{full}"
    );
    assert!(
        full.contains("old_name.rs"),
        "old (pre-rename) path must remain visible in the scoped patch:\n{full}"
    );
}

/// Regression (#8): a WIP-only file (present in the working tree but not in the
/// committed diff) must be listed in SCOPE.md, marked as WIP, so the pack's
/// manifest reflects everything actually included.
#[test]
fn scope_md_lists_wip_only_files() {
    let dir = init_repo();
    let repo = dir.path();
    write(repo, "src/stable.rs", "stable\n");
    write(repo, "docs/readme.md", "d\n");
    commit_all(repo, "init");

    run_git(repo, &["checkout", "-b", "feature"]);
    write(repo, "docs/readme.md", "d\nmore\n");
    commit_all(repo, "docs only commit");

    // WIP-only change to a src file that never appears in the committed diff.
    write(repo, "src/stable.rs", "stable\nWIP EDIT\n");

    let out = repo.join("pack");
    prview_scope(
        repo,
        &[
            "--exclude",
            "docs/**",
            "--wip",
            "--base",
            "main",
            "-o",
            out.to_str().unwrap(),
        ],
    )
    .success();

    let scope_md = std::fs::read_to_string(out.join("SCOPE.md")).expect("SCOPE.md");
    assert!(
        scope_md.contains("src/stable.rs"),
        "WIP-only file must be listed in SCOPE.md:\n{scope_md}"
    );
    assert!(
        scope_md.contains("(WIP)"),
        "WIP-only file must be marked as WIP:\n{scope_md}"
    );
}

/// Regression (#10): `--wip` must capture brand-new untracked files, not just
/// changes to tracked files. The workdir diff previously omitted untracked
/// entries entirely.
#[test]
fn wip_captures_untracked_file() {
    let dir = init_repo();
    let repo = dir.path();
    write(repo, "src/app.rs", "a\n");
    commit_all(repo, "init");
    run_git(repo, &["checkout", "-b", "feature"]);

    // A brand-new file that was never `git add`ed — untracked.
    write(repo, "src/brand_new.rs", "fn fresh() {}\n");

    let out = repo.join("pack");
    prview_scope(
        repo,
        &[
            "--include",
            "src/**",
            "--wip",
            "--base",
            "main",
            "-o",
            out.to_str().unwrap(),
        ],
    )
    .success();

    let wip = std::fs::read_to_string(out.join("wip.patch")).expect("wip.patch");
    assert!(
        wip.contains("src/brand_new.rs"),
        "untracked WIP file must be captured:\n{wip}"
    );
    assert!(
        wip.contains("fn fresh()"),
        "content must be present:\n{wip}"
    );
}

/// Merge commits are skipped per spec — their first-parent diff duplicates the
/// individual commits.
#[test]
fn merge_commits_are_skipped() {
    let dir = init_repo();
    let repo = dir.path();
    write(repo, "src/base.rs", "base\n");
    commit_all(repo, "init");
    let base_sha = {
        let o = git_cmd()
            .args(["rev-parse", "HEAD"])
            .current_dir(repo)
            .output()
            .unwrap();
        String::from_utf8_lossy(&o.stdout).trim().to_string()
    };

    run_git(repo, &["checkout", "-b", "topic"]);
    write(repo, "src/topic.rs", "topic\n");
    commit_all(repo, "topic commit");

    run_git(repo, &["checkout", "main"]);
    write(repo, "src/main2.rs", "main2\n");
    commit_all(repo, "main commit");

    run_git(repo, &["merge", "--no-ff", "-m", "Merge topic", "topic"]);

    let out = repo.join("pack");
    prview_scope(
        repo,
        &[
            "--include",
            "src/**",
            "--base",
            &base_sha,
            "-o",
            out.to_str().unwrap(),
        ],
    )
    .success();

    // Two real commits touch src/ (topic + main2); the merge must not appear.
    let per_commit = out.join("per-commit");
    let count = std::fs::read_dir(&per_commit)
        .map(|rd| rd.filter_map(|e| e.ok()).count())
        .unwrap_or(0);
    assert_eq!(
        count, 2,
        "merge commit must be skipped, expected 2 per-commit patches"
    );

    let commits_log = std::fs::read_to_string(out.join("commits.log")).unwrap_or_default();
    assert!(
        !commits_log.contains("Merge topic"),
        "merge commit must not appear in commits.log:\n{commits_log}"
    );
}

/// Output-dir guard: refuse to wipe a directory that isn't a prior scope pack.
#[test]
fn output_dir_guard_refuses_non_pack() {
    let dir = init_repo();
    let repo = dir.path();
    write(repo, "src/app.rs", "a\n");
    commit_all(repo, "init");
    run_git(repo, &["checkout", "-b", "feature"]);
    write(repo, "src/app.rs", "a\nb\n");
    commit_all(repo, "change");

    // A precious, non-pack directory the operator might accidentally target.
    write(repo, "important/keep.txt", "do not delete me\n");

    prview_scope(
        repo,
        &["--include", "src/**", "--base", "main", "-o", "important"],
    )
    .failure();

    assert!(
        repo.join("important/keep.txt").exists(),
        "guard must not delete a non-pack directory"
    );
}

/// Regression (#16): a directory that merely contains a `SCOPE.md` — but not the
/// schema-tagged pack marker — must be refused. `SCOPE.md` alone is a filename an
/// operator's own directory can legitimately carry; only our marker authorises a
/// wipe.
#[test]
fn output_dir_guard_refuses_bare_scope_md() {
    let dir = init_repo();
    let repo = dir.path();
    write(repo, "src/app.rs", "a\n");
    commit_all(repo, "init");
    run_git(repo, &["checkout", "-b", "feature"]);
    write(repo, "src/app.rs", "a\nb\n");
    commit_all(repo, "change");

    // Operator's own directory that happens to hold a SCOPE.md and precious data.
    write(repo, "notes/SCOPE.md", "my project scope, do not delete\n");
    write(repo, "notes/keep.txt", "precious\n");

    prview_scope(
        repo,
        &["--include", "src/**", "--base", "main", "-o", "notes"],
    )
    .failure();

    assert!(
        repo.join("notes/keep.txt").exists(),
        "guard must not delete a dir carrying only a bare SCOPE.md"
    );
    assert!(repo.join("notes/SCOPE.md").exists());
}

/// Regression (#16): re-running scope into a previously generated pack succeeds —
/// the marker written on the first run authorises cleaning on the second.
#[test]
fn output_dir_guard_allows_rerun_of_prior_pack() {
    let dir = init_repo();
    let repo = dir.path();
    write(repo, "src/app.rs", "a\n");
    commit_all(repo, "init");
    run_git(repo, &["checkout", "-b", "feature"]);
    write(repo, "src/app.rs", "a\nb\n");
    commit_all(repo, "change");

    let out = repo.join("pack");
    let args = &[
        "--include",
        "src/**",
        "--base",
        "main",
        "-o",
        out.to_str().unwrap(),
    ];
    prview_scope(repo, args).success();
    assert!(
        out.join(".prview-scope-pack.json").exists(),
        "marker written"
    );
    // Second run must be allowed to clean and regenerate the prior pack.
    prview_scope(repo, args).success();
    assert!(out.join("full.patch").exists());
}
