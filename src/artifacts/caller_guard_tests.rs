//! Pure context/publication boundary controls. No tools or scanner processes.
use super::*;
use crate::checks::caller_guard_tests::fixture;
use std::sync::Arc;

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
    let _scope = ledger.output_scope(Some(pack.clone()));
    for relative in CANCELLED_GENERATION_SUCCESS_SURFACES {
        let path = pack.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        crate::owned_output::fs::write(path, "synthetic pinned PASS").unwrap();
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
fn issue69_caller_guard_internal_output_uses_exact_leaves_and_protects_tracked_data() {
    let (root, home, config) = fixture();
    let pack = root.path().join("new-pack");
    std::fs::create_dir(&pack).unwrap();
    let tracked = pack.join("user-source.txt");
    std::fs::write(&tracked, "tracked source\n").unwrap();
    let repo = git2::Repository::open(root.path()).unwrap();
    let mut index = repo.index().unwrap();
    index
        .add_path(Path::new("new-pack/user-source.txt"))
        .unwrap();
    index.write().unwrap();
    let guard = crate::checks::caller_integrity::CallerGuard::capture_sync(
        config.repo_root,
        home.path().join("diagnostics"),
    );
    let _scope = crate::owned_output::enter(Arc::clone(&guard), Some(pack.clone()));
    crate::owned_output::fs::write(pack.join("own-report.json"), "{}\n").unwrap();
    guard.verify_sync("after-owned-report", None).unwrap();
    assert!(crate::owned_output::fs::write(&tracked, "overwrite\n").is_err());
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
fn issue69_caller_guard_tracked_latest_is_protected_without_parent_refusal() {
    let (root, home, config) = fixture();
    let latest = root.path().join("latest");
    std::os::unix::fs::symlink(home.path(), &latest).unwrap();
    let repo = git2::Repository::open(root.path()).unwrap();
    let mut index = repo.index().unwrap();
    index.add_path(Path::new("latest")).unwrap();
    index.write().unwrap();
    let guard = crate::checks::caller_integrity::CallerGuard::capture_sync(
        config.repo_root,
        home.path().join("diagnostics"),
    );
    let _scope = crate::owned_output::enter(Arc::clone(&guard), Some(root.path().to_path_buf()));
    assert!(crate::owned_output::admit_validated_mutable(&latest).is_err());
    assert_eq!(std::fs::read_link(latest).unwrap(), home.path());
}

#[cfg(unix)]
fn transaction_drift_control(replace_latest_with_foreign: bool) {
    let (root, home, config) = fixture();
    let _home = crate::config::override_test_prview_home(home.path());
    let previous = root.path().join("old-pack");
    let out = root.path().join("new-pack");
    std::fs::create_dir_all(previous.join("00_summary")).unwrap();
    std::fs::create_dir_all(out.join("00_summary")).unwrap();
    std::fs::write(
        previous.join("00_summary/RUN.json"),
        serde_json::to_vec(&serde_json::json!({"artifacts_root":previous})).unwrap(),
    )
    .unwrap();
    let latest = root.path().join("latest");
    std::os::unix::fs::symlink("old-pack", &latest).unwrap();
    let ledger = TaskLedger::new();
    ledger.initialize_caller_guard_config_sync(&config).unwrap();
    let _scope = ledger.output_scope(Some(out.clone()));
    crate::owned_output::fs::write(
        out.join("00_summary/RUN.json"),
        serde_json::to_vec(&serde_json::json!({"artifacts_root":out})).unwrap(),
    )
    .unwrap();
    let publication = crate::storage::acquire_publication_lock(|| false).unwrap();
    let transaction = begin_latest_publication(&publication, &out).unwrap();
    assert_eq!(std::fs::read_link(&latest).unwrap(), Path::new("new-pack"));
    std::fs::write(root.path().join("source.txt"), "foreign source\n").unwrap();
    assert!(
        ledger
            .verify_caller_sync("before-index-commit", None)
            .is_err()
    );
    if replace_latest_with_foreign {
        std::fs::remove_file(&latest).unwrap();
        std::os::unix::fs::symlink("foreign-pack", &latest).unwrap();
    }
    let rollback = rollback_latest_publication(&transaction);
    drop(publication);
    if replace_latest_with_foreign {
        assert!(rollback.is_err());
        assert_eq!(
            std::fs::read_link(&latest).unwrap(),
            Path::new("foreign-pack")
        );
        assert!(home.path().join("publication-transaction.json").is_file());
    } else {
        assert!(
            rollback.is_ok(),
            "actual transaction rollback must work after sticky source drift: {rollback:?}"
        );
        assert_eq!(std::fs::read_link(&latest).unwrap(), Path::new("old-pack"));
        assert!(!home.path().join("publication-transaction.json").exists());
    }
    assert!(
        !home.path().join("index.jsonl.lock").exists(),
        "exact own legacy sentinel must be released even after drift"
    );
    assert!(ledger.ensure_caller_valid().is_err());
    assert_eq!(
        std::fs::read_to_string(root.path().join("source.txt")).unwrap(),
        "foreign source\n"
    );
}

#[cfg(unix)]
#[test]
fn issue69_caller_guard_transaction_drift_rolls_back_latest_and_releases_owned_lock() {
    transaction_drift_control(false);
}
#[cfg(unix)]
#[test]
fn issue69_caller_guard_transaction_foreign_latest_is_preserved_after_drift() {
    transaction_drift_control(true);
}

#[cfg(unix)]
#[test]
fn issue69_caller_guard_external_invalid_journal_quarantine_keeps_source_valid() {
    let (_root, home, config) = fixture();
    let _home = crate::config::override_test_prview_home(home.path());
    let payload = b"not a publication journal\n";
    let journal = home.path().join("publication-transaction.json");
    std::fs::write(&journal, payload).unwrap();
    let ledger = TaskLedger::new();
    ledger.initialize_caller_guard_config_sync(&config).unwrap();
    let _scope = ledger.output_scope(None);
    let publication = crate::storage::acquire_publication_lock(|| false).unwrap();
    let recovery = recover_latest_publication(&publication);
    assert!(
        recovery.is_ok(),
        "validated external invalid journal must remain recoverable under active accounting: {recovery:?}"
    );
    assert!(!journal.exists());
    let preserved: Vec<_> = std::fs::read_dir(home.path())
        .unwrap()
        .map(|entry| entry.unwrap())
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with(".publication-transaction.invalid.")
        })
        .map(|entry| std::fs::read(entry.path()).unwrap())
        .collect();
    assert_eq!(preserved, vec![payload.to_vec()]);
    ledger
        .verify_caller_sync("after-external-quarantine", None)
        .unwrap();
    drop(publication);
    assert!(!home.path().join("index.jsonl.lock").exists());
    ledger.ensure_caller_valid().unwrap();
}

