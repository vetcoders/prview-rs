//! Exact profiles must use gitlink objects, never the operator's checkout.

use super::{Config, ProfileKind, test_config};
use crate::checks::{Check, CheckEligibility};
use std::path::Path;

fn init(path: &Path) -> git2::Repository {
    std::fs::create_dir_all(path).unwrap();
    let mut options = git2::RepositoryInitOptions::new();
    options.initial_head("main");
    git2::Repository::init_opts(path, &options).unwrap()
}

fn commit(repo: &git2::Repository) -> git2::Oid {
    let mut index = repo.index().unwrap();
    // Gitlinks are inserted explicitly by pin(); do not let libgit2 recurse
    // into an unregistered embedded repository while adding fixture files.
    let mut add =
        |path: &Path, _: &[u8]| i32::from(repo.workdir().unwrap().join(path).join(".git").exists());
    index
        .add_all(["*"], git2::IndexAddOption::DEFAULT, Some(&mut add))
        .unwrap();
    index.write().unwrap();
    let tree_oid = index.write_tree().unwrap();
    let tree = repo.find_tree(tree_oid).unwrap();
    let parent = repo.head().ok().map(|head| head.peel_to_commit().unwrap());
    let parents: Vec<_> = parent.iter().collect();
    let sig = git2::Signature::now("Profile test", "profile@example.test").unwrap();
    repo.commit(Some("HEAD"), &sig, &sig, "fixture", &tree, &parents)
        .unwrap()
}

fn pin(repo: &git2::Repository, name: &str, oid: git2::Oid) -> git2::Oid {
    let parent = repo.head().unwrap().peel_to_commit().unwrap();
    let mut builder = repo.treebuilder(Some(&parent.tree().unwrap())).unwrap();
    builder.insert(name, oid, 0o160000).unwrap();
    let tree_oid = builder.write().unwrap();
    let tree = repo.find_tree(tree_oid).unwrap();
    let sig = git2::Signature::now("Profile test", "profile@example.test").unwrap();
    repo.commit(Some("HEAD"), &sig, &sig, "pin", &tree, &[&parent])
        .unwrap()
}

fn cargo_project(path: &Path) {
    std::fs::create_dir_all(path.join("src")).unwrap();
    std::fs::write(
        path.join("Cargo.toml"),
        "[package]\nname = 'pinned_backend'\nversion = '0.1.0'\nedition = '2024'\n",
    )
    .unwrap();
    std::fs::write(path.join("src/lib.rs"), "pub fn pinned() {}\n").unwrap();
}

fn exact_config(root: &Path, target: git2::Oid) -> Config {
    let mut config = test_config();
    config.repo_root = root.to_path_buf();
    config.requested_profile = Some(crate::cli::Profile::Auto);
    config.target = Some(target.to_string());
    config.pinned_target = Some(crate::git::ResolvedRef {
        name: target.to_string(),
        commit_id: target.to_string(),
        is_remote: false,
    });
    config.do_fetch = false;
    config
}

fn assert_rust_target(config: &Config, relative: &str) {
    assert_eq!(config.profile.kind, ProfileKind::Rust);
    assert!(config.profile.has_cargo);
    assert_eq!(
        config.profile.cargo_root,
        Some(config.repo_root.join(relative))
    );
    assert_eq!(config.profile.rust_dirs, [config.repo_root.join(relative)]);
    let names: Vec<_> = crate::checks::get_checks_for_profile(config)
        .iter()
        .map(|check| check.name().to_owned())
        .collect();
    for name in [
        "Cargo check",
        "Clippy",
        "Rustfmt",
        "Cargo test",
        "Cargo audit",
    ] {
        assert!(names.iter().any(|actual| actual == name), "{names:?}");
    }
    assert!(!names.iter().any(|name| name == "ESLint"));
    assert_eq!(
        crate::checks::CargoCheck.check_eligibility(config),
        CheckEligibility::Run
    );
    let scan = config.scan_dir_override.as_ref().unwrap();
    assert_eq!(
        crate::checks::planned_cargo_cwd(config, scan),
        scan.join(relative)
    );
    assert_eq!(
        std::fs::read_to_string(scan.join(relative).join("src/lib.rs")).unwrap(),
        "pub fn pinned() {}\n"
    );
}

