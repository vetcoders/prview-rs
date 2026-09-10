//! Small protocol probes only: no review run, external checks, or repo scan.
use assert_cmd::Command;
use std::path::Path;
use std::time::Duration;

fn commit(repo: &git2::Repository, root: &Path, source: &str) -> git2::Oid {
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join("Cargo.toml"),
        "[package]\nname='tiny'\nversion='0.0.0'\nedition='2024'\n",
    )
    .unwrap();
    std::fs::write(root.join("src/lib.rs"), source).unwrap();
    let mut index = repo.index().unwrap();
    index.add_path(Path::new("Cargo.toml")).unwrap();
    index.add_path(Path::new("src/lib.rs")).unwrap();
    let tree_id = index.write_tree().unwrap();
    let tree = repo.find_tree(tree_id).unwrap();
    let signature = git2::Signature::now("Fixture", "fixture@example.com").unwrap();
    let parent = repo.head().ok().and_then(|head| head.peel_to_commit().ok());
    let parents: Vec<_> = parent.iter().collect();
    repo.commit(
        Some("HEAD"),
        &signature,
        &signature,
        "fixture",
        &tree,
        &parents,
    )
    .unwrap()
}

fn binary() -> Command {
    let mut command = Command::new(assert_cmd::cargo::cargo_bin!("prview"));
    command
        .timeout(Duration::from_secs(5))
        .env("RAYON_NUM_THREADS", "1")
        .env("TOKIO_WORKER_THREADS", "1")
        .env_remove("PRVIEW_INTERNAL_RUST_API_WORKER")
        .env_remove("PRVIEW_INTERNAL_LOCTREE_WORKER_ROOT");
    command
}

#[test]
fn private_worker_real_binary_returns_tiny_revision_delta() {
    let root = tempfile::tempdir().unwrap();
    let repo = git2::Repository::init(root.path()).unwrap();
    let base = commit(&repo, root.path(), "pub fn before() {}\n");
    let target = commit(&repo, root.path(), "pub fn after() {}\n");
    let pairs = serde_json::json!([{"base_revision":base.to_string(), "target_revision":target.to_string()}]);
    let result = binary()
        .arg("--prview-internal-rust-api-worker")
        .env("PRVIEW_INTERNAL_RUST_API_WORKER", "1")
        .env("PRVIEW_INTERNAL_RUST_API_REPO", root.path())
        .env("PRVIEW_INTERNAL_RUST_API_PAIRS", pairs.to_string())
        .assert()
        .success();
    let delta: serde_json::Value = serde_json::from_slice(&result.get_output().stdout).unwrap();
    assert_eq!(delta["base_revision"], format!("git_tree:{base}"));
    assert_eq!(delta["target_revision"], format!("git_tree:{target}"));
    assert!(!delta["added"].as_array().unwrap().is_empty());
    assert!(!delta["removed"].as_array().unwrap().is_empty());
    assert!(!String::from_utf8_lossy(&result.get_output().stdout).contains("running "));
}

#[test]
fn private_worker_real_binary_rejects_environment_only_activation() {
    let result = binary()
        .arg("--help")
        .env("PRVIEW_INTERNAL_RUST_API_WORKER", "1")
        .assert()
        .failure();
    assert!(
        String::from_utf8_lossy(&result.get_output().stderr)
            .contains("invalid private worker invocation")
    );
}