#[test]
fn issue72_review_success_result_cannot_hide_latched_caller_drift() {
    let (root, home, config) = fixture();
    let _home = crate::config::override_test_prview_home(home.path());
    let ledger = TaskLedger::new();
    ledger.initialize_caller_guard_config_sync(&config).unwrap();
    let pack = home.path().join("pack");
    let _scope = ledger.output_scope(Some(pack.clone()));
    std::fs::create_dir_all(pack.join("00_summary")).unwrap();
    crate::owned_output::fs::write(pack.join("00_summary/RUN.json"), b"own success").unwrap();
    std::fs::write(root.path().join("source.txt"), b"foreign source").unwrap();
    assert!(
        ledger
            .verify_caller_sync("late-cleanup-drift", None)
            .is_err()
    );
    let result = finish_caller_guarded_generation(&ledger, Some(&pack), Ok(pack.clone()));
    assert!(
        result.is_err(),
        "latched drift must override successful inner result"
    );
    assert!(!pack.join("00_summary/RUN.json").exists());
    assert!(pack.join("00_summary/INCOMPLETE.json").is_file());
}

#[cfg(unix)]
#[test]
fn issue72_review_recovery_removes_uncommitted_first_latest() {
    use std::os::unix::ffi::OsStrExt;
    let (_root, home, config) = fixture();
    let _home = crate::config::override_test_prview_home(home.path());
    let pack = home.path().join("first-pack");
    std::fs::create_dir_all(pack.join("00_summary")).unwrap();
    std::fs::write(
        pack.join("00_summary/RUN.json"),
        serde_json::to_vec(&serde_json::json!({"artifacts_root":pack})).unwrap(),
    )
    .unwrap();
    let latest = home.path().join("latest");
    std::os::unix::fs::symlink("first-pack", &latest).unwrap();
    let journal = home.path().join("publication-transaction.json");
    std::fs::write(&journal, serde_json::to_vec(&serde_json::json!({"schema":1,"out_dir":pack.as_os_str().as_bytes(),"previous_target":null})).unwrap()).unwrap();
    let ledger = TaskLedger::new();
    ledger.initialize_caller_guard_config_sync(&config).unwrap();
    let _scope = ledger.output_scope(None);
    let publication = crate::storage::acquire_publication_lock(|| false).unwrap();
    recover_latest_publication(&publication).unwrap();
    assert!(
        !latest.exists(),
        "first uncommitted publication must not stay advertised"
    );
    assert!(!journal.exists());
    ledger
        .verify_caller_sync("after-first-publication-recovery", None)
        .unwrap();
}