#[test]
fn gitlink_profile_and_cargo_plan_ignore_operator_head_dirty_and_deleted_manifest() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let super_repo = init(root);
    let backend = root.join("backend");
    let sub = init(&backend);
    cargo_project(&backend);
    let pinned = commit(&sub);
    for cargo_root in ["backend", "./backend/"] {
        std::fs::write(
            root.join("prview.toml"),
            format!("[project]\ncargo_root = '{cargo_root}'\n"),
        )
        .unwrap();
        commit(&super_repo);
        let target = pin(&super_repo, "backend", pinned);
        for operator_state in ["clean", "other-head", "dirty", "deleted"] {
            match operator_state {
                "other-head" => {
                    std::fs::remove_file(backend.join("Cargo.toml")).unwrap();
                    std::fs::write(backend.join("package.json"), "{}\n").unwrap();
                    commit(&sub);
                }
                "dirty" => {
                    std::fs::write(backend.join("Cargo.toml"), "operator garbage\n").unwrap()
                }
                "deleted" => std::fs::remove_file(backend.join("Cargo.toml")).unwrap(),
                _ => {}
            }
            // Operator-only markers and manifest must not override the target.
            std::fs::write(root.join("Cargo.toml"), "operator garbage\n").unwrap();
            std::fs::write(root.join("package.json"), "{}\n").unwrap();
            let mut config = exact_config(root, target);
            let ledger = crate::ledger::TaskLedger::new();
            crate::prepare_review_profile(&mut config, &ledger).unwrap();
            assert_rust_target(&config, "backend");
            std::fs::remove_file(root.join("Cargo.toml")).unwrap();
            std::fs::remove_file(root.join("package.json")).unwrap();
        }
        // Restore S for the next normalization case, without altering its objects.
        sub.set_head_detached(pinned).unwrap();
        sub.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
            .unwrap();
    }
}

#[test]
fn gitlink_profile_does_not_invent_cargo_from_operator_only_manifest() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let super_repo = init(root);
    let sub = init(&root.join("backend"));
    std::fs::write(root.join("backend/README.md"), "target has no crate\n").unwrap();
    let pinned = commit(&sub);
    std::fs::write(
        root.join("prview.toml"),
        "[project]\ncargo_root = 'backend'\n",
    )
    .unwrap();
    commit(&super_repo);
    let target = pin(&super_repo, "backend", pinned);
    cargo_project(&root.join("backend"));
    commit(&sub);
    let mut config = exact_config(root, target);
    crate::prepare_review_profile(&mut config, &crate::ledger::TaskLedger::new()).unwrap();
    assert_eq!(config.profile.kind, ProfileKind::Generic);
    assert_eq!(config.profile.cargo_root, None);
    assert_eq!(crate::checks::get_checks_for_profile(&config).len(), 1);
    assert!(matches!(
        crate::checks::CargoCheck.check_eligibility(&config),
        CheckEligibility::Skip(_)
    ));
}

#[test]
fn gitlink_profile_follows_nested_pins_instead_of_nested_head() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let super_repo = init(root);
    let sub = init(&root.join("backend"));
    let nested = init(&root.join("backend/engine"));
    cargo_project(&root.join("backend/engine"));
    let engine_s = commit(&nested);
    commit(&sub);
    let backend_s = pin(&sub, "engine", engine_s);
    std::fs::write(
        root.join("prview.toml"),
        "[project]\ncargo_root = './backend/engine/'\n",
    )
    .unwrap();
    commit(&super_repo);
    let target = pin(&super_repo, "backend", backend_s);
    std::fs::remove_file(root.join("backend/engine/Cargo.toml")).unwrap();
    commit(&nested);
    commit(&sub);
    let mut config = exact_config(root, target);
    let ledger = crate::ledger::TaskLedger::new();
    crate::prepare_review_profile(&mut config, &ledger).unwrap();
    assert_rust_target(&config, "backend/engine");
}

#[test]
fn gitlink_profile_detects_conventional_submodule_crate_root() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let super_repo = init(root);
    let sub = init(&root.join("service-rs"));
    cargo_project(&root.join("service-rs"));
    let pinned = commit(&sub);
    commit(&super_repo);
    let target = pin(&super_repo, "service-rs", pinned);
    let mut config = exact_config(root, target);
    let ledger = crate::ledger::TaskLedger::new();
    crate::prepare_review_profile(&mut config, &ledger).unwrap();
    assert_rust_target(&config, "service-rs");
}

