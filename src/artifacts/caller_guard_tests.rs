//! Pure context/publication boundary controls. No tools or scanner processes.
use super::*;
use crate::checks::caller_guard_tests::fixture;

#[test]
fn issue69_caller_guard_context_mock_admission_and_completion() {
    let (root, home, config) = fixture();
    let _home = crate::config::override_test_prview_home(home.path());
    let ledger = crate::ledger::TaskLedger::new();
    ledger
        .initialize_caller_guard_sync(&config.repo_root)
        .unwrap();
    context_caller_boundary(Some(&ledger), "before-context-command", Some("mock")).unwrap();
    // The synthetic command is just a byte write between the same hooks used
    // by the queue; no command is spawned by this control.
    std::fs::write(root.path().join("source.txt"), "modified\n").unwrap();
    assert!(context_caller_boundary(Some(&ledger), "after-context-command", Some("mock")).is_err());
    assert!(
        context_caller_boundary(Some(&ledger), "before-context-command", Some("queued mock"))
            .is_err()
    );
}

#[test]
fn issue69_caller_guard_publication_mock_rejects_and_clears_success_surfaces() {
    let (root, home, config) = fixture();
    let _home = crate::config::override_test_prview_home(home.path());
    let ledger = crate::ledger::TaskLedger::new();
    ledger
        .initialize_caller_guard_sync(&config.repo_root)
        .unwrap();
    let pack = home.path().join("owned-unpublished-pack");
    for relative in CANCELLED_GENERATION_SUCCESS_SURFACES {
        let path = pack.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "synthetic pinned PASS").unwrap();
    }
    let index = home.path().join("index.jsonl");
    let latest = home.path().join("latest");
    std::fs::write(&index, "old history\n").unwrap();
    std::fs::write(&latest, "old run\n").unwrap();
    std::fs::write(root.path().join("source.txt"), "modified\n").unwrap();
    let error = ledger
        .verify_caller_sync("before-pack-publication", None)
        .unwrap_err();
    assert!(finish_caller_guarded_generation(&ledger, Some(&pack), Err(error)).is_err());
    for relative in CANCELLED_GENERATION_SUCCESS_SURFACES {
        assert!(!pack.join(relative).exists(), "stale success: {relative}");
    }
    let marker: serde_json::Value =
        serde_json::from_slice(&std::fs::read(pack.join("00_summary/INCOMPLETE.json")).unwrap())
            .unwrap();
    assert_eq!(marker["reason"], "caller drift");
    assert_eq!(std::fs::read_to_string(index).unwrap(), "old history\n");
    assert_eq!(std::fs::read_to_string(latest).unwrap(), "old run\n");
}

#[test]
fn issue69_caller_guard_output_variant_a_refuses_non_source_namespace_before_write() {
    use crate::checks::caller_integrity::validate_output_namespace;
    let (root, home, _) = fixture();
    let unignored = root.path().join("new-pack");
    assert!(validate_output_namespace(root.path(), &unignored).is_err());
    assert!(
        !unignored.exists(),
        "preflight must not allocate the refused namespace"
    );
    let external = home.path().join("external-pack");
    validate_output_namespace(root.path(), &external).unwrap();
    assert!(!external.exists(), "preflight is read-only");
    validate_output_namespace(root.path(), &root.path().join(".generated/pack")).unwrap();
    let tracked = root.path().join(".generated/source.txt");
    std::fs::create_dir_all(tracked.parent().unwrap()).unwrap();
    std::fs::write(&tracked, "tracked source\n").unwrap();
    let repo = git2::Repository::open(root.path()).unwrap();
    let mut index = repo.index().unwrap();
    index.add_path(Path::new(".generated/source.txt")).unwrap();
    index.write().unwrap();
    assert!(validate_output_namespace(root.path(), &root.path().join(".generated")).is_err());
    let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
    let parent = repo.head().unwrap().peel_to_commit().unwrap();
    let sig = git2::Signature::now("Fixture", "fixture@example.invalid").unwrap();
    repo.commit(
        Some("HEAD"),
        &sig,
        &sig,
        "tracked ignored source",
        &tree,
        &[&parent],
    )
    .unwrap();
    index
        .remove_path(Path::new(".generated/source.txt"))
        .unwrap();
    index.write().unwrap();
    assert!(
        validate_output_namespace(root.path(), &root.path().join(".generated")).is_err(),
        "HEAD tracked source must also be refused"
    );
    assert_eq!(
        std::fs::read_to_string(tracked).unwrap(),
        "tracked source\n"
    );
}