#[cfg(unix)]
#[test]
fn issue72_review_parseable_invalid_journal_quarantine_updates_accounting() {
    use std::os::unix::ffi::OsStrExt;
    let (_root, home, config) = fixture();
    let _home = crate::config::override_test_prview_home(home.path());
    let pack = home.path().join("unknown-schema-pack");
    let journal = home.path().join("publication-transaction.json");
    let payload = serde_json::to_vec(&serde_json::json!({"schema":2,"out_dir":pack.as_os_str().as_bytes(),"previous_target":null})).unwrap();
    std::fs::write(&journal, &payload).unwrap();
    let ledger = TaskLedger::new();
    ledger.initialize_caller_guard_config_sync(&config).unwrap();
    let _scope = ledger.output_scope(None);
    let publication = crate::storage::acquire_publication_lock(|| false).unwrap();
    recover_latest_publication(&publication).unwrap();
    assert!(!journal.exists());
    let preserved: Vec<_> = std::fs::read_dir(home.path())
        .unwrap()
        .map(|entry| entry.unwrap())
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with(".publication-transaction.invalid.")
        })
        .map(|entry| std::fs::read(entry.path()).unwrap())
        .collect();
    assert_eq!(preserved, vec![payload]);
    ledger
        .verify_caller_sync("after-admitted-invalid-journal-quarantine", None)
        .unwrap();
}