#[test]
fn gitlink_cargo_plan_accepts_pinned_manifest_with_manual_rust_profile() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let super_repo = init(root);
    let sub = init(&root.join("backend"));
    cargo_project(&root.join("backend"));
    let pinned = commit(&sub);
    commit(&super_repo);
    let target = pin(&super_repo, "backend", pinned);
    let mut config = exact_config(root, target);
    config.requested_profile = None;
    config.profile = super::test_rust_profile(true);
    config.profile.cargo_root = Some(root.join("backend"));
    config.profile.rust_dirs = vec![root.join("backend")];
    let ledger = crate::ledger::TaskLedger::new();
    crate::prepare_review_profile(&mut config, &ledger).unwrap();
    assert_rust_target(&config, "backend");
}

#[test]
fn gitlink_profile_missing_pinned_object_is_an_error_not_generic() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let super_repo = init(root);
    let sub = init(&root.join("backend"));
    cargo_project(&root.join("backend"));
    commit(&sub);
    std::fs::write(
        root.join("prview.toml"),
        "[project]\ncargo_root = 'backend'\n",
    )
    .unwrap();
    commit(&super_repo);
    let missing = git2::Oid::from_str("1111111111111111111111111111111111111111").unwrap();
    let target = pin(&super_repo, "backend", missing);
    let mut config = exact_config(root, target);
    let error = config
        .refresh_profile_from_target(&target.to_string())
        .expect_err("an unavailable gitlink must fail closed");
    let message = format!("{error:#}");
    assert!(message.contains("backend"), "{message}");
    assert!(message.contains(&missing.to_string()), "{message}");
}

#[test]
fn gitlink_profile_uses_absorbed_module_store_without_operator_checkout() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let super_repo = init(root);
    let backend = root.join("backend");
    let sub = init(&backend);
    cargo_project(&backend);
    let pinned = commit(&sub);
    std::fs::write(
        root.join("prview.toml"),
        "[project]\ncargo_root = 'backend'\n",
    )
    .unwrap();
    commit(&super_repo);
    let target = pin(&super_repo, "backend", pinned);
    // Model submodule absorbgitdirs followed by an operator checkout removal.
    let store = super_repo.path().join("modules/backend");
    std::fs::create_dir_all(store.parent().unwrap()).unwrap();
    drop(sub);
    std::fs::rename(backend.join(".git"), &store).unwrap();
    git2::Repository::open(&store)
        .unwrap()
        .config()
        .unwrap()
        .set_str("core.worktree", backend.to_str().unwrap())
        .unwrap();
    std::fs::remove_dir_all(&backend).unwrap();
    let mut config = exact_config(root, target);
    let ledger = crate::ledger::TaskLedger::new();
    crate::prepare_review_profile(&mut config, &ledger).unwrap();
    assert_rust_target(&config, "backend");
}

#[cfg(unix)]
#[test]
fn gitlink_profile_rejects_manifest_symlink_and_cargo_root_symlink_component() {
    use std::os::unix::fs::symlink;

    for (link, cargo_root) in [("Cargo.toml", "backend"), ("linked", "backend/linked")] {
        let tmp = tempfile::tempdir().unwrap();
        let external = tempfile::tempdir().unwrap();
        cargo_project(external.path());
        let root = tmp.path();
        let super_repo = init(root);
        let backend = root.join("backend");
        let sub = init(&backend);
        let destination = if link == "Cargo.toml" {
            external.path().join("Cargo.toml")
        } else {
            external.path().to_path_buf()
        };
        symlink(destination, backend.join(link)).unwrap();
        let pinned = commit(&sub);
        std::fs::write(
            root.join("prview.toml"),
            format!("[project]\ncargo_root = '{cargo_root}'\n"),
        )
        .unwrap();
        commit(&super_repo);
        let target = pin(&super_repo, "backend", pinned);
        let mut config = exact_config(root, target);
        crate::prepare_review_profile(&mut config, &crate::ledger::TaskLedger::new()).unwrap();
        assert_eq!(config.profile.kind, ProfileKind::Generic, "{link}");
        assert_eq!(config.profile.cargo_root, None, "{link}");
        assert!(matches!(
            crate::checks::CargoCheck.check_eligibility(&config),
            CheckEligibility::Skip(_)
        ));
    }
}
