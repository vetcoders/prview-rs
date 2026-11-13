//! A partial clone lacks objects by design. A review that needs one must say
//! so, not suggest that the caller is outside a repository.
#![cfg(unix)]

use prview::git::git_cmd;
use std::path::Path;
use std::process::Command;

fn git(dir: &Path, args: &[&str]) {
    let status = git_cmd()
        .args([
            "-c",
            "user.name=fixture",
            "-c",
            "user.email=fixture@example.invalid",
        ])
        .args(["-c", "core.hooksPath=", "-c", "commit.gpgsign=false"])
        .args(args)
        .current_dir(dir)
        .status()
        .expect("run git");
    assert!(status.success(), "git {args:?} failed");
}

#[test]
fn local_review_of_partial_clone_names_the_missing_objects() {
    let root = tempfile::tempdir().expect("fixture root");
    let seed = root.path().join("seed");
    std::fs::create_dir(&seed).expect("seed directory");
    git(&seed, &["init", "--quiet", "--initial-branch=main"]);
    std::fs::write(seed.join("a.txt"), "base\n").expect("base file");
    git(&seed, &["add", "a.txt"]);
    git(&seed, &["commit", "--quiet", "-m", "base"]);
    git(&seed, &["switch", "--quiet", "-c", "feature"]);
    std::fs::write(seed.join("a.txt"), "changed\n").expect("changed file");
    std::fs::write(seed.join("b.txt"), "added\n").expect("added file");
    git(&seed, &["add", "a.txt", "b.txt"]);
    git(&seed, &["commit", "--quiet", "-m", "feature"]);
    git(&seed, &["switch", "--quiet", "main"]);

    let upstream = root.path().join("upstream.git");
    git(
        root.path(),
        &["clone", "--quiet", "--bare", "seed", "upstream.git"],
    );
    git(&upstream, &["config", "uploadpack.allowFilter", "true"]);
    let url = format!("file://{}", upstream.display());
    git(
        root.path(),
        &["clone", "--quiet", "--filter=blob:none", &url, "caller"],
    );
    let caller = root.path().join("caller");

    let home = tempfile::tempdir().expect("prview home");
    let output = Command::new(assert_cmd::cargo::cargo_bin!("prview"))
        .current_dir(&caller)
        .env("PRVIEW_HOME", home.path())
        .args(["origin/feature", "main", "--quick", "--no-fetch"])
        .output()
        .expect("run prview");
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert_eq!(output.status.code(), Some(1), "stderr={stderr}");
    assert!(stderr.contains("object not found"), "{stderr}");
    assert!(
        stderr.contains("this repository is a partial clone"),
        "the hint must name the partial clone: {stderr}"
    );
    assert!(
        stderr.contains("git fetch --refetch --no-filter origin"),
        "the hint must say how to obtain the objects: {stderr}"
    );
    assert!(
        !stderr.contains("inside a git repository"),
        "the caller is inside a healthy repository: {stderr}"
    );
}