#[cfg(unix)]
#[test]
fn issue72_review_followup_late_committed_drift_revokes_own_advertisements() {
    for foreign_role in ["journal", "index", "latest"] {
        let (_root, home, config) = fixture();
        let _home = crate::config::override_test_prview_home(home.path());
        let pack = home.path().join("committed-pack");
        std::fs::create_dir_all(pack.join("00_summary")).unwrap();
        let ledger = TaskLedger::new();
        ledger.initialize_caller_guard_config_sync(&config).unwrap();
        let _scope = ledger.output_scope(Some(pack.clone()));
        crate::owned_output::fs::write(
            pack.join("00_summary/RUN.json"),
            serde_json::to_vec(&serde_json::json!({"artifacts_root":pack})).unwrap(),
        )
        .unwrap();
        let publication = crate::storage::acquire_publication_lock(|| false).unwrap();
        let transaction = begin_latest_publication(&publication, &pack).unwrap();
        let entry: crate::storage::RunEntry = serde_json::from_value(serde_json::json!({
            "id":"own-run", "repo":"fixture", "branch":"main", "commit":"fixture", "path":pack,
            "created_at":"2026-10-17T00:00:00Z", "quality_pass":true, "merge_status":"PASS", "policy_mode":"warn",
            "checks_passed":0, "checks_failed":0, "files_changed":0, "size_bytes":0, "has_dashboard":false
        })).unwrap();
        crate::storage::register_and_prune_locked(&publication, &pack, entry, false, || false)
            .unwrap();
        let index_path = home.path().join("index.jsonl");
        let journal = home.path().join("publication-transaction.json");
        let latest = home.path().join("latest");
        let mut foreign_index = std::fs::read(&index_path).unwrap();
        foreign_index.extend_from_slice(b"\n");
        if foreign_role == "index" {
            std::fs::write(&index_path, &foreign_index).unwrap();
        }
        if foreign_role == "latest" {
            std::fs::remove_file(&latest).unwrap();
            std::os::unix::fs::symlink("foreign-pack", &latest).unwrap();
        }
        let foreign_journal = b"foreign journal diagnostic\n";
        std::fs::write(&journal, foreign_journal).unwrap();
        let inner = finish_committed_publication(&publication, &transaction, &ledger, &pack)
            .map(|()| pack.clone());
        let result = finish_caller_guarded_generation(&ledger, Some(&pack), inner);
        assert!(
            result.is_err(),
            "late committed drift cannot return success"
        );
        let detail = format!("{:#}", result.unwrap_err());
        assert!(!pack.join("00_summary/RUN.json").exists());
        assert!(pack.join("00_summary/INCOMPLETE.json").exists());
        assert_eq!(std::fs::read(&journal).unwrap(), foreign_journal);
        if foreign_role == "index" {
            assert_eq!(std::fs::read(&index_path).unwrap(), foreign_index);
            assert!(detail.contains("revocation unconfirmed"));
        } else {
            assert!(
                !crate::storage::RunIndex::load_strict()
                    .unwrap()
                    .entries()
                    .iter()
                    .any(|entry| entry.path == pack),
                "own committed row remains advertised"
            );
        }
        if foreign_role == "latest" {
            assert_eq!(
                std::fs::read_link(&latest).unwrap(),
                Path::new("foreign-pack")
            );
            assert!(detail.contains("revocation unconfirmed"));
        } else {
            assert!(
                std::fs::symlink_metadata(&latest).is_err(),
                "own committed alias remains advertised"
            );
        }
        ledger.ensure_caller_valid().unwrap_err();
    }
}

#[cfg(windows)]
fn windows_review_entry(pack: &Path) -> crate::storage::RunEntry {
    serde_json::from_value(serde_json::json!({
        "id":"windows-own-run", "repo":"fixture", "branch":"main", "commit":"fixture", "path":pack,
        "created_at":"2026-10-17T00:00:00Z", "quality_pass":true, "merge_status":"PASS", "policy_mode":"warn",
        "checks_passed":0, "checks_failed":0, "files_changed":0, "size_bytes":0, "has_dashboard":false
    })).unwrap()
}

#[cfg(windows)]
#[test]
fn issue72_review_windows_index_sync_uses_writable_handle() {
    let (_root, home, _config) = fixture();
    let _home = crate::config::override_test_prview_home(home.path());
    let mut index = crate::storage::RunIndex::load_strict().unwrap();
    index.append(windows_review_entry(&home.path().join("pack")));
    index.save().unwrap();
    index.save().unwrap();
    assert_eq!(
        crate::storage::RunIndex::load_strict()
            .unwrap()
            .entries()
            .len(),
        1
    );
}