#[tokio::test]
async fn issue69_caller_guard_quick_init_keeps_quality_baseline() {
    let (root, home, config) = fixture();
    let _home = crate::config::override_test_prview_home(home.path());
    let ledger = crate::ledger::TaskLedger::new();
    ledger
        .initialize_caller_guard(&config.repo_root)
        .await
        .unwrap();
    std::fs::write(root.path().join("source.txt"), "modified\n").unwrap();
    assert!(
        ledger
            .initialize_caller_guard_sync(&config.repo_root)
            .is_err(),
        "quick/artifact entry must not reset the quality-stage baseline"
    );
    let fresh_quick = crate::ledger::TaskLedger::new();
    fresh_quick
        .initialize_caller_guard_sync(&config.repo_root)
        .unwrap();
    std::fs::write(root.path().join("source.txt"), "changed again\n").unwrap();
    assert!(
        fresh_quick
            .verify_caller_sync("quick-publication", None)
            .is_err()
    );
}

#[test]
fn issue69_caller_guard_abort_retains_partial_raw_diagnostics() {
    let (root, home, config) = fixture();
    let _home = crate::config::override_test_prview_home(home.path());
    let ledger = crate::ledger::TaskLedger::new();
    ledger
        .initialize_caller_guard_sync(&config.repo_root)
        .unwrap();
    let pack = home.path().join("unpublished-partial");
    let context = pack.join("30_context");
    std::fs::create_dir_all(&context).unwrap();
    let stdout = context.join(".context-cmd-0.stdout.tmp");
    let stderr = context.join(".context-cmd-0.stderr.tmp");
    std::fs::write(&stdout, b"raw stdout\xff").unwrap();
    std::fs::write(&stderr, b"raw failure\x80").unwrap();
    std::fs::write(root.path().join("source.txt"), "modified\n").unwrap();
    let drift = ledger
        .verify_caller_sync("after-context-command", Some("mock"))
        .unwrap_err();
    let error = finish_caller_guarded_generation(&ledger, Some(&pack), Err(drift)).unwrap_err();
    assert!(error.to_string().contains(&pack.display().to_string()));
    assert_eq!(std::fs::read(stdout).unwrap(), b"raw stdout\xff");
    assert_eq!(std::fs::read(stderr).unwrap(), b"raw failure\x80");
    let receipt: serde_json::Value =
        serde_json::from_slice(&std::fs::read(pack.join("00_summary/INCOMPLETE.json")).unwrap())
            .unwrap();
    assert_eq!(receipt["partial_output_dir"], pack.to_str().unwrap());
    assert!(!pack.join("00_summary/RUN.json").exists());
}

#[test]
fn issue69_caller_guard_context_admission_rechecks_expired_deadline() {
    let (_root, home, config) = fixture();
    let _home = crate::config::override_test_prview_home(home.path());
    let ledger = crate::ledger::TaskLedger::new();
    ledger
        .initialize_caller_guard_sync(&config.repo_root)
        .unwrap();
    let governor = crate::governor::ResourceGovernor::with_budget(1, 1);
    let expired = Instant::now();
    let mut mock_started = false;
    if context_command_admission(Some(&ledger), &governor, expired, "mock").unwrap() {
        mock_started = true;
    }
    assert!(!mock_started);
    assert!(
        !governor.is_cancelled(),
        "stage timeout does not invent run cancellation"
    );
    assert!(
        context_command_admission(
            Some(&ledger),
            &governor,
            Instant::now() + std::time::Duration::from_secs(60),
            "mock control"
        )
        .unwrap()
    );
}

#[cfg(unix)]
#[test]
fn issue69_caller_guard_publication_parent_namespace_refuses_source_siblings() {
    use crate::checks::caller_integrity::validate_storage_namespaces;
    let (root, home, mut config) = fixture();
    let _home = crate::config::override_test_prview_home(home.path());
    config.output_dir = Some(root.path().join(".generated"));
    assert!(
        validate_storage_namespaces(&config).is_err(),
        "ignored pack alone cannot authorize nonignored sibling latest writes"
    );
    assert!(!root.path().join(".generated").exists());
    config.output_dir = Some(root.path().join(".generated/pack"));
    validate_storage_namespaces(&config).unwrap();
    let latest = root.path().join(".generated/latest");
    std::fs::create_dir_all(latest.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(home.path(), &latest).unwrap();
    let repo = git2::Repository::open(root.path()).unwrap();
    let mut index = repo.index().unwrap();
    index.add_path(Path::new(".generated/latest")).unwrap();
    index.write().unwrap();
    assert!(
        validate_storage_namespaces(&config).is_err(),
        "tracked sibling latest must be refused without following its target"
    );
    assert_eq!(std::fs::read_link(&latest).unwrap(), home.path());
    assert!(!config.output_dir.as_ref().unwrap().exists());
}