#[cfg(windows)]
#[test]
fn issue72_review_windows_internal_output_and_held_publication_handle_are_accounted() {
    let (root, _diagnostics, config) = fixture();
    let home = root.path().join("internal-home");
    std::fs::create_dir(&home).unwrap();
    let _home = crate::config::override_test_prview_home(&home);
    let pack = root.path().join("pack/00_summary");
    std::fs::create_dir_all(&pack).unwrap();
    let ledger = TaskLedger::new();
    ledger.initialize_caller_guard_config_sync(&config).unwrap();
    let _scope = ledger.output_scope(Some(root.path().join("pack")));
    crate::owned_output::fs::write(pack.join("report.json"), b"own nested report").unwrap();
    ledger
        .verify_caller_sync("windows-git-relative-owned-filter", None)
        .unwrap();
    let publication = crate::storage::acquire_publication_lock(|| false).unwrap();
    ledger
        .verify_caller_sync("windows-held-lock-content-observation", None)
        .unwrap();
    let mut index = crate::storage::RunIndex::load_strict().unwrap();
    index.append(windows_review_entry(&root.path().join("pack")));
    index.save().unwrap();
    let v2 = home.join("index.jsonl.lock.v2");
    let replacement = pack.join("replacement-lock");
    crate::owned_output::fs::write(&replacement, b"own replacement candidate").unwrap();
    assert!(std::fs::remove_file(&v2).is_err());
    assert!(std::fs::rename(&v2, home.join("moved-v2")).is_err());
    assert!(std::fs::rename(&replacement, &v2).is_err());
    ledger
        .verify_caller_sync("windows-held-lock-after-replacement-refusals", None)
        .unwrap();
    drop(publication);
    assert!(!home.join("index.jsonl.lock").exists());
    ledger
        .verify_caller_sync("windows-publication-released", None)
        .unwrap();
    let again = crate::storage::acquire_publication_lock(|| false).unwrap();
    ledger
        .verify_caller_sync("windows-publication-reacquired", None)
        .unwrap();
    drop(again);
    assert!(!home.join("index.jsonl.lock").exists());
    ledger
        .verify_caller_sync("windows-publication-released-again", None)
        .unwrap();
}

#[cfg(windows)]
#[test]
fn issue72_review_windows_failed_context_admission_closes_raw_handles() {
    for fail_stderr in [false, true] {
        let (root, diagnostics, config) = fixture();
        let pack = root.path().join("context-pack");
        std::fs::create_dir(&pack).unwrap();
        let stderr = pack.join(".context-cmd-0.stderr.tmp");
        if fail_stderr {
            std::fs::write(&stderr, b"initial user diagnostics").unwrap();
        }
        let guard = crate::checks::caller_integrity::CallerGuard::capture_sync(
            config.repo_root,
            diagnostics.path().join("diagnostics"),
        );
        let _scope = crate::owned_output::enter(Arc::clone(&guard), Some(pack.clone()));
        let command = context_artifacts::ContextCmd {
            label: "missing owned context executable".to_owned(),
            gate: None,
            cmd: pack
                .join("nonexistent-context-executable.exe")
                .to_string_lossy()
                .into_owned(),
            args: vec![],
            cwd: root.path().to_path_buf(),
            out_dir: pack.clone(),
            out_file: String::new(),
        };
        let governor = crate::governor::ResourceGovernor::new();
        let timings =
            context_artifacts::run_context_cmds_parallel(&[command], 3, false, &governor).unwrap();
        assert_eq!(timings.len(), 1);
        assert_eq!(timings[0].status, "spawn_failed");
        assert!(!pack.join(".context-cmd-0.stdout.tmp").exists());
        if fail_stderr {
            assert_eq!(std::fs::read(&stderr).unwrap(), b"initial user diagnostics");
        } else {
            assert!(!stderr.exists());
        }
        guard
            .verify_sync("after-windows-context-failure-cleanup", None)
            .unwrap();
    }
}
