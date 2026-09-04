use super::*;

fn generate_fixture_pack(
    repo_root: &Path,
    output_dir: &Path,
    target_sha: &str,
    base_sha: &str,
    governor: &crate::governor::ResourceGovernor,
) -> Result<PathBuf> {
    let ledger = crate::ledger::TaskLedger::new();
    generate_fixture_pack_with_ledger(
        repo_root, output_dir, target_sha, base_sha, governor, &ledger,
    )
}

fn generate_fixture_pack_with_ledger(
    repo_root: &Path,
    output_dir: &Path,
    target_sha: &str,
    base_sha: &str,
    governor: &crate::governor::ResourceGovernor,
    ledger: &crate::ledger::TaskLedger,
) -> Result<PathBuf> {
    generate_fixture_pack_with_ledger_and_diffs(
        repo_root,
        output_dir,
        target_sha,
        base_sha,
        governor,
        ledger,
        &[],
    )
}

fn generate_fixture_pack_with_ledger_and_diffs(
    repo_root: &Path,
    output_dir: &Path,
    target_sha: &str,
    base_sha: &str,
    governor: &crate::governor::ResourceGovernor,
    ledger: &crate::ledger::TaskLedger,
    diffs: &[Diff],
) -> Result<PathBuf> {
    let mut config = test_config_builder()
        .repo_root(repo_root)
        .target(Some("feature"))
        .bases(&["main"])
        .profile(test_generic_profile())
        .execution_mode(ExecutionMode::Standard)
        .run_tests(false)
        .run_lint(false)
        .do_fetch(false)
        .use_cache(false)
        .create_zip(true)
        .build();
    config.run_bundle = false;
    config.run_security = false;
    config.run_heuristics = false;
    config.create_dashboard = true;
    config.quiet = true;
    config.output_dir = Some(output_dir.to_path_buf());

    let resolved_target = ResolvedRef {
        name: "feature".to_string(),
        commit_id: target_sha.to_string(),
        is_remote: false,
    };
    let resolved_bases = [ResolvedRef {
        name: "main".to_string(),
        commit_id: base_sha.to_string(),
        is_remote: false,
    }];

    generate(GenerateInput {
        config: &config,
        ledger,
        diffs,
        checks: &[],
        heuristics: None,
        resolved_target: &resolved_target,
        resolved_bases: &resolved_bases,
        run_start: Instant::now(),
        skipped_checks: Vec::new(),
        worktree_clean: Some(true),
        worktree_status_digest: None,
        governor,
    })
}

fn assert_no_success_surfaces(output_dir: &Path, seam: ArtifactGenerationSeam) {
    for relative in CANCELLED_GENERATION_SUCCESS_SURFACES {
        let path = output_dir.join(relative);
        assert!(
            !path.exists(),
            "{} survived cancellation at {}",
            path.display(),
            seam.label()
        );
    }
}

/// A cancelled pack must not be advertised as the latest completed review
/// (parent `latest` symlink) or as a row in the run index. Publication is
/// irreversible; the seam check has to run before those side effects.
fn assert_cancelled_pack_is_not_published(output_dir: &Path, seam: ArtifactGenerationSeam) {
    if let Some(parent) = output_dir.parent() {
        let latest = parent.join("latest");
        if latest.exists() {
            let target = fs::read_link(&latest).unwrap_or_default();
            assert_ne!(
                target.as_os_str(),
                output_dir.file_name().unwrap_or_default(),
                "cancelled pack at {} was published as latest ({})",
                seam.label(),
                latest.display()
            );
        }
    }

    let index = crate::config::prview_home().join("index.jsonl");
    if index.exists() {
        let hay = fs::read_to_string(&index).unwrap_or_default();
        let path = output_dir.display().to_string();
        assert!(
            !hay.contains(&path),
            "cancelled pack at {} was registered in the run index: {path}",
            seam.label()
        );
    }
}

#[test]
fn cancellation_injection_stops_every_artifact_generation_seam() {
    let publication_home = tempfile::tempdir().expect("publication home");
    let _publication_home =
        crate::config::override_test_prview_home(publication_home.path().to_path_buf());
    assert_eq!(ArtifactGenerationSeam::ALL.len(), 22);
    let unique_labels: std::collections::HashSet<_> = ArtifactGenerationSeam::ALL
        .iter()
        .map(|seam| seam.label())
        .collect();
    assert_eq!(unique_labels.len(), ArtifactGenerationSeam::ALL.len());

    let (repo, base_sha, target_sha) = init_advanced_base_fixture();
    for (index, seam) in ArtifactGenerationSeam::ALL.iter().copied().enumerate() {
        let output = tempfile::tempdir().expect("output tempdir");
        let output_dir = output.path().join("pack");
        let governor = crate::governor::ResourceGovernor::new();
        let probe = generation_seam_test_hook::ProbeGuard::install(Some(seam));

        let error =
            generate_fixture_pack(repo.path(), &output_dir, &target_sha, &base_sha, &governor)
                .expect_err("injected cancellation must stop artifact generation");
        let observed = probe.observed();
        drop(probe);

        assert!(
            crate::governor::is_cancellation(&error),
            "{} returned {error:#}",
            seam.label()
        );
        assert_eq!(
            observed,
            ArtifactGenerationSeam::ALL[..=index],
            "generation did not stop exactly at {}",
            seam.label()
        );
        if let Some(next) = ArtifactGenerationSeam::ALL.get(index + 1) {
            assert!(
                !observed.contains(next),
                "stage after {} was observed: {}",
                seam.label(),
                next.label()
            );
        }

        assert_no_success_surfaces(&output_dir, seam);
        assert_cancelled_pack_is_not_published(&output_dir, seam);
        let incomplete: serde_json::Value = serde_json::from_str(
            &fs::read_to_string(output_dir.join("00_summary/INCOMPLETE.json"))
                .expect("incomplete marker"),
        )
        .expect("valid incomplete JSON");
        assert_eq!(incomplete["status"], "incomplete");
        assert_eq!(incomplete["reason"], "cancelled");
        assert_eq!(incomplete["stage"], seam.label());
    }
}

#[test]
fn cancellation_at_publication_preserves_existing_latest() {
    let publication_home = tempfile::tempdir().expect("publication home");
    let _publication_home =
        crate::config::override_test_prview_home(publication_home.path().to_path_buf());
    let (repo, base_sha, target_sha) = init_advanced_base_fixture();
    let output = tempfile::tempdir().expect("output tempdir");
    let first = output.path().join("first");
    let governor = crate::governor::ResourceGovernor::new();
    generate_fixture_pack(repo.path(), &first, &target_sha, &base_sha, &governor)
        .expect("completed predecessor pack");

    #[cfg(unix)]
    {
        let latest = output.path().join("latest");
        assert_eq!(
            fs::read_link(&latest).expect("predecessor latest"),
            first.file_name().expect("first basename")
        );
    }

    let second = output.path().join("second");
    let cancel_governor = crate::governor::ResourceGovernor::new();
    let probe = generation_seam_test_hook::ProbeGuard::install(Some(
        ArtifactGenerationSeam::RunIndexPublication,
    ));
    generate_fixture_pack(
        repo.path(),
        &second,
        &target_sha,
        &base_sha,
        &cancel_governor,
    )
    .expect_err("publication seam must stop before advertising the new pack");
    drop(probe);

    #[cfg(unix)]
    {
        let latest = output.path().join("latest");
        assert_eq!(
            fs::read_link(&latest).expect("preserved latest"),
            first.file_name().expect("first basename"),
            "cancel must not retarget latest at the incomplete pack"
        );
    }
    assert_cancelled_pack_is_not_published(&second, ArtifactGenerationSeam::RunIndexPublication);
}

#[test]
fn explicit_output_dir_is_one_immutable_pack_path() {
    let publication_home = tempfile::tempdir().expect("publication home");
    let _publication_home =
        crate::config::override_test_prview_home(publication_home.path().to_path_buf());
    let (repo, base_sha, target_sha) = init_advanced_base_fixture();
    let output = tempfile::tempdir().expect("output tempdir");
    let pack = output.path().join("pack");
    let governor = crate::governor::ResourceGovernor::new();

    generate_fixture_pack(repo.path(), &pack, &target_sha, &base_sha, &governor)
        .expect("first explicit pack claims the path");
    let first_identity = fs::read_to_string(pack.join("00_summary/RUN.json")).unwrap();
    let error = generate_fixture_pack(repo.path(), &pack, &target_sha, &base_sha, &governor)
        .expect_err("a second run cannot overwrite one historical pack path");

    assert!(
        error.to_string().contains("must name a new directory"),
        "got {error:#}"
    );
    assert_eq!(
        fs::read_to_string(pack.join("00_summary/RUN.json")).unwrap(),
        first_identity,
        "the rejected rerun must not mix stale and new artifact files"
    );
    assert_eq!(
        crate::storage::RunIndex::load().entries().len(),
        1,
        "one immutable output path owns one history row"
    );
}

#[test]
fn index_commit_failure_is_fatal_and_does_not_publish_a_completed_run() {
    let publication_home = tempfile::tempdir().expect("publication home");
    let _publication_home =
        crate::config::override_test_prview_home(publication_home.path().to_path_buf());
    let (repo, base_sha, target_sha) = init_advanced_base_fixture();
    let output = tempfile::tempdir().expect("output tempdir");
    let pack = output.path().join("pack");
    let governor = crate::governor::ResourceGovernor::new();
    crate::storage::arm_test_index_save_failure();

    let error = generate_fixture_pack(repo.path(), &pack, &target_sha, &base_sha, &governor)
        .expect_err("an unindexed pack must not be reported as completed");

    assert!(
        error.to_string().contains("run publication failed"),
        "got {error:#}"
    );
    assert!(
        crate::storage::RunIndex::load().entries().is_empty(),
        "failed publication must not manufacture an index row"
    );
    #[cfg(unix)]
    assert!(
        !output.path().join("latest").exists(),
        "failed publication must roll back its latest advertisement"
    );
}

#[test]
fn mcp_output_reservation_is_strict_and_single_use() {
    let tmp = tempfile::tempdir().expect("output root");
    let pack = tmp.path().join("pack");
    fs::create_dir(&pack).expect("exclusive MCP allocation");
    reserve_mcp_output_dir(&pack, "correct-nonce").expect("reservation");
    fs::write(pack.join("run.log"), "launcher output").expect("allowed log");
    fs::write(pack.join("run.stderr.log"), "").expect("allowed stderr");

    claim_explicit_output_dir_with_reservation(&pack, Some("correct-nonce"))
        .expect("matching reservation claims the fresh directory");
    assert!(
        !pack.join(MCP_OUTPUT_RESERVATION_FILE).exists(),
        "the one-shot reservation is consumed"
    );
    claim_explicit_output_dir_with_reservation(&pack, Some("correct-nonce"))
        .expect_err("the same reserved path cannot be claimed twice");

    let wrong_nonce = tmp.path().join("wrong-nonce");
    fs::create_dir(&wrong_nonce).unwrap();
    reserve_mcp_output_dir(&wrong_nonce, "real").unwrap();
    claim_explicit_output_dir_with_reservation(&wrong_nonce, Some("forged"))
        .expect_err("a wrong nonce cannot adopt the directory");
    assert!(
        wrong_nonce.join(MCP_OUTPUT_RESERVATION_FILE).exists(),
        "a failed claim does not consume the real reservation"
    );

    let contaminated = tmp.path().join("contaminated");
    fs::create_dir(&contaminated).unwrap();
    reserve_mcp_output_dir(&contaminated, "nonce").unwrap();
    fs::write(contaminated.join("stale-artifact.json"), "{}").unwrap();
    claim_explicit_output_dir_with_reservation(&contaminated, Some("nonce"))
        .expect_err("unexpected content prevents stale-pack adoption");

    let missing = tmp.path().join("missing");
    fs::create_dir(&missing).unwrap();
    claim_explicit_output_dir_with_reservation(&missing, Some("nonce"))
        .expect_err("a nonce without its create-new sentinel proves nothing");
}

#[test]
fn cancellation_while_waiting_for_publication_lock_finalizes_the_pack_as_incomplete() {
    use std::sync::Arc;
    use std::time::Duration;

    let publication_home = tempfile::tempdir().expect("publication home");
    let _publication_home =
        crate::config::override_test_prview_home(publication_home.path().to_path_buf());
    let (repo, base_sha, target_sha) = init_advanced_base_fixture();
    let output = tempfile::tempdir().expect("output tempdir");
    let first = output.path().join("first");
    let first_governor = crate::governor::ResourceGovernor::new();
    generate_fixture_pack(repo.path(), &first, &target_sha, &base_sha, &first_governor)
        .expect("completed predecessor pack");

    let held_publication = crate::storage::acquire_publication_lock(|| false).unwrap();
    let governor = Arc::new(crate::governor::ResourceGovernor::new());
    let (waiting_tx, waiting_rx) = std::sync::mpsc::channel();
    let _waiting = crate::storage::PublicationLockWaitGuard::install(waiting_tx);
    let canceller = {
        let governor = Arc::clone(&governor);
        std::thread::spawn(move || {
            waiting_rx
                .recv_timeout(Duration::from_secs(5))
                .expect("run never reached the busy publication lock");
            governor.cancel();
        })
    };
    let second = output.path().join("second");
    let error = generate_fixture_pack(
        repo.path(),
        &second,
        &target_sha,
        &base_sha,
        governor.as_ref(),
    )
    .expect_err("a run waiting for the publication lock must observe cancellation");
    canceller.join().unwrap();
    drop(held_publication);

    assert!(crate::governor::is_cancellation(&error), "{error:#}");
    assert_no_success_surfaces(&second, ArtifactGenerationSeam::RunIndexPublication);
    assert_cancelled_pack_is_not_published(&second, ArtifactGenerationSeam::RunIndexPublication);
    let incomplete: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(second.join("00_summary/INCOMPLETE.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        incomplete["stage"],
        ArtifactGenerationSeam::RunIndexPublication.label()
    );
    #[cfg(unix)]
    assert_eq!(
        fs::read_link(output.path().join("latest")).unwrap(),
        PathBuf::from("first")
    );
}

#[test]
fn cancellation_after_latest_symlink_restores_predecessor() {
    let publication_home = tempfile::tempdir().expect("publication home");
    let _publication_home =
        crate::config::override_test_prview_home(publication_home.path().to_path_buf());
    let (repo, base_sha, target_sha) = init_advanced_base_fixture();
    let output = tempfile::tempdir().expect("output tempdir");
    let first = output.path().join("first");
    let governor = crate::governor::ResourceGovernor::new();
    generate_fixture_pack(repo.path(), &first, &target_sha, &base_sha, &governor)
        .expect("completed predecessor pack");

    #[cfg(unix)]
    {
        let latest = output.path().join("latest");
        assert_eq!(
            fs::read_link(&latest).expect("predecessor latest"),
            first.file_name().expect("first basename")
        );
    }

    let second = output.path().join("second");
    let cancel_governor = crate::governor::ResourceGovernor::new();
    let probe = generation_seam_test_hook::ProbeGuard::install(Some(
        ArtifactGenerationSeam::LatestAdvertisement,
    ));
    generate_fixture_pack(
        repo.path(),
        &second,
        &target_sha,
        &base_sha,
        &cancel_governor,
    )
    .expect_err("latest advertisement seam must restore the predecessor alias");
    drop(probe);

    #[cfg(unix)]
    {
        let latest = output.path().join("latest");
        assert_eq!(
            fs::read_link(&latest).expect("restored latest"),
            first.file_name().expect("first basename"),
            "cancel after writing latest must restore the predecessor, not leave the incomplete pack advertised"
        );
    }
    assert_cancelled_pack_is_not_published(&second, ArtifactGenerationSeam::LatestAdvertisement);
}

#[test]
fn rollback_failure_keeps_cancellation_identity_and_detail() {
    let error = preserve_primary_error_after_latest_rollback(
        crate::governor::Cancelled.into(),
        Err(anyhow::anyhow!("rollback storage fault")),
    );

    assert!(
        crate::governor::is_cancellation(&error),
        "rollback context must not replace the typed cancellation: {error:#}"
    );
    assert!(
        format!("{error:#}").contains("rollback storage fault"),
        "rollback detail must remain observable: {error:#}"
    );
}

#[test]
fn cancellation_that_arrives_during_rollback_remains_typed() {
    let governor = crate::governor::ResourceGovernor::new();
    let cancellation_before_rollback = governor.is_cancelled();
    governor.cancel();

    let error = publication_failure_after_rollback(
        anyhow::anyhow!("publication storage fault"),
        cancellation_before_rollback,
        Err(anyhow::anyhow!("rollback storage fault")),
        &governor,
    );

    assert!(
        crate::governor::is_cancellation(&error),
        "cancellation observed after rollback must remain typed: {error:#}"
    );
    let detail = format!("{error:#}");
    assert!(detail.contains("publication storage fault"), "{detail}");
    assert!(detail.contains("rollback storage fault"), "{detail}");
}

#[cfg(unix)]
#[tokio::test]
async fn cancellation_during_shared_snapshot_cleanup_never_publishes_the_pack() {
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Arc;
    use std::time::Duration;

    let publication_home = tempfile::tempdir().expect("publication home");
    let _publication_home =
        crate::config::override_test_prview_home(publication_home.path().to_path_buf());
    let (repo, base_sha, target_sha) = init_advanced_base_fixture();
    let output = tempfile::tempdir().expect("output tempdir");
    let first = output.path().join("first");
    let first_governor = crate::governor::ResourceGovernor::new();
    generate_fixture_pack(repo.path(), &first, &target_sha, &base_sha, &first_governor)
        .expect("completed predecessor pack");

    let snapshot = crate::git::create_worktree_snapshot(repo.path(), &target_sha)
        .expect("shared target snapshot");
    let ledger = crate::ledger::TaskLedger::new();
    ledger.set_shared_snapshot(Some(snapshot));

    let pids = output.path().join("cleanup.pids");
    let shim = output.path().join("blocking-git");
    std::fs::write(
        &shim,
        format!(
            "#!/bin/sh\nif [ \"$1\" = worktree ] && [ \"$2\" = remove ]; then\n  sleep 30 &\n  printf '%s %s\\n' \"$$\" \"$!\" > '{}'\n  wait\nfi\nexec git \"$@\"\n",
            pids.display()
        ),
    )
    .unwrap();
    let mut permissions = std::fs::metadata(&shim).unwrap().permissions();
    permissions.set_mode(0o700);
    std::fs::set_permissions(&shim, permissions).unwrap();

    let governor = Arc::new(crate::governor::ResourceGovernor::new());
    let canceller = {
        let governor = Arc::clone(&governor);
        let pids = pids.clone();
        std::thread::spawn(move || {
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            while crate::proc::read_published_unix_pids(&pids, 2).is_none() {
                assert!(
                    std::time::Instant::now() < deadline,
                    "snapshot cleanup never spawned its governed git child"
                );
                std::thread::sleep(Duration::from_millis(5));
            }
            governor.cancel();
        })
    };
    let _git = crate::git::override_test_git_program(shim);
    let second = output.path().join("second");
    let result = crate::governor::with_run_scope(Arc::clone(&governor), async {
        crate::governor::blocking_stage(|| {
            generate_fixture_pack_with_ledger(
                repo.path(),
                &second,
                &target_sha,
                &base_sha,
                governor.as_ref(),
                &ledger,
            )
        })
    })
    .await;
    canceller.join().unwrap();

    let error = result.expect_err("cancelled cleanup must abort before publication");
    assert!(crate::governor::is_cancellation(&error), "{error:#}");
    assert_eq!(
        fs::read_link(output.path().join("latest")).unwrap(),
        PathBuf::from("first")
    );
    assert_cancelled_pack_is_not_published(&second, ArtifactGenerationSeam::SharedSnapshotCleanup);
    let incomplete: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(second.join("00_summary/INCOMPLETE.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        incomplete["stage"],
        ArtifactGenerationSeam::SharedSnapshotCleanup.label()
    );

    let recorded = fs::read_to_string(&pids).unwrap();
    for (position, pid) in recorded
        .split_whitespace()
        .map(|pid| pid.parse::<i32>().unwrap())
        .enumerate()
    {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            // SAFETY: signal 0 only probes PIDs created and recorded by this test.
            if unsafe { libc::kill(pid, 0) } == -1
                && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
            {
                break;
            }
            if std::time::Instant::now() >= deadline {
                let state = std::process::Command::new("ps")
                    .args([
                        "-o",
                        "pid=,ppid=,pgid=,stat=,command=",
                        "-p",
                        &pid.to_string(),
                    ])
                    .output()
                    .map(|output| String::from_utf8_lossy(&output.stdout).into_owned())
                    .unwrap_or_else(|error| format!("ps failed: {error}"));
                panic!(
                    "snapshot cleanup process {position} (pid {pid}) survived cancellation; test pid {}; state: {state}",
                    std::process::id()
                );
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

#[test]
fn artifact_generation_registry_is_exact_and_success_path_reaches_every_seam() {
    let publication_home = tempfile::tempdir().expect("publication home");
    let _publication_home =
        crate::config::override_test_prview_home(publication_home.path().to_path_buf());
    let expected = ArtifactGenerationSeam::ALL;
    let last = expected.len() - 1;
    let mut duplicate = expected;
    duplicate[last] = duplicate[last - 1];
    let mut reordered = expected;
    reordered.swap(8, 9);
    assert_ne!(
        &expected[..last],
        expected.as_slice(),
        "missing seam accepted"
    );
    assert_ne!(duplicate, expected, "duplicate seam accepted");
    assert_ne!(reordered, expected, "reordered seams accepted");

    let (repo, base_sha, target_sha) = init_advanced_base_fixture();
    let output = tempfile::tempdir().expect("output tempdir");
    let output_dir = output.path().join("pack");
    let governor = crate::governor::ResourceGovernor::new();
    let probe = generation_seam_test_hook::ProbeGuard::install(None);
    let generated =
        generate_fixture_pack(repo.path(), &output_dir, &target_sha, &base_sha, &governor)
            .expect("positive-control artifact generation");
    let observed = probe.observed();
    drop(probe);

    assert_eq!(generated, output_dir);
    assert_eq!(
        observed, expected,
        "production callsites drifted from registry"
    );
    assert!(!output_dir.join("00_summary/INCOMPLETE.json").exists());
    for relative in CANCELLED_GENERATION_SUCCESS_SURFACES {
        assert!(
            output_dir.join(relative).exists(),
            "positive control did not publish {relative}"
        );
    }
    #[cfg(unix)]
    if let Some(parent) = output_dir.parent() {
        let latest = parent.join("latest");
        assert!(
            latest.exists(),
            "positive control did not publish the latest symlink"
        );
        assert_eq!(
            fs::read_link(&latest).expect("latest symlink"),
            output_dir.file_name().expect("pack basename")
        );
    }
}

#[test]
fn cancellation_after_durable_publication_commit_does_not_relabel_the_run() {
    let publication_home = tempfile::tempdir().expect("publication home");
    let _publication_home =
        crate::config::override_test_prview_home(publication_home.path().to_path_buf());
    let (repo, base_sha, target_sha) = init_advanced_base_fixture();
    let output = tempfile::tempdir().expect("output tempdir");
    let output_dir = output.path().join("pack");
    let governor = crate::governor::ResourceGovernor::new();
    let _commit = publication_commit_test_hook::CommitGuard::install();

    let generated =
        generate_fixture_pack(repo.path(), &output_dir, &target_sha, &base_sha, &governor)
            .expect("a signal after the durable commit cannot cancel the completed run");

    assert_eq!(generated, output_dir);
    assert!(
        governor.is_cancelled(),
        "the probe must deliver the late cancel"
    );
    assert!(!output_dir.join("00_summary/INCOMPLETE.json").exists());
    for relative in CANCELLED_GENERATION_SUCCESS_SURFACES {
        assert!(
            output_dir.join(relative).exists(),
            "completed publication lost {relative} after its commit point"
        );
    }
    #[cfg(unix)]
    assert_eq!(
        fs::read_link(output.path().join("latest")).unwrap(),
        PathBuf::from("pack")
    );
    assert!(
        crate::storage::RunIndex::load()
            .entries()
            .iter()
            .any(|entry| entry.path == output_dir),
        "durably committed pack must remain indexed"
    );
}

#[test]
fn junk_files_excluded_from_zip_and_manifest() {
    use std::fs::File;

    let tmp = tempfile::tempdir().expect("tempdir");
    let out = tmp.path();
    let summary = out.join("00_summary");
    fs::create_dir_all(&summary).expect("00_summary");

    // A real artifact that MUST survive both surfaces.
    fs::write(summary.join("RUN.json"), b"{\"ok\":true}").expect("RUN.json");
    // OS junk at the root AND nested — nested placement is what proves the
    // WalkDir traversal in both call sites filters below the top level.
    fs::write(out.join(".DS_Store"), b"junk").expect("root .DS_Store");
    fs::write(summary.join(".DS_Store"), b"junk").expect("nested .DS_Store");
    fs::write(out.join("Thumbs.db"), b"junk").expect("Thumbs.db");
    // Mutable MCP control files are useful beside the live pack but are not
    // immutable payload: stdout can still grow after MANIFEST generation.
    for control in ["RUNNING.json", "run.log", "run.stderr.log"] {
        fs::write(out.join(control), b"mutable control").expect("MCP control file");
    }

    generate_manifest(out).expect("generate_manifest");
    // SANITY.json is written after the manifest in production and must ride
    // along in the shipped archive, so create_zip requires it to be present.
    fs::write(summary.join("SANITY.json"), b"{\"valid\":true}").expect("SANITY.json");
    create_zip(out, false).expect("create_zip");

    let is_junk = |name: &str| {
        matches!(
            Path::new(name).file_name().and_then(|n| n.to_str()),
            Some(".DS_Store") | Some("Thumbs.db")
        )
    };

    // MANIFEST.json must list RUN.json and no junk.
    let manifest: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(summary.join("MANIFEST.json")).expect("read manifest"),
    )
    .expect("parse manifest");
    let manifest_paths: Vec<&str> = manifest["files"]
        .as_array()
        .expect("files array")
        .iter()
        .filter_map(|f| f["path"].as_str())
        .collect();
    assert!(
        manifest_paths.iter().any(|p| p.ends_with("RUN.json")),
        "MANIFEST.json must contain the real artifact, got {manifest_paths:?}"
    );
    assert!(
        !manifest_paths.iter().any(|p| is_junk(p)),
        "MANIFEST.json must not list OS junk, got {manifest_paths:?}"
    );
    assert!(
        !manifest_paths
            .iter()
            .any(|path| MCP_CONTROL_FILES.contains(path)),
        "MANIFEST.json must not hash mutable MCP control files: {manifest_paths:?}"
    );

    // The shipped ZIP must contain RUN.json and no junk.
    let mut zip = zip::ZipArchive::new(File::open(out.join("artifacts.zip")).expect("open zip"))
        .expect("read zip");
    let zip_names: Vec<String> = (0..zip.len())
        .map(|i| zip.by_index(i).expect("zip entry").name().to_string())
        .collect();
    assert!(
        zip_names.iter().any(|n| n.ends_with("RUN.json")),
        "artifacts.zip must contain the real artifact, got {zip_names:?}"
    );
    assert!(
        !zip_names.iter().any(|n| is_junk(n)),
        "artifacts.zip must not ship OS junk, got {zip_names:?}"
    );
    assert!(
        !zip_names
            .iter()
            .any(|name| MCP_CONTROL_FILES.contains(&name.as_str())),
        "artifacts.zip must not ship mutable MCP control files: {zip_names:?}"
    );
    // The shipped pack must be self-validating: RUN.json (source of truth),
    // MANIFEST.json (integrity) and SANITY.json (verdict) all ride along.
    for required in [
        "00_summary/RUN.json",
        "00_summary/MANIFEST.json",
        "00_summary/SANITY.json",
    ] {
        assert!(
            zip_names.iter().any(|n| n.replace('\\', "/") == required),
            "artifacts.zip must contain {required}, got {zip_names:?}"
        );
    }
}

#[test]
fn create_zip_rejects_pack_missing_metadata() {
    // A pack whose SANITY.json was never written must not be shipped: the
    // archive would fail a consumer's own required-files check. create_zip
    // must catch the incomplete pack instead of emitting a silent success.
    let tmp = tempfile::tempdir().expect("tempdir");
    let out = tmp.path();
    let summary = out.join("00_summary");
    fs::create_dir_all(&summary).expect("00_summary");
    fs::write(summary.join("RUN.json"), b"{\"ok\":true}").expect("RUN.json");
    generate_manifest(out).expect("generate_manifest");
    // Deliberately omit SANITY.json.

    let err = create_zip(out, false).expect_err("create_zip must reject an incomplete pack");
    assert!(
        err.to_string().contains("SANITY.json"),
        "error must name the missing metadata, got: {err}"
    );
}
use crate::artifacts::signal::{BreakingKind, BreakingRisk};
use crate::checks::{CheckProvenance, CheckStatus};
use crate::cli::ExecutionMode;
use crate::config::{
    test_config_builder, test_generic_profile, test_js_profile, test_rust_profile,
};
use crate::git::{CommitInfo, DiffStats, FileChange, FileStatus, Repository, ResolvedRef, git_cmd};
use crate::policy::{PolicyConfig, PolicyMode, PolicySeverity};
use std::time::Duration;

#[test]
fn api_delta_no_diff_only_runtime() {
    let production = include_str!("mod.rs");
    assert!(production.contains("compare_rust_api_revisions_isolated("));
    assert!(!production.contains("compare_rust_api_revisions("));
    for phase in ["rust-api.fast-preset-unknown", "rust-api.isolated-worker"] {
        assert!(
            production.contains(phase),
            "the Rust API stage and RUN timing must name {phase}"
        );
    }
    assert!(production.contains("analyze_js_ts_public_api_diff(&patch_texts)"));
    assert!(production.contains("analyze_js_ts_breaking_changes(&patch_texts)"));
    assert!(production.contains("analyze_rust_env_requirements(&patch_texts)"));
    assert!(
        !production.contains("generate_public_api_diff(&quality_dir, &patch_texts)"),
        "Rust production must never return to the diff-only PUBLIC_API backend"
    );
    assert!(
        !production.contains("analyze_all_breaking_changes(&patch_texts)"),
        "Rust production must never return to the diff-only BREAKING backend"
    );
}

#[test]
fn rust_api_worker_activation_precedes_public_cli_parsing() {
    let main = include_str!("../main.rs");
    let worker = main
        .find("PRVIEW_INTERNAL_RUST_API_WORKER")
        .expect("private worker activation");
    let cli_parse = main.find("Cli::parse()").expect("public CLI parser");
    assert!(worker < cli_parse);
    assert!(main.contains("run_private_rust_api_worker()"));
}

#[test]
fn js_ts_legacy_breaking_path_never_observes_rust_patch_lines() {
    let patch = "diff --git a/src/lib.rs b/src/lib.rs\n--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1 +0,0 @@\n-pub fn rust_only() {}\ndiff --git a/src/api.ts b/src/api.ts\n--- a/src/api.ts\n+++ b/src/api.ts\n@@ -1 +0,0 @@\n-export function js_only() {}\n";
    let findings = signal::analyze_js_ts_breaking_changes(&[patch.to_owned()]);
    assert_eq!(findings.len(), 1);
    assert_eq!(findings[0].file, "src/api.ts");
    assert!(findings[0].line.contains("js_only"));
    assert!(
        findings
            .iter()
            .all(|finding| !finding.line.contains("rust_only"))
    );
}

#[test]
fn marker_like_hunk_content_survives_both_legacy_api_adapters() {
    let patch = "diff --git a/src/api.ts b/src/api.ts\n--- a/src/api.ts\n+++ b/src/api.ts\n@@ -1,2 +1,2 @@\n--- content collision\n-export function removed_after_collision() {}\n+++ content collision\n+export function added_after_collision() {}\n";

    let public = signal::analyze_js_ts_public_api_diff(&[patch.to_owned()]);
    assert!(
        public
            .removed
            .iter()
            .any(|finding| finding.signature.contains("removed_after_collision"))
    );
    assert!(
        public
            .added
            .iter()
            .any(|finding| finding.signature.contains("added_after_collision"))
    );

    let breaking = signal::analyze_js_ts_breaking_changes(&[patch.to_owned()]);
    assert!(
        breaking
            .iter()
            .any(|finding| finding.line.contains("removed_after_collision"))
    );
}

#[test]
fn malformed_marker_identity_never_reclassifies_rust_for_either_legacy_adapter() {
    for patch in [
        "diff --git a/src/lib.rs b/src/api.ts\n--- a/src/fake.ts\n+++ b/src/api.ts\n@@ -1 +1 @@\n-pub fn rust_secret() {}\n+export function js_added() {}\n",
        "diff --git a/src/api.ts b/src/lib.rs\n--- a/src/api.ts\n+++ b/src/fake.ts\n@@ -1 +1 @@\n-export function js_secret() {}\n+pub fn rust_added() {}\n",
        "diff --git \"a/src/lib.rs\" \"b/src/quoted\\040api.ts\"\n--- \"a/src/fake\\040api.ts\"\n+++ \"b/src/quoted\\040api.ts\"\n@@ -1 +1 @@\n-pub fn quoted_rust_secret() {}\n+export function quoted_js_added() {}\n",
        "diff --git a/src/lib.rs b/src/api.ts\nsimilarity index 61%\nrename from src/lib.rs\nrename to src/api.ts\n@@ -1 +1 @@\n-pub fn markerless_rust_secret() {}\n+export function markerless_js_added() {}\n",
    ] {
        let filtered = signal::js_ts_patch_sections(patch);
        assert_eq!(filtered, "", "{patch}");

        let public = signal::analyze_js_ts_public_api_diff(&[patch.to_owned()]);
        assert!(public.added.is_empty(), "{patch}");
        assert!(public.removed.is_empty(), "{patch}");

        let breaking = signal::analyze_js_ts_breaking_changes(&[patch.to_owned()]);
        assert!(breaking.is_empty(), "{patch}");
    }
}

#[test]
fn cross_language_rust_to_ts_keeps_only_the_js_added_side() {
    let patch = "diff --git a/src/lib.rs b/src/api.ts\nsimilarity index 61%\nrename from src/lib.rs\nrename to src/api.ts\n--- a/src/lib.rs\n+++ b/src/api.ts\n@@ -1 +1 @@\n-pub fn rust_removed() {}\n+export function js_added() {}\n";
    let filtered = signal::js_ts_patch_sections(patch);
    assert!(filtered.contains("js_added"));
    assert!(!filtered.contains("rust_removed"));
    assert!(!filtered.contains("src/lib.rs"));

    let public = signal::analyze_js_ts_public_api_diff(&[patch.to_owned()]);
    assert!(
        public
            .added
            .iter()
            .any(|finding| finding.signature.contains("js_added"))
    );
    assert!(public.removed.is_empty());

    let breaking = signal::analyze_js_ts_breaking_changes(&[patch.to_owned()]);
    assert!(breaking.iter().all(|finding| {
        !finding.line.contains("rust_removed") && !finding.file.ends_with(".rs")
    }));
}

#[test]
fn cross_language_ts_to_rust_keeps_only_the_js_removed_side() {
    let patch = "diff --git a/src/api.ts b/src/lib.rs\nsimilarity index 61%\nrename from src/api.ts\nrename to src/lib.rs\n--- a/src/api.ts\n+++ b/src/lib.rs\n@@ -1 +1 @@\n-export function js_removed() {}\n+pub fn rust_added() {}\n";
    let filtered = signal::js_ts_patch_sections(patch);
    assert!(filtered.contains("js_removed"));
    assert!(!filtered.contains("rust_added"));
    assert!(!filtered.contains("src/lib.rs"));

    let public = signal::analyze_js_ts_public_api_diff(&[patch.to_owned()]);
    assert!(
        public
            .removed
            .iter()
            .any(|finding| finding.signature.contains("js_removed"))
    );
    assert!(public.added.is_empty());

    let breaking = signal::analyze_js_ts_breaking_changes(&[patch.to_owned()]);
    assert!(breaking.iter().any(|finding| {
        finding.file == "src/api.ts"
            && finding.line.contains("js_removed")
            && matches!(finding.kind, BreakingKind::RemovedSymbol { .. })
    }));
    assert!(
        breaking.iter().all(|finding| {
            !finding.line.contains("rust_added") && !finding.file.ends_with(".rs")
        })
    );
}

#[test]
fn quoted_and_unquoted_space_js_paths_survive_both_legacy_adapters() {
    for patch in [
        "diff --git \"a/src/quoted\\040api.ts\" \"b/src/quoted\\040api.ts\"\n--- \"a/src/quoted\\040api.ts\"\n+++ \"b/src/quoted\\040api.ts\"\n@@ -1 +1 @@\n-export function api(value: number): number { return value; }\n+export function api(value: string): string { return value; }\n",
        "diff --git a/src/plain old.ts b/src/plain new.ts\n--- a/src/plain old.ts\n+++ b/src/plain new.ts\n@@ -1 +1 @@\n-export function api(value: number): number { return value; }\n+export function api(value: string): string { return value; }\n",
    ] {
        let filtered = signal::js_ts_patch_sections(patch);
        assert!(filtered.contains("export function api"));

        let public = signal::analyze_js_ts_public_api_diff(&[patch.to_owned()]);
        assert!(
            public
                .removed
                .iter()
                .any(|finding| finding.signature.contains("value: number")),
            "{filtered}"
        );
        assert!(
            public
                .added
                .iter()
                .any(|finding| finding.signature.contains("value: string")),
            "{filtered}"
        );

        let breaking = signal::analyze_js_ts_breaking_changes(&[patch.to_owned()]);
        assert!(breaking.iter().any(|finding| {
            finding.file.ends_with(".ts")
                && finding.line.contains("value: number")
                && matches!(finding.kind, BreakingKind::RemovedSymbol { .. })
        }));
    }
}

#[test]
fn js_add_delete_sections_keep_the_correct_legacy_side() {
    let added = "diff --git a/src/new.ts b/src/new.ts\nnew file mode 100644\n--- /dev/null\n+++ b/src/new.ts\n@@ -0,0 +1 @@\n+export function added_js() {}\n";
    let deleted = "diff --git a/src/old.ts b/src/old.ts\ndeleted file mode 100644\n--- a/src/old.ts\n+++ /dev/null\n@@ -1 +0,0 @@\n-export function removed_js() {}\n";

    let public = signal::analyze_js_ts_public_api_diff(&[added.to_owned(), deleted.to_owned()]);
    assert!(
        public
            .added
            .iter()
            .any(|finding| finding.signature.contains("added_js"))
    );
    assert!(
        public
            .removed
            .iter()
            .any(|finding| finding.signature.contains("removed_js"))
    );

    let breaking = signal::analyze_js_ts_breaking_changes(&[added.to_owned(), deleted.to_owned()]);
    assert!(breaking.iter().any(|finding| {
        finding.file == "src/old.ts"
            && finding.line.contains("removed_js")
            && matches!(finding.kind, BreakingKind::RemovedSymbol { .. })
    }));
    assert!(
        breaking
            .iter()
            .all(|finding| !finding.line.contains("added_js"))
    );
}

#[test]
fn rust_env_signal_is_preserved_without_emitting_rust_api_facts() {
    let patch = "diff --git a/src/lib.rs b/src/lib.rs\n--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1 +1,2 @@\n-pub fn old() {}\n+pub fn new() {}\n+// REQUIRED_ENV MY_DATABASE_URL=postgres://localhost\n";
    let findings = signal::analyze_rust_env_requirements(&[patch.to_owned()]);
    assert_eq!(findings.len(), 1);
    assert!(matches!(
        &findings[0].kind,
        BreakingKind::NewEnvRequirement { variable } if variable == "MY_DATABASE_URL"
    ));
    assert!(findings.iter().all(|finding| !matches!(
        finding.kind,
        BreakingKind::RemovedSymbol { .. }
            | BreakingKind::ChangedSignature { .. }
            | BreakingKind::RelocatedSymbol { .. }
    )));
}

macro_rules! generate_merge_gate_test {
    ($dir:expr, $config:expr, $checks:expr, $heuristics:expr, $inline:expr, $breaking:expr, $coverage:expr, $skipped_checks:expr, $resolved_target:expr, $resolved_bases:expr $(,)?) => {
        generate_merge_gate(MergeGateInput {
            dir: $dir,
            config: $config,
            // Nothing recorded: these packs replay no stored result, so no gate
            // row can carry a stale-cache caveat.
            ledger: &crate::ledger::TaskLedger::new(),
            checks: $checks,
            heuristics: $heuristics,
            inline: $inline,
            breaking: $breaking,
            rust_api_delta: None,
            coverage: $coverage,
            diffs: &[],
            skipped_checks: $skipped_checks,
            resolved_target: $resolved_target,
            resolved_bases: $resolved_bases,
            clean_comparison: CleanComparison::for_test(true, true),
        })
    };
}

macro_rules! generate_run_json_test {
    ($dir:expr, $artifacts_root:expr, $config:expr, $checks:expr, $heuristics:expr, $resolved_target:expr, $resolved_bases:expr, ($run_started_at:expr, $total_duration_secs:expr), $stage_timings:expr, $context_artifacts:expr, $context_command_timings:expr, $regression:expr $(,)?) => {
        generate_run_json_test!(
            $dir,
            $artifacts_root,
            $config,
            $checks,
            $heuristics,
            $resolved_target,
            $resolved_bases,
            ($run_started_at, $total_duration_secs),
            $stage_timings,
            $context_artifacts,
            $context_command_timings,
            $regression,
            &crate::ledger::TaskLedger::new(),
        )
    };
    ($dir:expr, $artifacts_root:expr, $config:expr, $checks:expr, $heuristics:expr, $resolved_target:expr, $resolved_bases:expr, ($run_started_at:expr, $total_duration_secs:expr), $stage_timings:expr, $context_artifacts:expr, $context_command_timings:expr, $regression:expr, $ledger:expr $(,)?) => {
        generate_run_json(RunJsonInput {
            ledger: $ledger,
            dir: $dir,
            artifacts_root: $artifacts_root,
            config: $config,
            checks: $checks,
            skipped_checks: &[],
            heuristics: $heuristics,
            resolved_target: $resolved_target,
            resolved_bases: $resolved_bases,
            run_started_at: $run_started_at,
            total_duration_secs: $total_duration_secs,
            stage_timings: $stage_timings,
            context_artifacts: $context_artifacts,
            context_command_timings: $context_command_timings,
            regression: $regression,
        })
    };
}

fn create_test_config(policy: PolicyConfig) -> Config {
    test_config_builder()
        .target(Some("feature/security-gate"))
        .bases(&["main"])
        .profile(test_rust_profile(true))
        .execution_mode(ExecutionMode::Standard)
        .run_tests(true)
        .run_lint(true)
        .do_fetch(false)
        .use_cache(false)
        .create_zip(false)
        .policy(policy)
        .build()
}

fn run_git_fixture(repo: &Path, args: &[&str]) {
    let status = git_cmd()
        .args(args)
        .current_dir(repo)
        .status()
        .expect("git command");
    assert!(status.success(), "git {args:?} failed with {status}");
}

fn write_commit_fixture(repo: &Path, name: &str, body: &str) -> String {
    fs::write(repo.join(name), body).expect("write fixture");
    run_git_fixture(repo, &["add", name]);
    run_git_fixture(
        repo,
        &[
            "-c",
            "user.name=prview test",
            "-c",
            "user.email=prview@example.test",
            "commit",
            "-m",
            name,
        ],
    );
    let output = git_cmd()
        .args(["rev-parse", "HEAD"])
        .current_dir(repo)
        .output()
        .expect("rev-parse");
    assert!(output.status.success());
    String::from_utf8(output.stdout)
        .expect("utf8 rev-parse")
        .trim()
        .to_string()
}

fn init_advanced_base_fixture() -> (tempfile::TempDir, String, String) {
    let tmp = tempfile::tempdir().expect("tempdir");
    run_git_fixture(tmp.path(), &["init", "-q", "-b", "main"]);
    let merge_base = write_commit_fixture(tmp.path(), "own.rs", "pub fn own() -> u8 { 1 }\n");
    run_git_fixture(tmp.path(), &["checkout", "-q", "-b", "feature"]);
    let target = write_commit_fixture(tmp.path(), "own.rs", "pub fn own() -> u8 { 2 }\n");
    run_git_fixture(tmp.path(), &["checkout", "-q", "main"]);
    let _advance_one = write_commit_fixture(
        tmp.path(),
        "unrelated.rs",
        "pub fn unrelated_one() -> u8 { 1 }\n",
    );
    let _advance_two = write_commit_fixture(
        tmp.path(),
        "unrelated.rs",
        "pub fn unrelated_one() -> u8 { 1 }\npub fn unrelated_two() -> u8 { 2 }\n",
    );
    run_git_fixture(tmp.path(), &["checkout", "-q", "feature"]);
    (tmp, merge_base, target)
}

#[tokio::test]
async fn artifact_pipeline_diffs_from_merge_base_when_base_advanced() {
    let (repo_tmp, merge_base, _target) = init_advanced_base_fixture();
    let output_tmp = tempfile::tempdir().expect("output tempdir");
    let output_dir = output_tmp.path().join("pack");

    let mut config = test_config_builder()
        .repo_root(repo_tmp.path())
        .target(Some("feature"))
        .bases(&["main"])
        .profile(test_generic_profile())
        .execution_mode(ExecutionMode::Standard)
        .run_tests(false)
        .run_lint(false)
        .do_fetch(false)
        .use_cache(false)
        .create_zip(false)
        .build();
    config.run_bundle = false;
    config.run_security = false;
    config.run_heuristics = false;
    config.create_dashboard = false;
    config.quiet = true;
    config.output_dir = Some(output_dir.clone());

    let app = crate::App::from_config(config).expect("app");
    let report = app.run().await.expect("run prview");
    assert_eq!(report.artifacts_dir, output_dir);

    let full_patch =
        fs::read_to_string(output_dir.join("10_diff/full.patch")).expect("read full.patch");
    assert!(
        full_patch.contains("own.rs"),
        "target-owned change must be present:\n{full_patch}"
    );
    assert!(
        !full_patch.contains("unrelated.rs"),
        "advanced base-only file must not leak into three-dot diff:\n{full_patch}"
    );

    let raw_report = fs::read_to_string(output_dir.join("report.json")).expect("read report.json");
    let report_json: serde_json::Value =
        serde_json::from_str(&raw_report).expect("parse report.json");
    assert_eq!(
        report_json["meta"]["range"]["merge_base"].as_str(),
        Some(merge_base.as_str())
    );
    assert_eq!(report_json["meta"]["range"]["base"].as_str(), Some("main"));
    assert_eq!(
        report_json["meta"]["range"]["head"].as_str(),
        Some("feature")
    );
    assert_eq!(
        report_json["diff"]["stats"]["files_changed"].as_u64(),
        Some(1)
    );

    let repo = Repository::open(repo_tmp.path()).expect("open repo");
    let resolved_target = repo
        .resolve_target(&app.config)
        .expect("resolve target after run");
    let resolved_bases = repo
        .resolve_bases(&app.config)
        .expect("resolve bases after run");
    let diff_bases = repo.resolve_diff_bases(&resolved_target, &resolved_bases, true);
    assert_eq!(
        diff_bases.first().map(|base| base.commit_id.as_str()),
        Some(merge_base.as_str())
    );
}

fn sample_cargo_audit_output() -> String {
    r#"{
  "vulnerabilities": {
    "found": true,
    "count": 2,
    "list": [
      {
        "advisory": {
          "id": "RUSTSEC-2024-0001",
          "title": "Unsound transmute in example crate",
          "url": "https://rustsec.org/advisories/RUSTSEC-2024-0001",
          "cvss": {
            "score": 9.8
          }
        },
        "package": {
          "name": "example-crate",
          "version": "0.3.1"
        },
        "versions": {
          "patched": [">=0.3.2"]
        }
      },
      {
        "advisory": {
          "id": "RUSTSEC-2024-0002",
          "title": "Denial of service in helper crate",
          "url": "https://rustsec.org/advisories/RUSTSEC-2024-0002",
          "cvss": {
            "score": 5.4
          }
        },
        "package": {
          "name": "helper-crate",
          "version": "1.4.0"
        },
        "versions": {
          "patched": [">=1.4.1"]
        }
      }
    ]
  }
}
warning: advisory database is 3 days old
"#
    .to_string()
}

fn sample_cargo_tree_output() -> &'static str {
    r#"rmcp-memex v0.1.0 (/workspace/rmcp-memex)
├── example-crate v0.3.1
├── tonic v0.12.3
│   └── example-crate v0.3.1
└── helper-crate v1.4.0
"#
}

#[test]
fn infer_batch_theme_prefers_search_keywords() {
    let batch = vec![
        CommitInfo {
            id: "1".into(),
            short_id: "1111111".into(),
            author: "Test Author".into(),
            email: "m@example.com".into(),
            date: "2026-03-08".into(),
            message: "feat(search): add hybrid query routing".into(),
        },
        CommitInfo {
            id: "2".into(),
            short_id: "2222222".into(),
            author: "Test Author".into(),
            email: "m@example.com".into(),
            date: "2026-03-08".into(),
            message: "refactor(index): improve bm25 ranking".into(),
        },
    ];

    assert_eq!(infer_batch_theme(&batch), "search infrastructure");
}

#[test]
fn cargo_tree_index_extracts_dependency_paths() {
    let index = CargoTreeIndex::from_text(sample_cargo_tree_output());
    let finding = CargoAuditFinding {
        advisory_id: "RUSTSEC-2024-0001".into(),
        package_name: "example-crate".into(),
        package_version: "0.3.1".into(),
        title: "Unsound issue".into(),
        severity: "critical".into(),
        sarif_level: "error",
        patched_versions: Some(">=1.10.2".into()),
        help_url: None,
    };

    let paths = index.paths_for(&finding, 4);
    assert_eq!(paths.len(), 2);
    assert!(paths.iter().any(
            |path| path == "rmcp-memex v0.1.0 (/workspace/rmcp-memex) -> example-crate v0.3.1"
        ));
    assert!(paths.iter().any(|path| {
        path == "rmcp-memex v0.1.0 (/workspace/rmcp-memex) -> tonic v0.12.3 -> example-crate v0.3.1"
    }));
}

#[test]
fn merge_gate_blocks_failed_cargo_audit_in_warn_mode_when_severity_is_block() {
    let mut policy = PolicyConfig {
        mode: PolicyMode::Warn,
        ..PolicyConfig::default()
    };
    policy
        .checks
        .insert("cargo_audit".to_string(), PolicySeverity::Block);

    let config = create_test_config(policy);
    let checks = vec![CheckResult {
        name: "Cargo audit".to_string(),
        status: CheckStatus::Failed,
        duration: Duration::from_secs(1),
        output: "RUSTSEC-2026-0001".to_string(),
        cached: false,
        provenance: None,
    }];
    let inline = InlineFindingsSummary {
        status: "passed".to_string(),
        findings_count: 0,
        dashboard_findings: vec![],
    };
    let resolved_target = ResolvedRef {
        name: "feature/runtime-target".to_string(),
        commit_id: "abc1234abc1234abc1234abc1234abc1234ab".to_string(),
        is_remote: false,
    };
    let resolved_bases = vec![ResolvedRef {
        name: "origin/main".to_string(),
        commit_id: "def5678def5678def5678def5678def5678de".to_string(),
        is_remote: true,
    }];

    let tmp = tempfile::tempdir().expect("tempdir");
    generate_merge_gate_test!(
        tmp.path(),
        &config,
        &checks,
        None,
        &inline,
        &[],
        &CoverageDelta {
            total_source: 0,
            covered_count: 0,
            pct: None,
            uncovered: vec![],
            covered: vec![],
            non_code_count: 0,
            ghost_tests: vec![],
        },
        &[],
        &resolved_target,
        &resolved_bases,
    )
    .expect("merge gate");

    let raw = std::fs::read_to_string(tmp.path().join("MERGE_GATE.json")).expect("read gate");
    let gate: serde_json::Value = serde_json::from_str(&raw).expect("parse gate");

    let cargo_check = gate["checks"]
        .as_array()
        .and_then(|checks| checks.iter().find(|c| c["id"] == "cargo_audit"))
        .expect("cargo_audit check");

    assert_eq!(cargo_check["class"].as_str(), Some("FAIL"));
    assert_eq!(cargo_check["severity"].as_str(), Some("block"));
    assert_eq!(cargo_check["blocking"].as_bool(), Some(true));
    assert_eq!(gate["target"].as_str(), Some("feature/runtime-target"));
    assert_eq!(gate["bases"], serde_json::json!(["origin/main"]));
    assert_eq!(gate["profile"].as_str(), Some("Rust"));
    assert_eq!(gate["decision"]["allow_merge"].as_bool(), Some(false));

    let blocking_issues = gate["decision"]["blocking_issues"]
        .as_array()
        .expect("blocking issues array");
    assert!(
        blocking_issues
            .iter()
            .any(|item| item.as_str() == Some("Cargo audit (Failed)"))
    );
}

#[test]
fn merge_gate_executed_cargo_check_carries_real_evidence_and_log() {
    // "cargo check" is the case where the policy engine id (cargo_check) and the
    // artifact-writer id (cargo) diverge. An executed check must still resolve
    // to its on-disk result.json and log — never fall through to the "skipped —
    // no artifact generated" placeholder with a null log.
    let config = create_test_config(PolicyConfig::default());
    let checks = vec![CheckResult {
        name: "cargo check".to_string(),
        status: CheckStatus::Passed,
        duration: Duration::from_millis(1500),
        output: "ok".to_string(),
        cached: false,
        provenance: None,
    }];
    let inline = InlineFindingsSummary {
        status: "passed".to_string(),
        findings_count: 0,
        dashboard_findings: vec![],
    };
    let resolved_target = ResolvedRef {
        name: "feature/runtime-target".to_string(),
        commit_id: "abc1234abc1234abc1234abc1234abc1234ab".to_string(),
        is_remote: false,
    };
    let resolved_bases = vec![ResolvedRef {
        name: "origin/main".to_string(),
        commit_id: "def5678def5678def5678def5678def5678de".to_string(),
        is_remote: true,
    }];

    let tmp = tempfile::tempdir().expect("tempdir");
    generate_merge_gate_test!(
        tmp.path(),
        &config,
        &checks,
        None,
        &inline,
        &[],
        &CoverageDelta {
            total_source: 0,
            covered_count: 0,
            pct: None,
            uncovered: vec![],
            covered: vec![],
            non_code_count: 0,
            ghost_tests: vec![],
        },
        &[],
        &resolved_target,
        &resolved_bases,
    )
    .expect("merge gate");

    let raw = std::fs::read_to_string(tmp.path().join("MERGE_GATE.json")).expect("read gate");
    let gate: serde_json::Value = serde_json::from_str(&raw).expect("parse gate");

    let entry = gate["checks"]
        .as_array()
        .and_then(|checks| checks.iter().find(|c| c["name"] == "cargo check"))
        .expect("cargo check gate entry");

    assert_eq!(entry["execution_state"].as_str(), Some("executed"));
    assert_eq!(
        entry["evidence"].as_str(),
        Some("20_quality/cargo.result.json"),
        "executed check must reference its real result artifact, not a placeholder"
    );
    assert_eq!(
        entry["log"].as_str(),
        Some("20_quality/cargo.log"),
        "executed check must reference its real log, not null"
    );
    assert!(
        entry["duration_secs"].as_f64().unwrap_or(0.0) > 0.0,
        "executed check must carry the measured duration"
    );
}

#[test]
fn run_json_reports_explicit_update_mode_without_guessing_from_disabled_checks() {
    let mut config = create_test_config(PolicyConfig::default());
    config.execution_mode = ExecutionMode::Update;

    let resolved_target = ResolvedRef {
        name: "feature/runtime-target".to_string(),
        commit_id: "abc1234abc1234abc1234abc1234abc1234ab".to_string(),
        is_remote: false,
    };
    let resolved_bases = vec![ResolvedRef {
        name: "origin/main".to_string(),
        commit_id: "def5678def5678def5678def5678def5678de".to_string(),
        is_remote: true,
    }];

    let tmp = tempfile::tempdir().expect("tempdir");
    generate_run_json_test!(
        tmp.path(),
        tmp.path(),
        &config,
        &[],
        None,
        &resolved_target,
        &resolved_bases,
        ("2026-03-08T12:00:00Z", 1.5),
        &[],
        &[],
        &[],
        None,
    )
    .expect("run json");

    let raw = std::fs::read_to_string(tmp.path().join("RUN.json")).expect("read run json");
    let run: serde_json::Value = serde_json::from_str(&raw).expect("parse run json");

    assert_eq!(run["flags"]["mode"].as_str(), Some("update"));
    assert_eq!(run["flags"]["update"].as_bool(), Some(true));
    assert_eq!(run["flags"]["quick"].as_bool(), Some(false));
    assert_eq!(run["flags"]["deep"].as_bool(), Some(false));
}

#[test]
fn run_json_records_actual_output_dir_used_for_run() {
    let config = create_test_config(PolicyConfig::default());

    let resolved_target = ResolvedRef {
        name: "feature/runtime-target".to_string(),
        commit_id: "abc1234abc1234abc1234abc1234abc1234ab".to_string(),
        is_remote: false,
    };
    let resolved_bases = vec![ResolvedRef {
        name: "origin/main".to_string(),
        commit_id: "def5678def5678def5678def5678def5678de".to_string(),
        is_remote: true,
    }];

    let summary_dir = tempfile::tempdir().expect("summary tempdir");
    let actual_out_dir = summary_dir.path().join("actual-run-dir");

    generate_run_json_test!(
        summary_dir.path(),
        &actual_out_dir,
        &config,
        &[],
        None,
        &resolved_target,
        &resolved_bases,
        ("2026-03-08T12:00:00Z", 1.5),
        &[],
        &[],
        &[],
        None,
    )
    .expect("run json");

    let raw = std::fs::read_to_string(summary_dir.path().join("RUN.json")).expect("read run json");
    let run: serde_json::Value = serde_json::from_str(&raw).expect("parse run json");

    assert_eq!(
        run["artifacts_root"].as_str(),
        Some(actual_out_dir.to_string_lossy().as_ref())
    );
}

#[test]
fn run_json_uses_unused_symbols_key_for_heuristics_summary() {
    use crate::heuristics::{
        DeadParrot, HeuristicsResult, HeuristicsSummary, LoctreeAnalysis, TwinsAnalysis,
    };

    let config = create_test_config(PolicyConfig::default());
    let resolved_target = ResolvedRef {
        name: "feature/runtime-target".to_string(),
        commit_id: "abc1234abc1234abc1234abc1234abc1234ab".to_string(),
        is_remote: false,
    };
    let resolved_bases = vec![ResolvedRef {
        name: "origin/main".to_string(),
        commit_id: "def5678def5678def5678def5678def5678de".to_string(),
        is_remote: true,
    }];
    let heuristics = HeuristicsResult {
        loctree: Some(LoctreeAnalysis {
            available: true,
            twins: TwinsAnalysis {
                dead_parrots: vec![DeadParrot {
                    file: "src/lib.rs".to_string(),
                    symbol: "unused_helper".to_string(),
                    kind: "function".to_string(),
                    line: 42,
                }],
                exact_twins: vec![],
                total_symbols: 1,
            },
            ..Default::default()
        }),
        summary: HeuristicsSummary {
            dead_exports: 2,
            circular_imports: 1,
            dead_parrots: 1,
            exact_twins: 0,
            total_files: 10,
            total_loc: 100,
        },
        ..Default::default()
    };

    let summary_dir = tempfile::tempdir().expect("summary tempdir");
    generate_run_json_test!(
        summary_dir.path(),
        summary_dir.path(),
        &config,
        &[],
        Some(&heuristics),
        &resolved_target,
        &resolved_bases,
        ("2026-03-08T12:00:00Z", 1.5),
        &[],
        &[],
        &[],
        None,
    )
    .expect("run json");

    let raw = std::fs::read_to_string(summary_dir.path().join("RUN.json")).expect("read run json");
    let run: serde_json::Value = serde_json::from_str(&raw).expect("parse run json");
    let heuristics_json = &run["heuristics"];

    assert_eq!(heuristics_json["unused_symbols"].as_u64(), Some(1));
    assert!(heuristics_json.get("dead_parrots").is_none());
}

#[test]
fn run_json_marks_fast_remote_only_standard_flags_and_disabled_analysis() {
    let mut config = create_test_config(PolicyConfig::default());
    config.remote_only = true;
    config.run_tests = false;
    config.run_heuristics = false;

    let resolved_target = ResolvedRef {
        name: "feature/runtime-target".to_string(),
        commit_id: "abc1234abc1234abc1234abc1234abc1234ab".to_string(),
        is_remote: true,
    };
    let resolved_bases = vec![ResolvedRef {
        name: "origin/main".to_string(),
        commit_id: "def5678def5678def5678def5678def5678de".to_string(),
        is_remote: true,
    }];

    let summary_dir = tempfile::tempdir().expect("summary tempdir");
    generate_run_json_test!(
        summary_dir.path(),
        summary_dir.path(),
        &config,
        &[],
        None,
        &resolved_target,
        &resolved_bases,
        ("2026-03-08T12:00:00Z", 1.5),
        &[],
        &[],
        &[],
        None,
    )
    .expect("run json");

    let raw = std::fs::read_to_string(summary_dir.path().join("RUN.json")).expect("read run json");
    let run: serde_json::Value = serde_json::from_str(&raw).expect("parse run json");

    assert_eq!(run["flags"]["remote_only"].as_bool(), Some(true));
    assert_eq!(
        run["flags"]["fast_remote_only_standard"].as_bool(),
        Some(true)
    );
    assert_eq!(run["analysis"]["mode"].as_str(), Some("disabled"));
}

#[test]
fn run_json_records_stage_timings() {
    let config = create_test_config(PolicyConfig::default());

    let resolved_target = ResolvedRef {
        name: "feature/runtime-target".to_string(),
        commit_id: "abc1234abc1234abc1234abc1234abc1234ab".to_string(),
        is_remote: false,
    };
    let resolved_bases = vec![ResolvedRef {
        name: "origin/main".to_string(),
        commit_id: "def5678def5678def5678def5678def5678de".to_string(),
        is_remote: true,
    }];

    let summary_dir = tempfile::tempdir().expect("summary tempdir");
    let stage_timings = vec![
        StageTiming {
            label: "00_summary".to_string(),
            duration_secs: 0.25,
        },
        StageTiming {
            label: "report.json + dashboard".to_string(),
            duration_secs: 1.75,
        },
    ];

    generate_run_json_test!(
        summary_dir.path(),
        summary_dir.path(),
        &config,
        &[],
        None,
        &resolved_target,
        &resolved_bases,
        ("2026-03-08T12:00:00Z", 1.5),
        &stage_timings,
        &[],
        &[],
        None,
    )
    .expect("run json");

    let raw = std::fs::read_to_string(summary_dir.path().join("RUN.json")).expect("read run json");
    let run: serde_json::Value = serde_json::from_str(&raw).expect("parse run json");

    let timings = run["timings"].as_array().expect("timings array");
    assert_eq!(timings.len(), 2);
    assert_eq!(timings[0]["label"].as_str(), Some("00_summary"));
    assert_eq!(timings[0]["duration_secs"].as_f64(), Some(0.25));
    assert_eq!(
        timings[1]["label"].as_str(),
        Some("report.json + dashboard")
    );
    assert_eq!(timings[1]["duration_secs"].as_f64(), Some(1.75));
    assert_eq!(run["resources"]["requested_budget"], "safe");
    assert_eq!(run["resources"]["effective_budget"], "safe");
    assert_eq!(run["resources"]["parent_permits"], 1);
    assert_eq!(run["resources"]["child_worker_limit"], 1);
    assert!(
        run["resources"]["schedule"]
            .as_str()
            .is_some_and(|schedule| schedule.starts_with("cheap orientation/checks"))
    );
}

#[test]
fn run_json_records_deferred_context_artifacts() {
    let mut config = create_test_config(PolicyConfig::default());
    config.remote_only = true;
    config.profile.has_tsconfig = true;

    let resolved_target = ResolvedRef {
        name: "feature/runtime-target".to_string(),
        commit_id: "abc1234abc1234abc1234abc1234abc1234ab".to_string(),
        is_remote: true,
    };
    let resolved_bases = vec![ResolvedRef {
        name: "origin/main".to_string(),
        commit_id: "def5678def5678def5678def5678def5678de".to_string(),
        is_remote: true,
    }];

    let summary_dir = tempfile::tempdir().expect("summary tempdir");
    let context_artifacts = vec![ContextArtifactDecision {
            key: "tsc_trace",
            path: "30_context/tsc-trace.log",
            generated: false,
            recommended: true,
            reason: "skipped by default in fast remote-only runs; generate when investigating because TypeScript check failed with module-resolution-style errors".to_string(),
        }];

    generate_run_json_test!(
        summary_dir.path(),
        summary_dir.path(),
        &config,
        &[],
        None,
        &resolved_target,
        &resolved_bases,
        ("2026-03-08T12:00:00Z", 1.5),
        &[],
        &context_artifacts,
        &[],
        None,
    )
    .expect("run json");

    let raw = std::fs::read_to_string(summary_dir.path().join("RUN.json")).expect("read run json");
    let run: serde_json::Value = serde_json::from_str(&raw).expect("parse run json");

    let context_artifacts = run["context_artifacts"]
        .as_array()
        .expect("context artifacts array");
    assert_eq!(context_artifacts.len(), 1);
    assert_eq!(context_artifacts[0]["key"].as_str(), Some("tsc_trace"));
    assert_eq!(context_artifacts[0]["generated"].as_bool(), Some(false));
    assert_eq!(context_artifacts[0]["recommended"].as_bool(), Some(true));
    assert!(
        context_artifacts[0]["reason"]
            .as_str()
            .expect("reason")
            .contains("TypeScript check failed")
    );
}

#[test]
fn run_json_records_context_command_timings() {
    let config = create_test_config(PolicyConfig::default());

    let resolved_target = ResolvedRef {
        name: "feature/runtime-target".to_string(),
        commit_id: "abc1234abc1234abc1234abc1234abc1234ab".to_string(),
        is_remote: false,
    };
    let resolved_bases = vec![ResolvedRef {
        name: "origin/main".to_string(),
        commit_id: "def5678def5678def5678def5678def5678de".to_string(),
        is_remote: true,
    }];

    let summary_dir = tempfile::tempdir().expect("summary tempdir");
    let context_commands = vec![
        ContextCommandTiming {
            label: "cargo tree".to_string(),
            artifact: Some("30_context/cargo-tree.txt".to_string()),
            status: "completed",
            started: true,
            duration_secs: 3.5,
            reason: None,
        },
        ContextCommandTiming {
            label: "tauri info".to_string(),
            artifact: Some("30_context/tauri-info.log".to_string()),
            status: "timed_out",
            started: true,
            duration_secs: 30.0,
            reason: Some("exceeded 30s context timeout".to_string()),
        },
    ];

    generate_run_json_test!(
        summary_dir.path(),
        summary_dir.path(),
        &config,
        &[],
        None,
        &resolved_target,
        &resolved_bases,
        ("2026-03-08T12:00:00Z", 1.5),
        &[],
        &[],
        &context_commands,
        None,
    )
    .expect("run json");

    let raw = std::fs::read_to_string(summary_dir.path().join("RUN.json")).expect("read run json");
    let run: serde_json::Value = serde_json::from_str(&raw).expect("parse run json");

    let context_commands = run["context_commands"]
        .as_array()
        .expect("context commands array");
    assert_eq!(context_commands.len(), 2);
    assert_eq!(context_commands[0]["label"].as_str(), Some("cargo tree"));
    assert_eq!(
        context_commands[0]["artifact"].as_str(),
        Some("30_context/cargo-tree.txt")
    );
    assert_eq!(context_commands[1]["status"].as_str(), Some("timed_out"));
    assert_eq!(context_commands[1]["duration_secs"].as_f64(), Some(30.0));
}

/// The `ledger` view is the run's account of the work it considered: every
/// lifecycle serializes with the evidence it holds and nothing else, under a
/// schema counter of its own.
#[test]
fn run_json_records_the_task_ledger() {
    use crate::checks::TreeState;
    use crate::ledger::{SubstrateKey, TaskEntry, TaskKey, TaskKind, TaskLedger, TaskState};

    let config = create_test_config(PolicyConfig::default());
    let resolved_target = ResolvedRef {
        name: "feature/ledger".to_string(),
        commit_id: "abc1234abc1234abc1234abc1234abc1234ab".to_string(),
        is_remote: false,
    };
    let resolved_bases = vec![ResolvedRef {
        name: "origin/main".to_string(),
        commit_id: "def5678def5678def5678def5678def5678de".to_string(),
        is_remote: true,
    }];

    let substrate = SubstrateKey {
        target_sha: Some("abc1234".to_string()),
        tree_state: Some(TreeState::Snapshot),
    };
    let ledger = TaskLedger::new();
    let record = |tool: &str, kind, state| {
        ledger.record(TaskEntry {
            key: TaskKey::new(tool, substrate.clone()),
            kind,
            state,
            queued_at: None,
            started_at: None,
        });
    };
    record(
        "TypeScript",
        TaskKind::Check,
        TaskState::Run {
            duration: Duration::from_millis(8130),
        },
    );
    record(
        "TypeScript",
        TaskKind::ContextArtifact,
        TaskState::Cached {
            cache_age_secs: Some(42),
            origin: SubstrateKey {
                target_sha: Some("older".to_string()),
                tree_state: Some(TreeState::LocalDirty),
            },
        },
    );
    record(
        "ESLint",
        TaskKind::ContextArtifact,
        TaskState::Reused {
            origin: substrate.clone(),
        },
    );
    record(
        "ESLint",
        TaskKind::Check,
        TaskState::Skipped {
            reason: "fast remote-only preset".to_string(),
        },
    );
    record(
        "cargo tree",
        TaskKind::ContextArtifact,
        TaskState::NotApplicable {
            reason: "no cargo project".to_string(),
        },
    );

    let summary_dir = tempfile::tempdir().expect("summary tempdir");
    generate_run_json_test!(
        summary_dir.path(),
        summary_dir.path(),
        &config,
        &[],
        None,
        &resolved_target,
        &resolved_bases,
        ("2026-03-08T12:00:00Z", 1.5),
        &[],
        &[],
        &[],
        None,
        &ledger,
    )
    .expect("run json");

    let raw = std::fs::read_to_string(summary_dir.path().join("RUN.json")).expect("read run json");
    let run: serde_json::Value = serde_json::from_str(&raw).expect("parse run json");

    assert_eq!(
        run["schema_version"].as_str(),
        Some("1.0"),
        "an additive view must not move the pack's schema version",
    );
    assert_eq!(run["ledger"]["schema"].as_u64(), Some(2));

    let entries = run["ledger"]["entries"].as_array().expect("ledger entries");
    assert_eq!(entries.len(), 5);

    assert_eq!(entries[0]["tool"].as_str(), Some("tsc"));
    assert_eq!(entries[0]["kind"].as_str(), Some("check"));
    assert_eq!(entries[0]["lifecycle"].as_str(), Some("run"));
    let duration = entries[0]["duration_secs"].as_f64().expect("duration");
    assert!(
        (duration - 8.13).abs() < 1e-4,
        "durations serialize as f32 seconds like the rest of RUN.json, got {duration}",
    );
    assert_eq!(
        entries[0]["substrate"],
        serde_json::json!({"target_sha": "abc1234", "tree_state": "snapshot"}),
        "the substrate speaks the same tree_state vocabulary as checks[]",
    );
    assert!(entries[0].get("reason").is_none());
    assert!(entries[0].get("cache_age_secs").is_none());

    assert_eq!(entries[1]["kind"].as_str(), Some("context_artifact"));
    assert_eq!(entries[1]["lifecycle"].as_str(), Some("cached"));
    assert_eq!(entries[1]["cache_age_secs"].as_u64(), Some(42));
    assert_eq!(
        entries[1]["origin"],
        serde_json::json!({"target_sha": "older", "tree_state": "local-dirty"}),
        "a replay reports the tree the ORIGINAL execution read",
    );

    assert_eq!(entries[2]["tool"].as_str(), Some("eslint"));
    assert_eq!(entries[2]["kind"].as_str(), Some("context_artifact"));
    assert_eq!(entries[2]["lifecycle"].as_str(), Some("reused"));
    assert_eq!(
        entries[2]["origin"],
        serde_json::json!({"target_sha": "abc1234", "tree_state": "snapshot"}),
        "reuse names the live gate's substrate, not a stored cache entry",
    );
    assert!(entries[2].get("cache_age_secs").is_none());
    assert!(entries[2].get("reason").is_none());

    assert_eq!(entries[3]["tool"].as_str(), Some("eslint"));
    assert_eq!(entries[3]["lifecycle"].as_str(), Some("skipped"));
    assert_eq!(
        entries[3]["reason"].as_str(),
        Some("fast remote-only preset")
    );

    assert_eq!(
        entries[4]["tool"].as_str(),
        Some("cargo_tree"),
        "a command with no gate counterpart is slugged from its own label",
    );
    assert_eq!(entries[4]["lifecycle"].as_str(), Some("not_applicable"));
}

/// The ledger already records queued_at vs started_at; the wire view must
/// keep that gap so a slow tool is distinguishable from a long resource wait.
#[test]
fn run_json_emits_queue_wait_when_admission_lagged() {
    use crate::checks::TreeState;
    use crate::ledger::{SubstrateKey, TaskEntry, TaskKey, TaskKind, TaskLedger, TaskState};

    let config = create_test_config(PolicyConfig::default());
    let resolved_target = ResolvedRef {
        name: "feature/queue-wait".to_string(),
        commit_id: "abc1234abc1234abc1234abc1234abc1234ab".to_string(),
        is_remote: false,
    };
    let resolved_bases = vec![ResolvedRef {
        name: "origin/main".to_string(),
        commit_id: "def5678def5678def5678def5678def5678de".to_string(),
        is_remote: true,
    }];
    let substrate = SubstrateKey {
        target_sha: Some("abc1234".to_string()),
        tree_state: Some(TreeState::Snapshot),
    };
    let started = std::time::Instant::now();
    let queued = started
        .checked_sub(Duration::from_secs(12))
        .expect("monotonic clock can subtract 12s");
    let ledger = TaskLedger::new();
    ledger.record(TaskEntry {
        key: TaskKey::new("Clippy", substrate),
        kind: TaskKind::Check,
        state: TaskState::Run {
            duration: Duration::from_secs(3),
        },
        queued_at: Some(queued),
        started_at: Some(started),
    });

    let summary_dir = tempfile::tempdir().expect("summary tempdir");
    generate_run_json_test!(
        summary_dir.path(),
        summary_dir.path(),
        &config,
        &[],
        None,
        &resolved_target,
        &resolved_bases,
        ("2026-03-08T12:00:00Z", 1.5),
        &[],
        &[],
        &[],
        None,
        &ledger,
    )
    .expect("run json");

    let raw = std::fs::read_to_string(summary_dir.path().join("RUN.json")).expect("read run json");
    let run: serde_json::Value = serde_json::from_str(&raw).expect("parse run json");
    let entries = run["ledger"]["entries"].as_array().expect("ledger entries");
    assert_eq!(entries.len(), 1);
    let wait = entries[0]["queue_wait_secs"]
        .as_f64()
        .expect("queue wait is serialized");
    assert!(
        (wait - 12.0).abs() < 0.05,
        "queue wait is started_at − queued_at in seconds, got {wait}"
    );
    assert!(
        entries[0].get("queued_at").is_none() && entries[0].get("started_at").is_none(),
        "absolute Instants are not part of the wire contract"
    );
}

/// A skip is decided before the run can know which tree it will read. By the
/// time `RUN.json` is written the run does know, and the entry must say so —
/// a reader of the pack cannot apply the in-memory fallback a later stage can.
#[test]
fn run_json_reports_the_reviewed_substrate_for_a_skip_recorded_before_it() {
    use crate::checks::TreeState;
    use crate::ledger::{SubstrateKey, TaskEntry, TaskKey, TaskKind, TaskLedger, TaskState};

    let config = create_test_config(PolicyConfig::default());
    let resolved_target = ResolvedRef {
        name: "feature/ledger".to_string(),
        commit_id: "abc1234abc1234abc1234abc1234abc1234ab".to_string(),
        is_remote: false,
    };
    let summary_dir = tempfile::tempdir().expect("summary tempdir");

    let ledger = TaskLedger::new();
    // The checks stage's first pass: no substrate resolved yet.
    ledger.record(TaskEntry {
        key: TaskKey::new("ESLint", SubstrateKey::default()),
        kind: TaskKind::Check,
        state: TaskState::Skipped {
            reason: "fast remote-only preset".to_string(),
        },
        queued_at: None,
        started_at: None,
    });
    // …and then the run materialises its shared snapshot.
    ledger.set_substrate(SubstrateKey {
        target_sha: Some("abc1234".to_string()),
        tree_state: Some(TreeState::Snapshot),
    });

    generate_run_json_test!(
        summary_dir.path(),
        summary_dir.path(),
        &config,
        &[],
        None,
        &resolved_target,
        &[],
        ("2026-03-08T12:00:00Z", 1.5),
        &[],
        &[],
        &[],
        None,
        &ledger,
    )
    .expect("run json");

    let raw = std::fs::read_to_string(summary_dir.path().join("RUN.json")).expect("read run json");
    let run: serde_json::Value = serde_json::from_str(&raw).expect("parse run json");
    assert_eq!(
        run["ledger"]["entries"][0]["substrate"],
        serde_json::json!({"target_sha": "abc1234", "tree_state": "snapshot"}),
        "the pack must name the tree the skip was a decision about",
    );
}

/// A run with no ledger entries still publishes the view, so a consumer can tell
/// "this pack records nothing" from "this pack is too old to have the section".
#[test]
fn run_json_ledger_view_is_always_present() {
    let config = create_test_config(PolicyConfig::default());
    let resolved_target = ResolvedRef {
        name: "feature/ledger".to_string(),
        commit_id: "abc1234abc1234abc1234abc1234abc1234ab".to_string(),
        is_remote: false,
    };
    let summary_dir = tempfile::tempdir().expect("summary tempdir");

    generate_run_json_test!(
        summary_dir.path(),
        summary_dir.path(),
        &config,
        &[],
        None,
        &resolved_target,
        &[],
        ("2026-03-08T12:00:00Z", 1.5),
        &[],
        &[],
        &[],
        None,
    )
    .expect("run json");

    let raw = std::fs::read_to_string(summary_dir.path().join("RUN.json")).expect("read run json");
    let run: serde_json::Value = serde_json::from_str(&raw).expect("parse run json");
    assert_eq!(run["ledger"]["schema"].as_u64(), Some(2));
    assert_eq!(
        run["ledger"]["entries"].as_array().map(Vec::len),
        Some(0),
        "an empty ledger is an empty list, not a missing section",
    );
}

#[test]
fn checks_status_lists_disabled_rust_quality_checks_as_skipped() {
    let mut config = create_test_config(PolicyConfig::default());
    config.run_tests = false;
    config.run_lint = false;
    let tmp = tempfile::tempdir().expect("tempdir");

    let checks = [build_heuristics_check(None, &config)];
    let skipped = [
        crate::checks::SkippedCheck {
            id: "clippy".to_string(),
            name: "Clippy".to_string(),
            reason: "lint disabled".to_string(),
        },
        crate::checks::SkippedCheck {
            id: "cargo_test".to_string(),
            name: "Cargo Test".to_string(),
            reason: "tests disabled".to_string(),
        },
    ];

    generate_checks_status_json(tmp.path(), &config, &checks, &skipped).expect("checks status");

    let raw =
        std::fs::read_to_string(tmp.path().join("checks-status.json")).expect("read checks status");
    let status: serde_json::Value = serde_json::from_str(&raw).expect("parse checks status");

    assert_eq!(status["clippy"].as_str(), Some("skipped (lint disabled)"));
    assert_eq!(
        status["cargo_test"].as_str(),
        Some("skipped (tests disabled)")
    );
    // cargo geiger is opt-in via --security-full: cleanly absent from the
    // status surface, never a "skipped (security disabled)" caveat.
    assert!(status.get("cargo_geiger").is_none());
    assert_eq!(
        status["heuristics_loctree"].as_str(),
        Some("skipped (heuristics disabled)")
    );
}

#[test]
fn checks_status_includes_geiger_when_security_full() {
    let mut config = create_test_config(PolicyConfig::default());
    config.security_full = true;
    let tmp = tempfile::tempdir().expect("tempdir");

    let skipped = [crate::checks::SkippedCheck {
        id: "cargo_geiger".to_string(),
        name: "Cargo Geiger".to_string(),
        reason: "tool not installed".to_string(),
    }];
    generate_checks_status_json(tmp.path(), &config, &[], &skipped).expect("checks status");

    let raw =
        std::fs::read_to_string(tmp.path().join("checks-status.json")).expect("read checks status");
    let status: serde_json::Value = serde_json::from_str(&raw).expect("parse checks status");

    // With the full tier opted in, geiger rejoins the status surface.
    assert!(status.get("cargo_geiger").is_some());
}

#[test]
fn checks_status_explains_fast_remote_only_preset() {
    let mut config = create_test_config(PolicyConfig::default());
    config.remote_only = true;
    config.run_tests = false;
    config.run_heuristics = false;
    let tmp = tempfile::tempdir().expect("tempdir");

    let checks = [build_heuristics_check(None, &config)];
    let skipped = ["Cargo Test", "Clippy", "Rustfmt"].map(|name| crate::checks::SkippedCheck {
        id: check_id_from_name(name),
        name: name.to_string(),
        reason: "fast remote-only preset".to_string(),
    });

    generate_checks_status_json(tmp.path(), &config, &checks, &skipped).expect("checks status");

    let raw =
        std::fs::read_to_string(tmp.path().join("checks-status.json")).expect("read checks status");
    let status: serde_json::Value = serde_json::from_str(&raw).expect("parse checks status");

    assert_eq!(
        status["cargo_test"].as_str(),
        Some("skipped (fast remote-only preset)")
    );
    assert_eq!(
        status["clippy"].as_str(),
        Some("skipped (fast remote-only preset)")
    );
    assert_eq!(
        status["rustfmt"].as_str(),
        Some("skipped (fast remote-only preset)")
    );
    assert_eq!(
        status["heuristics_loctree"].as_str(),
        Some("skipped (fast remote-only preset)")
    );
}

#[test]
fn checks_status_includes_loctree_heuristics_when_available() {
    let mut config = create_test_config(PolicyConfig::default());
    config.run_heuristics = true;
    let mut heuristics = HeuristicsResult::default();
    heuristics.loctree = Some(crate::heuristics::LoctreeAnalysis {
        available: true,
        ..Default::default()
    });
    heuristics.summary.total_files = 10; // non-zero so it's not skipped
    let tmp = tempfile::tempdir().expect("tempdir");
    let checks = [build_heuristics_check(Some(&heuristics), &config)];

    generate_checks_status_json(tmp.path(), &config, &checks, &[]).expect("checks status");

    let raw =
        std::fs::read_to_string(tmp.path().join("checks-status.json")).expect("read checks status");
    let status: serde_json::Value = serde_json::from_str(&raw).expect("parse checks status");
    assert_eq!(status["heuristics_loctree"].as_str(), Some("passed"));
}

#[test]
fn synthetic_heuristics_check_records_the_tree_it_scanned() {
    // heuristics_loctree gates the pack like any other check, so PROVENANCE.json
    // must be able to name the tree behind it. In snapshot mode that is the
    // extracted target tree — `git archive` writes the commit and nothing else,
    // so the scan really is exactly that commit.
    use crate::checks::TreeState;
    use crate::heuristics::{HeuristicsResult, HeuristicsSummary, LoctreeAnalysis};

    let heuristics = HeuristicsResult {
        loctree: Some(LoctreeAnalysis {
            available: true,
            ..Default::default()
        }),
        summary: HeuristicsSummary {
            total_files: 12,
            ..Default::default()
        },
        analysis_root: Some("/tmp/prview/repo/abc1234-1".to_string()),
        analysis_sha: Some("abc1234abc1234abc1234abc1234abc1234abc12".to_string()),
        started_at: Some("2026-08-22T10:00:00+02:00".to_string()),
        finished_at: Some("2026-08-22T10:00:04+02:00".to_string()),
        ..Default::default()
    };

    let config = create_test_config(PolicyConfig::default());
    let prov = build_heuristics_check(Some(&heuristics), &config)
        .provenance
        .expect("a gating signal must name its substrate");

    assert_eq!(prov.cwd, "/tmp/prview/repo/abc1234-1");
    assert_eq!(
        prov.target_sha.as_deref(),
        Some("abc1234abc1234abc1234abc1234abc1234abc12"),
    );
    assert_eq!(prov.tree_state, Some(TreeState::Snapshot));
    assert_eq!(prov.started_at, "2026-08-22T10:00:00+02:00");
}

#[test]
fn synthetic_heuristics_check_leaves_an_unnamed_snapshot_unknown() {
    // A pack written before the analysis commit was recorded still has its
    // analysis root. Report the directory, and stop there: guessing that it
    // holds the target commit is exactly the false certification the manifest
    // exists to prevent.
    use crate::heuristics::{HeuristicsResult, HeuristicsSummary, LoctreeAnalysis};

    let heuristics = HeuristicsResult {
        loctree: Some(LoctreeAnalysis {
            available: true,
            ..Default::default()
        }),
        summary: HeuristicsSummary {
            total_files: 12,
            ..Default::default()
        },
        analysis_root: Some("/tmp/prview/repo/older-pack".to_string()),
        ..Default::default()
    };

    let config = create_test_config(PolicyConfig::default());
    let prov = build_heuristics_check(Some(&heuristics), &config)
        .provenance
        .expect("the scanned directory is known even when its commit is not");

    assert_eq!(prov.cwd, "/tmp/prview/repo/older-pack");
    assert_eq!(prov.target_sha, None);
    assert_eq!(prov.tree_state, None);
}

#[test]
fn synthetic_heuristics_check_skips_zero_file_scan() {
    use crate::heuristics::{HeuristicsResult, HeuristicsSummary, LoctreeAnalysis};

    let heuristics = HeuristicsResult {
        loctree: Some(LoctreeAnalysis {
            available: true,
            ..Default::default()
        }),
        summary: HeuristicsSummary {
            total_files: 0,
            ..Default::default()
        },
        ..Default::default()
    };

    let config = create_test_config(PolicyConfig::default());
    let check = build_heuristics_check(Some(&heuristics), &config);

    assert_eq!(check.name, "heuristics_loctree");
    assert_eq!(check.status, CheckStatus::Skipped);
    assert!(check.output.contains("total_files=0"));
}

#[test]
fn merge_gate_marks_heuristics_disabled_as_not_run() {
    let config = create_test_config(PolicyConfig::default());
    let inline = InlineFindingsSummary {
        status: "passed".to_string(),
        findings_count: 0,
        dashboard_findings: vec![],
    };
    let resolved_target = ResolvedRef {
        name: "feature/runtime-target".to_string(),
        commit_id: "abc1234abc1234abc1234abc1234abc1234ab".to_string(),
        is_remote: false,
    };
    let resolved_bases = vec![ResolvedRef {
        name: "origin/main".to_string(),
        commit_id: "def5678def5678def5678def5678def5678de".to_string(),
        is_remote: true,
    }];

    let tmp = tempfile::tempdir().expect("tempdir");
    generate_merge_gate_test!(
        tmp.path(),
        &config,
        &[],
        Some(&HeuristicsResult::default()),
        &inline,
        &[],
        &CoverageDelta {
            total_source: 0,
            covered_count: 0,
            pct: None,
            uncovered: vec![],
            covered: vec![],
            non_code_count: 0,
            ghost_tests: vec![],
        },
        &[],
        &resolved_target,
        &resolved_bases,
    )
    .expect("merge gate");

    let raw = std::fs::read_to_string(tmp.path().join("MERGE_GATE.json")).expect("read gate");
    let gate: serde_json::Value = serde_json::from_str(&raw).expect("parse gate");
    let heuristics_check = gate["checks"]
        .as_array()
        .and_then(|checks| checks.iter().find(|c| c["id"] == "heuristics_loctree"))
        .expect("heuristics_loctree check");

    assert_eq!(heuristics_check["status"].as_str(), Some("skipped"));
    assert_eq!(heuristics_check["class"].as_str(), Some("SKIP"));
    assert_eq!(heuristics_check["blocking"].as_bool(), Some(false));
    for field in [
        "execution_state",
        "outcome",
        "policy_conclusion",
        "confidence_impact",
        "merge_impact",
        "reason",
    ] {
        assert!(
            heuristics_check.get(field).is_some(),
            "fallback heuristics row must preserve {field}"
        );
    }
}

#[test]
fn merge_gate_files_field_omits_inline_findings_path_when_no_findings() {
    let config = create_test_config(PolicyConfig::default());
    let inline = InlineFindingsSummary {
        status: "passed".to_string(),
        findings_count: 0,
        dashboard_findings: vec![],
    };
    let resolved_target = ResolvedRef {
        name: "main".to_string(),
        commit_id: "abc1234abc1234abc1234abc1234abc1234ab".to_string(),
        is_remote: false,
    };
    let resolved_bases = vec![];

    let tmp = tempfile::tempdir().expect("tempdir");
    generate_merge_gate_test!(
        tmp.path(),
        &config,
        &[],
        None,
        &inline,
        &[],
        &CoverageDelta {
            total_source: 0,
            covered_count: 0,
            pct: None,
            uncovered: vec![],
            covered: vec![],
            non_code_count: 0,
            ghost_tests: vec![],
        },
        &[],
        &resolved_target,
        &resolved_bases,
    )
    .expect("merge gate");

    let raw = std::fs::read_to_string(tmp.path().join("MERGE_GATE.json")).expect("read gate");
    let gate: serde_json::Value = serde_json::from_str(&raw).expect("parse gate");

    assert_eq!(
        gate["files"]["merge_gate_json"].as_str(),
        Some("00_summary/MERGE_GATE.json")
    );
    assert!(gate["inline_findings"]["file"].is_null());
    assert!(gate["files"]["inline_findings"].is_null());
    assert_eq!(
        gate["files"]["full_patch"].as_str(),
        Some("10_diff/full.patch")
    );
    assert_eq!(
        gate["files"]["checks_log"].as_str(),
        Some("20_quality/full-checks.log")
    );
}

#[test]
fn merge_gate_includes_inline_findings_path_when_sarif_exists() {
    let config = create_test_config(PolicyConfig::default());
    let inline = InlineFindingsSummary {
        status: "warnings".to_string(),
        findings_count: 1,
        dashboard_findings: vec![],
    };
    let resolved_target = ResolvedRef {
        name: "main".to_string(),
        commit_id: "abc1234abc1234abc1234abc1234abc1234ab".to_string(),
        is_remote: false,
    };
    let resolved_bases = vec![];

    let tmp = tempfile::tempdir().expect("tempdir");
    generate_merge_gate_test!(
        tmp.path(),
        &config,
        &[],
        None,
        &inline,
        &[],
        &CoverageDelta {
            total_source: 0,
            covered_count: 0,
            pct: None,
            uncovered: vec![],
            covered: vec![],
            non_code_count: 0,
            ghost_tests: vec![],
        },
        &[],
        &resolved_target,
        &resolved_bases,
    )
    .expect("merge gate");

    let raw = std::fs::read_to_string(tmp.path().join("MERGE_GATE.json")).expect("read gate");
    let gate: serde_json::Value = serde_json::from_str(&raw).expect("parse gate");

    assert_eq!(
        gate["inline_findings"]["file"].as_str(),
        Some("30_context/INLINE_FINDINGS.sarif")
    );
    assert_eq!(
        gate["files"]["inline_findings"].as_str(),
        Some("30_context/INLINE_FINDINGS.sarif")
    );
}

#[test]
fn merge_gate_surfaces_review_caveats_when_merge_needs_review() {
    let config = create_test_config(PolicyConfig::default());
    let checks = vec![
        CheckResult {
            name: "Cargo test".to_string(),
            status: CheckStatus::Passed,
            duration: Duration::from_secs(1),
            output: "ok".to_string(),
            cached: false,
            provenance: None,
        },
        CheckResult {
            name: "Clippy".to_string(),
            status: CheckStatus::Passed,
            duration: Duration::from_secs(1),
            output: "ok".to_string(),
            cached: false,
            provenance: None,
        },
    ];
    let inline = InlineFindingsSummary {
        status: "warnings".to_string(),
        findings_count: 1,
        dashboard_findings: vec![DashboardFinding {
            level: "warning",
            check_name: "heuristics_loctree".to_string(),
            check_id: "heuristics_loctree".to_string(),
            message: "dead exports=1".to_string(),
            in_diff: Some(true),
        }],
    };
    let breaking = vec![BreakingFinding {
        file: "src/lib.rs".to_string(),
        kind: BreakingKind::RemovedSymbol {
            symbol_type: "function".to_string(),
        },
        line: "pub fn old_api()".to_string(),
        risk_level: BreakingRisk::High,
    }];
    let coverage = CoverageDelta {
        total_source: 4,
        covered_count: 1,
        pct: Some(25),
        uncovered: vec![crate::artifacts::signal::CoverageFile {
            status: 'M',
            path: "src/lib.rs".to_string(),
        }],
        covered: vec![],
        non_code_count: 0,
        ghost_tests: vec![],
    };
    let resolved_target = ResolvedRef {
        name: "main".to_string(),
        commit_id: "abc1234abc1234abc1234abc1234abc1234ab".to_string(),
        is_remote: false,
    };
    let resolved_bases = vec![];

    let tmp = tempfile::tempdir().expect("tempdir");
    generate_merge_gate_test!(
        tmp.path(),
        &config,
        &checks,
        None,
        &inline,
        &breaking,
        &coverage,
        &[],
        &resolved_target,
        &resolved_bases,
    )
    .expect("merge gate");

    let raw = std::fs::read_to_string(tmp.path().join("MERGE_GATE.json")).expect("read gate");
    let gate: serde_json::Value = serde_json::from_str(&raw).expect("parse gate");
    assert_eq!(
        gate["decision"]["recommended_label"].as_str(),
        Some("MERGE WITH REVIEW")
    );
    // Three descriptive caveats (breaking breakdown, coverage, inline finding)
    // plus the breaking-change escalation reason caveat (critic-1).
    assert_eq!(
        gate["decision"]["review_caveats"]
            .as_array()
            .map(|items| items.len()),
        Some(4)
    );
    assert!(
        gate["decision"]["review_caveats"]
            .as_array()
            .is_some_and(|items| items.iter().any(|item| item
                .as_str()
                .is_some_and(|text| text == "breaking API change detected: 1 finding"))),
        "breaking change must escalate with an explicit reason caveat"
    );
}

#[test]
fn build_review_caveats_include_orphaned_test_candidates() {
    let coverage = CoverageDelta {
        total_source: 2,
        covered_count: 2,
        pct: Some(100),
        uncovered: vec![],
        covered: vec![],
        non_code_count: 0,
        ghost_tests: vec![crate::artifacts::signal::CoveragePair {
            src_status: 'D',
            src_path: "src/foo.rs".to_string(),
            test_status: 'M',
            test_path: "tests/foo_test.rs".to_string(),
            tier: crate::artifacts::signal::CoverageMatchTier::High,
        }],
    };

    let caveats = build_review_caveats(&[], &coverage, 0);

    assert!(
        caveats
            .iter()
            .any(|caveat| caveat == "1 orphaned test candidate")
    );
}

#[test]
fn merge_gate_splits_introduced_and_preexisting_inline_findings() {
    let config = create_test_config(PolicyConfig::default());
    let mk = |in_diff: bool| DashboardFinding {
        level: "warning",
        check_name: "Semgrep scan".to_string(),
        check_id: "semgrep_scan".to_string(),
        message: "finding".to_string(),
        in_diff: Some(in_diff),
    };
    let inline = InlineFindingsSummary {
        status: "warnings".to_string(),
        findings_count: 5,
        dashboard_findings: vec![mk(true), mk(false), mk(false), mk(false), mk(false)],
    };
    let resolved_target = ResolvedRef {
        name: "main".to_string(),
        commit_id: "abc1234abc1234abc1234abc1234abc1234ab".to_string(),
        is_remote: false,
    };
    let resolved_bases = vec![];

    let tmp = tempfile::tempdir().expect("tempdir");
    generate_merge_gate_test!(
        tmp.path(),
        &config,
        &[],
        None,
        &inline,
        &[],
        &CoverageDelta {
            total_source: 0,
            covered_count: 0,
            pct: None,
            uncovered: vec![],
            covered: vec![],
            non_code_count: 0,
            ghost_tests: vec![],
        },
        &[],
        &resolved_target,
        &resolved_bases,
    )
    .expect("merge gate");

    let raw = std::fs::read_to_string(tmp.path().join("MERGE_GATE.json")).expect("read gate");
    let gate: serde_json::Value = serde_json::from_str(&raw).expect("parse gate");
    let inline_obj = &gate["inline_findings"];
    assert_eq!(inline_obj["findings_count"].as_u64(), Some(5));
    assert_eq!(
        inline_obj["introduced_count"].as_u64(),
        Some(1),
        "one finding is in the diff"
    );
    assert_eq!(
        inline_obj["preexisting_count"].as_u64(),
        Some(4),
        "four findings are pre-existing whole-repo debt"
    );
}

#[test]
fn merge_gate_splits_preexisting_quality_failures_from_inline_findings() {
    let config = create_test_config(PolicyConfig::default());
    let cargo_root = tempfile::tempdir().expect("cargo root");
    let checks = vec![
        CheckResult {
            name: "Cargo audit".to_string(),
            status: CheckStatus::Failed,
            duration: Duration::from_secs(1),
            output: sample_cargo_audit_output(),
            cached: false,
            provenance: Some(CheckProvenance {
                command: "cargo audit --json".to_string(),
                tool_version: None,
                cwd: cargo_root.path().display().to_string(),
                exit_code: Some(1),
                started_at: "2026-01-01T00:00:00Z".to_string(),
                finished_at: "2026-01-01T00:00:01Z".to_string(),
                hard_fail_signatures: vec![],
                cache_key: None,
                target_sha: None,
                tree_state: None,
            }),
        },
        // Satisfy required Rust quality signals so they don't add unclassified gaps
        CheckResult {
            name: "Cargo test".to_string(),
            status: CheckStatus::Passed,
            duration: Duration::from_secs(1),
            output: "test result: ok".to_string(),
            cached: false,
            provenance: None,
        },
        CheckResult {
            name: "Clippy".to_string(),
            status: CheckStatus::Passed,
            duration: Duration::from_secs(1),
            output: "ok".to_string(),
            cached: false,
            provenance: None,
        },
    ];
    let inline = generate_inline_findings(cargo_root.path(), &checks, &[], None, None)
        .expect("inline findings");
    let resolved_target = ResolvedRef {
        name: "main".to_string(),
        commit_id: "abc1234abc1234abc1234abc1234abc1234ab".to_string(),
        is_remote: false,
    };
    let resolved_bases = vec![ResolvedRef {
        name: "origin/main".to_string(),
        commit_id: "def5678def5678def5678def5678def5678de".to_string(),
        is_remote: true,
    }];

    generate_merge_gate_test!(
        cargo_root.path(),
        &config,
        &checks,
        None,
        &inline,
        &[],
        &CoverageDelta {
            total_source: 0,
            covered_count: 0,
            pct: None,
            uncovered: vec![],
            covered: vec![],
            non_code_count: 0,
            ghost_tests: vec![],
        },
        &[],
        &resolved_target,
        &resolved_bases,
    )
    .expect("merge gate");

    let raw =
        std::fs::read_to_string(cargo_root.path().join("MERGE_GATE.json")).expect("read gate");
    let gate: serde_json::Value = serde_json::from_str(&raw).expect("parse gate");

    // Pre-existing failures still appear in quality_failures (backward compat)
    assert!(
        gate["decision"]["quality_failures"]
            .as_array()
            .is_some_and(|failures| failures.iter().any(|value| value == "Cargo audit"))
    );
    // ...and are also classified separately
    assert!(
        gate["decision"]["preexisting_quality_failures"]
            .as_array()
            .is_some_and(|failures| failures.iter().any(|value| value == "Cargo audit"))
    );
    assert!(
        gate["decision"]["quality_failure_details"]
            .as_array()
            .is_some_and(|details| details.iter().any(|detail| {
                detail["name"] == "Cargo audit"
                    && detail["classification"].as_str() == Some("pre-existing")
            }))
    );
    // Pre-existing failures do NOT block: quality_pass should be true
    assert_eq!(gate["decision"]["quality_pass"].as_bool(), Some(true));
    // Pre-existing failures surface as review caveats
    assert!(
        gate["decision"]["review_caveats"]
            .as_array()
            .is_some_and(|caveats| caveats.iter().any(|c| c
                .as_str()
                .is_some_and(|s| s.contains("Pre-existing") && s.contains("Cargo audit"))))
    );
}

#[test]
fn merge_gate_reason_mentions_preexisting_failures_under_merge_with_review() {
    let config = test_config_builder()
        .target(Some("feature/ui-lint"))
        .bases(&["main"])
        .profile(test_js_profile(true))
        .execution_mode(ExecutionMode::Standard)
        .run_tests(false)
        .run_lint(true)
        .do_fetch(false)
        .use_cache(false)
        .create_zip(false)
        .policy(PolicyConfig::default())
        .build();
    let checks = vec![CheckResult {
        name: "ESLint".to_string(),
        status: CheckStatus::Failed,
        duration: Duration::from_secs(1),
        output: "src/e2e/legacy.spec.ts:10:5: error Unexpected any".to_string(),
        cached: false,
        provenance: None,
    }];
    let inline = InlineFindingsSummary {
        status: "warnings".to_string(),
        findings_count: 1,
        dashboard_findings: vec![DashboardFinding {
            level: "error",
            check_name: "ESLint".to_string(),
            check_id: "eslint".to_string(),
            message: "Unexpected any".to_string(),
            in_diff: Some(false),
        }],
    };
    let resolved_target = ResolvedRef {
        name: "main".to_string(),
        commit_id: "abc1234abc1234abc1234abc1234abc1234ab".to_string(),
        is_remote: false,
    };
    let resolved_bases = vec![ResolvedRef {
        name: "origin/main".to_string(),
        commit_id: "def5678def5678def5678def5678def5678de".to_string(),
        is_remote: true,
    }];
    let tmp = tempfile::tempdir().expect("tempdir");

    generate_merge_gate_test!(
        tmp.path(),
        &config,
        &checks,
        None,
        &inline,
        &[],
        &CoverageDelta {
            total_source: 0,
            covered_count: 0,
            pct: None,
            uncovered: vec![],
            covered: vec![],
            non_code_count: 0,
            ghost_tests: vec![],
        },
        &[],
        &resolved_target,
        &resolved_bases,
    )
    .expect("merge gate");

    let raw = std::fs::read_to_string(tmp.path().join("MERGE_GATE.json")).expect("read gate");
    let gate: serde_json::Value = serde_json::from_str(&raw).expect("parse gate");

    // Pre-existing failures no longer block or degrade the verdict, but they do
    // require explicit review.
    assert_eq!(
        gate["decision"]["recommended_label"].as_str(),
        Some("MERGE WITH REVIEW")
    );
    assert_eq!(gate["decision"]["quality_pass"].as_bool(), Some(true));
    // Pre-existing failures surface as a review caveat alongside the inline finding
    assert!(
        gate["decision"]["review_caveats"]
            .as_array()
            .is_some_and(|caveats| caveats.iter().any(|c| c
                .as_str()
                .is_some_and(|s| s.contains("Pre-existing") && s.contains("ESLint"))))
    );
    assert!(
        gate["decision"]["decision_reason"]
            .as_str()
            .is_some_and(|reason| {
                reason.contains("1 pre-existing") && reason.contains("review signal")
            })
    );
}

#[test]
fn merge_gate_marks_skipped_rust_quality_signals_as_review_caveats() {
    let config = create_test_config(PolicyConfig::default());

    let inline = InlineFindingsSummary {
        status: "passed".to_string(),
        findings_count: 0,
        dashboard_findings: vec![],
    };
    let resolved_target = ResolvedRef {
        name: "main".to_string(),
        commit_id: "abc1234abc1234abc1234abc1234abc1234ab".to_string(),
        is_remote: false,
    };
    let resolved_bases = vec![];
    let skipped = vec![
        crate::checks::SkippedCheck {
            id: "cargo_test".to_string(),
            name: "Cargo test".to_string(),
            reason: "disabled for this run".to_string(),
        },
        crate::checks::SkippedCheck {
            id: "clippy".to_string(),
            name: "Clippy".to_string(),
            reason: "disabled for this run".to_string(),
        },
    ];

    let tmp = tempfile::tempdir().expect("tempdir");
    generate_merge_gate_test!(
        tmp.path(),
        &config,
        &[],
        None,
        &inline,
        &[],
        &CoverageDelta {
            total_source: 0,
            covered_count: 0,
            pct: None,
            uncovered: vec![],
            covered: vec![],
            non_code_count: 0,
            ghost_tests: vec![],
        },
        &skipped,
        &resolved_target,
        &resolved_bases,
    )
    .expect("merge gate");

    let raw = std::fs::read_to_string(tmp.path().join("MERGE_GATE.json")).expect("read gate");
    let gate: serde_json::Value = serde_json::from_str(&raw).expect("parse gate");

    // PolicyEngine: skipped checks with Warn severity → Advisory → review caveats
    let caveats = gate["decision"]["review_caveats"]
        .as_array()
        .expect("review_caveats array");
    assert!(caveats.iter().any(|item| {
        item.as_str()
            .is_some_and(|s| s.contains("Cargo test") && s.contains("skipped"))
    }));
    assert!(caveats.iter().any(|item| {
        item.as_str()
            .is_some_and(|s| s.contains("Clippy") && s.contains("skipped"))
    }));
    // Analysis should be degraded due to skipped Warn-severity checks
    assert_eq!(
        gate["decision"]["analysis_status"].as_str(),
        Some("degraded")
    );
}

#[test]
fn merge_gate_surfaces_skipped_cargo_geiger_when_security_was_requested() {
    let mut config = create_test_config(PolicyConfig::default());
    config.run_security = true;

    let inline = InlineFindingsSummary {
        status: "passed".to_string(),
        findings_count: 0,
        dashboard_findings: vec![],
    };
    let resolved_target = ResolvedRef {
        name: "main".to_string(),
        commit_id: "abc1234abc1234abc1234abc1234abc1234ab".to_string(),
        is_remote: false,
    };
    let resolved_bases = vec![];
    let skipped_checks = vec![crate::checks::SkippedCheck {
        id: "cargo_geiger".to_string(),
        name: "Cargo geiger".to_string(),
        reason: "cannot run in current context (profile=Rust)".to_string(),
    }];

    let tmp = tempfile::tempdir().expect("tempdir");
    generate_merge_gate_test!(
        tmp.path(),
        &config,
        &[],
        None,
        &inline,
        &[],
        &CoverageDelta {
            total_source: 0,
            covered_count: 0,
            pct: None,
            uncovered: vec![],
            covered: vec![],
            non_code_count: 0,
            ghost_tests: vec![],
        },
        &skipped_checks,
        &resolved_target,
        &resolved_bases,
    )
    .expect("merge gate");

    let raw = std::fs::read_to_string(tmp.path().join("MERGE_GATE.json")).expect("read gate");
    let gate: serde_json::Value = serde_json::from_str(&raw).expect("parse gate");
    // PolicyEngine: skipped check with Warn severity → Advisory → review caveat
    assert!(
        gate["decision"]["review_caveats"]
            .as_array()
            .is_some_and(|items| items.iter().any(|item| item
                .as_str()
                .is_some_and(|s| s.contains("Cargo geiger") && s.contains("skipped"))))
    );
}

#[test]
fn merge_gate_surfaces_runtime_skipped_cargo_geiger() {
    // A runtime Skipped (timeout or virtual manifest) lands in `checks`, not
    // `skipped_checks`; the gate must still surface the security advisory.
    for (output, label) in [
        (
            "cargo geiger skipped: cargo timed out after 600s",
            "timed out",
        ),
        (
            "error: the manifest is a virtual manifest, but this command requires running against an actual package",
            "virtual manifest",
        ),
    ] {
        let mut config = create_test_config(PolicyConfig::default());
        config.run_security = true;

        let inline = InlineFindingsSummary {
            status: "passed".to_string(),
            findings_count: 0,
            dashboard_findings: vec![],
        };
        let resolved_target = ResolvedRef {
            name: "main".to_string(),
            commit_id: "abc1234abc1234abc1234abc1234abc1234ab".to_string(),
            is_remote: false,
        };
        let resolved_bases = vec![];
        let checks = vec![CheckResult {
            name: "Cargo geiger".to_string(),
            status: CheckStatus::Skipped,
            duration: std::time::Duration::ZERO,
            output: output.to_string(),
            cached: false,
            provenance: None,
        }];

        let tmp = tempfile::tempdir().expect("tempdir");
        generate_merge_gate_test!(
            tmp.path(),
            &config,
            &checks,
            None,
            &inline,
            &[],
            &CoverageDelta {
                total_source: 0,
                covered_count: 0,
                pct: None,
                uncovered: vec![],
                covered: vec![],
                non_code_count: 0,
                ghost_tests: vec![],
            },
            &[], // no pre-run skipped_checks — the skip happened at runtime
            &resolved_target,
            &resolved_bases,
        )
        .expect("merge gate");

        let raw = std::fs::read_to_string(tmp.path().join("MERGE_GATE.json")).expect("read gate");
        let gate: serde_json::Value = serde_json::from_str(&raw).expect("parse gate");
        let caveats: Vec<&str> = gate["decision"]["review_caveats"]
            .as_array()
            .expect("caveats array")
            .iter()
            .filter_map(|v| v.as_str())
            .collect();
        let matching = caveats
            .iter()
            .filter(|c| c.starts_with("cargo geiger skipped for this run"))
            .count();
        assert_eq!(
            matching, 1,
            "exactly one geiger skip caveat expected for {label}, got {caveats:?}"
        );
    }
}

#[test]
fn merge_gate_surfaces_cargo_audit_informational_warnings_as_review_caveat() {
    // The status here used to be `Passed`, which was a fiction: a cargo-audit run
    // carrying an unmaintained-crate advisory reports `Warnings`. The injected
    // `Passed` kept the check out of the quality summary entirely and therefore
    // masked the warning→failure bug this test now also guards.
    let config = create_test_config(PolicyConfig::default());
    let checks = vec![CheckResult {
            name: "Cargo audit".to_string(),
            status: CheckStatus::Warnings,
            duration: Duration::from_secs(1),
            output: r#"{"vulnerabilities":{"found":false,"count":0,"list":[]},"warnings":{"unmaintained":[{"kind":"unmaintained","package":{"name":"paste","version":"1.0.15"},"advisory":{"id":"RUSTSEC-2024-0436"}}]}}"#.to_string(),
            cached: false,
            provenance: None,
        }];
    let inline = InlineFindingsSummary {
        status: "passed".to_string(),
        findings_count: 0,
        dashboard_findings: vec![],
    };
    let resolved_target = ResolvedRef {
        name: "main".to_string(),
        commit_id: "abc1234abc1234abc1234abc1234abc1234ab".to_string(),
        is_remote: false,
    };
    let resolved_bases = vec![];
    let tmp = tempfile::tempdir().expect("tempdir");

    generate_merge_gate_test!(
        tmp.path(),
        &config,
        &checks,
        None,
        &inline,
        &[],
        &CoverageDelta {
            total_source: 0,
            covered_count: 0,
            pct: None,
            uncovered: vec![],
            covered: vec![],
            non_code_count: 0,
            ghost_tests: vec![],
        },
        &[],
        &resolved_target,
        &resolved_bases,
    )
    .expect("merge gate");

    let raw = std::fs::read_to_string(tmp.path().join("MERGE_GATE.json")).expect("read gate");
    let gate: serde_json::Value = serde_json::from_str(&raw).expect("parse gate");
    assert!(
        gate["decision"]["review_caveats"]
            .as_array()
            .is_some_and(|items| items.iter().any(|item| item
                .as_str()
                .is_some_and(|text| text.contains("Cargo audit note: 1 informational advisory"))))
    );
    assert!(
        gate["decision"]["review_caveats"]
            .as_array()
            .is_some_and(|items| items.iter().any(|item| item
                .as_str()
                .is_some_and(|text| text.contains("paste (unmaintained)"))))
    );

    // A warning-level check that produced no locatable finding classifies as
    // `Unclassified`, but it is a WARNING — it must not be counted as a failed
    // quality check. Before the origin split this exact shape flipped
    // `quality_pass` to false and printed "1 quality check failed".
    assert_eq!(
        gate["decision"]["quality_pass"].as_bool(),
        Some(true),
        "an unlocated warning is not a quality failure: {}",
        gate["decision"]
    );
    assert!(
        !raw.contains("quality check failed") && !raw.contains("quality checks failed"),
        "MERGE_GATE.json must not describe warnings as failed quality checks: {raw}"
    );
    // The verdict itself is unchanged: Warnings still reach the policy engine as
    // an advisory signal, so the run stays CONDITIONAL — only the label is honest.
    assert_eq!(
        gate["decision"]["verdict"].as_str(),
        Some("CONDITIONAL"),
        "warning-level advisory keeps the CONDITIONAL verdict"
    );
    // …and the analysis is no longer degraded by a phantom quality failure.
    assert_eq!(
        gate["decision"]["analysis_status"].as_str(),
        Some("complete"),
        "no failed check means the analysis is complete, not degraded"
    );
}

#[test]
fn merge_gate_names_the_origin_of_every_quality_failure_entry() {
    // The origin split lives in memory only until the pack states it. Read from
    // disk, `introduced_quality_failures: ["Rustfmt"]` next to `quality_pass:
    // true` is a pack contradicting itself: the array claims a failure, the flag
    // claims nothing failed, and nothing in the JSON explains which is right.
    let config = create_test_config(PolicyConfig::default());
    let checks = vec![
        CheckResult {
            name: "Rustfmt".to_string(),
            status: CheckStatus::Warnings,
            duration: Duration::from_secs(1),
            output: "Diff in src/new.rs at line 1".to_string(),
            cached: false,
            provenance: None,
        },
        CheckResult {
            name: "Clippy".to_string(),
            status: CheckStatus::Failed,
            duration: Duration::from_secs(1),
            output: "src/new.rs:1: error".to_string(),
            cached: false,
            provenance: None,
        },
    ];
    let inline = InlineFindingsSummary {
        status: "passed".to_string(),
        findings_count: 0,
        dashboard_findings: vec![],
    };
    let resolved_target = ResolvedRef {
        name: "main".to_string(),
        commit_id: "abc1234abc1234abc1234abc1234abc1234ab".to_string(),
        is_remote: false,
    };
    let tmp = tempfile::tempdir().expect("tempdir");

    generate_merge_gate_test!(
        tmp.path(),
        &config,
        &checks,
        None,
        &inline,
        &[],
        &CoverageDelta {
            total_source: 0,
            covered_count: 0,
            pct: None,
            uncovered: vec![],
            covered: vec![],
            non_code_count: 0,
            ghost_tests: vec![],
        },
        &[],
        &resolved_target,
        &[],
    )
    .expect("merge gate");

    let raw = std::fs::read_to_string(tmp.path().join("MERGE_GATE.json")).expect("read gate");
    let gate: serde_json::Value = serde_json::from_str(&raw).expect("parse gate");

    let details = gate["decision"]["quality_failure_details"]
        .as_array()
        .expect("details array");
    let origin_of = |name: &str| {
        details
            .iter()
            .find(|detail| detail["name"] == name)
            .unwrap_or_else(|| panic!("`{name}` missing from quality_failure_details: {details:?}"))
            ["origin"]
            .as_str()
            .map(str::to_string)
    };
    assert_eq!(
        origin_of("Rustfmt"),
        Some("warning".to_string()),
        "a warning-level entry must say so on the wire: {gate}"
    );
    assert_eq!(
        origin_of("Clippy"),
        Some("failure".to_string()),
        "a hard failure must stay distinguishable from a warning: {gate}"
    );

    // A new readable field is a MINOR schema change; a pack that carries it and
    // still claims 2.1 lies to `tools/validate_merge_gate.py` and to any reader
    // deciding whether `origin` can be trusted to be present.
    assert_eq!(
        gate["schema_version"].as_str(),
        Some(crate::gate::MERGE_GATE_SCHEMA_VERSION),
        "the pack stamps the schema this build writes"
    );
    assert_eq!(
        crate::gate::MERGE_GATE_SCHEMA_VERSION,
        "2.3",
        "typed enforcement disposition and its proof bump the MINOR after origin"
    );
}

#[test]
fn merge_gate_does_not_fail_fast_remote_only_for_expected_rust_gaps() {
    let mut config = create_test_config(PolicyConfig::default());
    config.run_tests = false;
    config.run_lint = true;
    config.remote_only = true;

    let inline = InlineFindingsSummary {
        status: "passed".to_string(),
        findings_count: 0,
        dashboard_findings: vec![],
    };
    let resolved_target = ResolvedRef {
        name: "main".to_string(),
        commit_id: "abc1234abc1234abc1234abc1234abc1234ab".to_string(),
        is_remote: false,
    };
    let resolved_bases = vec![];

    let tmp = tempfile::tempdir().expect("tempdir");
    generate_merge_gate_test!(
        tmp.path(),
        &config,
        &[],
        None,
        &inline,
        &[],
        &CoverageDelta {
            total_source: 0,
            covered_count: 0,
            pct: None,
            uncovered: vec![],
            covered: vec![],
            non_code_count: 0,
            ghost_tests: vec![],
        },
        &[],
        &resolved_target,
        &resolved_bases,
    )
    .expect("merge gate");

    let raw = std::fs::read_to_string(tmp.path().join("MERGE_GATE.json")).expect("read gate");
    let gate: serde_json::Value = serde_json::from_str(&raw).expect("parse gate");
    assert_eq!(
        gate["decision"]["quality_failures"]
            .as_array()
            .map(|items| items.len()),
        Some(0)
    );
    assert!(
        !gate["decision"]["review_caveats"]
            .as_array()
            .is_some_and(|items| items
                .iter()
                .any(|item| item.as_str() == Some("cargo test skipped for this run")))
    );
    assert!(
        !gate["decision"]["review_caveats"]
            .as_array()
            .is_some_and(|items| items
                .iter()
                .any(|item| item.as_str() == Some("clippy skipped for this run")))
    );
}

#[test]
fn merge_gate_blocks_missing_rust_quality_signal_when_policy_sets_block() {
    let mut policy = PolicyConfig {
        mode: PolicyMode::Warn,
        ..PolicyConfig::default()
    };
    policy
        .checks
        .insert("cargo_test".to_string(), PolicySeverity::Block);

    let config = create_test_config(policy);

    let inline = InlineFindingsSummary {
        status: "passed".to_string(),
        findings_count: 0,
        dashboard_findings: vec![],
    };
    let resolved_target = ResolvedRef {
        name: "main".to_string(),
        commit_id: "abc1234abc1234abc1234abc1234abc1234ab".to_string(),
        is_remote: false,
    };
    let resolved_bases = vec![];
    let skipped = vec![crate::checks::SkippedCheck {
        id: "cargo_test".to_string(),
        name: "Cargo test".to_string(),
        reason: "disabled for this run".to_string(),
    }];

    let tmp = tempfile::tempdir().expect("tempdir");
    generate_merge_gate_test!(
        tmp.path(),
        &config,
        &[CheckResult {
            name: "Clippy".to_string(),
            status: CheckStatus::Passed,
            duration: Duration::from_secs(1),
            output: "ok".to_string(),
            cached: false,
            provenance: None,
        }],
        None,
        &inline,
        &[],
        &CoverageDelta {
            total_source: 0,
            covered_count: 0,
            pct: None,
            uncovered: vec![],
            covered: vec![],
            non_code_count: 0,
            ghost_tests: vec![],
        },
        &skipped,
        &resolved_target,
        &resolved_bases,
    )
    .expect("merge gate");

    let raw = std::fs::read_to_string(tmp.path().join("MERGE_GATE.json")).expect("read gate");
    let gate: serde_json::Value = serde_json::from_str(&raw).expect("parse gate");

    // PolicyEngine: skipped check with Block severity → Blocked conclusion
    assert_eq!(gate["decision"]["allow_merge"].as_bool(), Some(false));
    assert_eq!(gate["decision"]["verdict"].as_str(), Some("BLOCK"));
    assert_eq!(
        gate["decision"]["analysis_status"].as_str(),
        Some("incomplete")
    );
}

#[test]
fn merge_gate_skipped_cargo_geiger_with_ignore_severity_produces_no_caveat() {
    // PolicyEngine: skipped check with Ignore severity → Satisfied → no caveat
    let mut policy = PolicyConfig::default();
    policy
        .checks
        .insert("cargo_geiger".to_string(), PolicySeverity::Ignore);
    let config = create_test_config(policy);
    let tmp = tempfile::tempdir().expect("tempdir");
    let inline = InlineFindingsSummary {
        status: "passed".to_string(),
        findings_count: 0,
        dashboard_findings: vec![],
    };
    let resolved_target = ResolvedRef {
        name: "feature/runtime-target".to_string(),
        commit_id: "abc1234abc1234abc1234abc1234abc1234ab".to_string(),
        is_remote: false,
    };
    let resolved_bases = vec![ResolvedRef {
        name: "main".to_string(),
        commit_id: "def5678def5678def5678def5678def5678de".to_string(),
        is_remote: false,
    }];

    let skipped = vec![crate::checks::SkippedCheck {
        id: "cargo_geiger".to_string(),
        name: "Cargo geiger".to_string(),
        reason: "cannot run in current context".to_string(),
    }];

    generate_merge_gate_test!(
        tmp.path(),
        &config,
        &[],
        None,
        &inline,
        &[],
        &CoverageDelta {
            total_source: 0,
            covered_count: 0,
            pct: None,
            uncovered: vec![],
            covered: vec![],
            non_code_count: 0,
            ghost_tests: vec![],
        },
        &skipped,
        &resolved_target,
        &resolved_bases,
    )
    .expect("merge gate");

    let raw = std::fs::read_to_string(tmp.path().join("MERGE_GATE.json")).expect("read gate");
    let gate: serde_json::Value = serde_json::from_str(&raw).expect("parse gate");
    let caveats = gate["decision"]["review_caveats"]
        .as_array()
        .expect("caveats array");
    assert!(
        !caveats.iter().any(|c| {
            let s = c.as_str().unwrap_or("");
            s.contains("Cargo geiger")
        }),
        "Ignore-severity skipped check should not produce caveat, got: {:?}",
        caveats
    );
}

#[test]
fn failures_summary_turns_cargo_audit_into_human_summary() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let summary_dir = tmp.path().join("00_summary");
    std::fs::create_dir_all(&summary_dir).expect("summary dir");
    std::fs::create_dir_all(tmp.path().join("30_context")).expect("context dir");
    std::fs::write(
        tmp.path().join("30_context/cargo-tree.txt"),
        sample_cargo_tree_output(),
    )
    .expect("cargo tree");
    let checks = vec![CheckResult {
        name: "Cargo audit".to_string(),
        status: CheckStatus::Failed,
        duration: Duration::from_secs(2),
        output: sample_cargo_audit_output(),
        cached: false,
        provenance: None,
    }];

    generate_failures_summary(&summary_dir, &checks).expect("failures summary");
    let content = std::fs::read_to_string(summary_dir.join("FAILURES_SUMMARY.md")).expect("read");

    assert!(content.contains("2 security advisories affecting 2 locked dependencies"));
    assert!(content.contains("### Advisories"));
    assert!(content.contains("`RUSTSEC-2024-0001` critical in `example-crate@0.3.1`"));
    assert!(content.contains("Fix: `>=0.3.2`"));
    assert!(!content.contains("\"vulnerabilities\""));
    assert!(!content.contains("\"count\": 2"));
}

#[test]
fn failures_summary_is_written_when_no_checks_failed() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let summary_dir = tmp.path().join("00_summary");
    std::fs::create_dir_all(&summary_dir).expect("summary dir");
    let checks = vec![CheckResult {
        name: "Cargo test".to_string(),
        status: CheckStatus::Passed,
        duration: Duration::from_secs(2),
        output: String::new(),
        cached: false,
        provenance: None,
    }];

    generate_failures_summary(&summary_dir, &checks).expect("failures summary");
    let content = std::fs::read_to_string(summary_dir.join("FAILURES_SUMMARY.md")).expect("read");

    assert!(content.contains("# Failures Summary"));
    assert!(content.contains("No blocking check failures."));
    assert!(content.contains("1 check(s) recorded."));
}

#[test]
fn gate_result_json_carries_the_scanned_tree_provenance() {
    // The substrate a gate ran on must survive into the artifact: without
    // target_sha + tree_state a reader cannot tell whether the gate saw the
    // reviewed commit or an uncommitted local tree. Absent fields stay absent
    // (older packs and non-git substrates must not grow null keys).
    let tmp = tempfile::tempdir().expect("tempdir");
    let base = CheckResult {
        name: "Ruff".to_string(),
        status: CheckStatus::Passed,
        duration: Duration::from_secs(1),
        output: String::new(),
        cached: false,
        provenance: Some(CheckProvenance {
            command: "ruff check .".to_string(),
            tool_version: None,
            cwd: "[external]/tmp/snapshot".to_string(),
            exit_code: Some(0),
            started_at: "2026-01-01T00:00:00Z".to_string(),
            finished_at: "2026-01-01T00:00:01Z".to_string(),
            hard_fail_signatures: vec![],
            cache_key: None,
            target_sha: Some("a".repeat(40)),
            tree_state: Some(crate::checks::TreeState::Snapshot),
        }),
    };

    generate_gate_results(tmp.path(), std::slice::from_ref(&base)).expect("gate results");
    let value: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(tmp.path().join("ruff.result.json")).expect("read json"),
    )
    .expect("parse json");
    assert_eq!(value["target_sha"].as_str(), Some("a".repeat(40).as_str()));
    assert_eq!(value["tree_state"].as_str(), Some("snapshot"));

    let mut unknown = base;
    if let Some(prov) = unknown.provenance.as_mut() {
        prov.target_sha = None;
        prov.tree_state = None;
    }
    generate_gate_results(tmp.path(), &[unknown]).expect("gate results");
    let value: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(tmp.path().join("ruff.result.json")).expect("read json"),
    )
    .expect("parse json");
    assert!(value.get("target_sha").is_none());
    assert!(value.get("tree_state").is_none());
}

#[test]
fn gate_result_json_has_failed_tests_for_cargo_test() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let checks = vec![CheckResult {
        name: "Cargo test".to_string(),
        status: CheckStatus::Failed,
        duration: Duration::from_secs(1),
        output: "\
thread 'tests::bad' panicked at src/lib.rs:42:5:
assertion failed
test tests::bad ... FAILED

failures:

---- tests::bad stdout ----

failures:
    tests::bad

test result: FAILED. 0 passed; 1 failed
"
        .to_string(),
        cached: false,
        provenance: Some(CheckProvenance {
            command: "cargo test --lib --tests --no-fail-fast".to_string(),
            tool_version: None,
            cwd: "/tmp/repo".to_string(),
            exit_code: Some(101),
            started_at: "2026-01-01T00:00:00Z".to_string(),
            finished_at: "2026-01-01T00:00:01Z".to_string(),
            hard_fail_signatures: vec!["SIGABRT".to_string()],
            cache_key: None,
            target_sha: None,
            tree_state: None,
        }),
    }];

    generate_gate_results(tmp.path(), &checks).expect("gate results");

    let raw =
        std::fs::read_to_string(tmp.path().join("cargo_test.result.json")).expect("read json");
    let value: serde_json::Value = serde_json::from_str(&raw).expect("parse json");

    assert_eq!(
        value["failed_tests"].as_array(),
        Some(&vec![serde_json::Value::String("tests::bad".to_string())])
    );
    assert_eq!(value["failed_test_count"].as_u64(), Some(1));
}

#[test]
fn failures_summary_lists_failed_cargo_tests_and_prefers_test_root_cause() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let summary_dir = tmp.path().join("00_summary");
    std::fs::create_dir_all(&summary_dir).expect("summary dir");

    let checks = vec![CheckResult {
        name: "Cargo test".to_string(),
        status: CheckStatus::Failed,
        duration: Duration::from_secs(1),
        output: "\
thread 'tests::bad' panicked at src/lib.rs:42:5:
assertion failed
test tests::bad ... FAILED

failures:

---- tests::bad stdout ----

failures:
    tests::bad

test result: FAILED. 0 passed; 1 failed
"
        .to_string(),
        cached: false,
        provenance: Some(CheckProvenance {
            command: "cargo test --lib --tests --no-fail-fast".to_string(),
            tool_version: None,
            cwd: "/tmp/repo".to_string(),
            exit_code: Some(101),
            started_at: "2026-01-01T00:00:00Z".to_string(),
            finished_at: "2026-01-01T00:00:01Z".to_string(),
            hard_fail_signatures: vec!["SIGABRT".to_string()],
            cache_key: None,
            target_sha: None,
            tree_state: None,
        }),
    }];

    generate_failures_summary(&summary_dir, &checks).expect("failures summary");
    let content = std::fs::read_to_string(summary_dir.join("FAILURES_SUMMARY.md")).expect("read");

    assert!(content.contains("1 test failed: tests::bad"));
    assert!(content.contains("### Failed Tests"));
    assert!(content.contains("`tests::bad` (src/lib.rs:42:5)"));
    assert!(!content.contains("Hard failure detected: SIGABRT"));
}

#[test]
fn root_cause_for_unlaunchable_mypy_reports_missing_tool() {
    // PV-02: when mypy could not be launched (uv spawn-fail), the root cause
    // must say "not installed", not parrot a phantom type error.
    let check = CheckResult {
        name: "Mypy".to_string(),
        status: CheckStatus::Failed,
        duration: Duration::from_secs(0),
        output:
            "error: Failed to spawn: `mypy`\n  Caused by: No such file or directory (os error 2)\n"
                .to_string(),
        cached: false,
        provenance: None,
    };

    let rc = extract_root_cause(&check).expect("root cause for failed mypy");
    assert_eq!(rc.cause, "mypy not installed / could not be launched");
    assert!(
        rc.hint.to_lowercase().contains("install mypy"),
        "hint should suggest installing mypy: {}",
        rc.hint
    );
}

#[test]
fn root_cause_for_real_mypy_type_errors_reports_found_summary() {
    // PV-02 regression guard: a genuine type-error run must keep the existing
    // "Found ..." cause, not be downgraded to a missing-tool message.
    let check = CheckResult {
        name: "Mypy".to_string(),
        status: CheckStatus::Failed,
        duration: Duration::from_secs(0),
        output: "src/x.py:3: error: Incompatible return value type\nFound 1 error in 1 file\n"
            .to_string(),
        cached: false,
        provenance: None,
    };

    let rc = extract_root_cause(&check).expect("root cause for failed mypy");
    assert!(
        rc.cause.starts_with("Found "),
        "cause should start with the mypy summary: {}",
        rc.cause
    );
}

#[test]
fn inline_findings_emits_one_sarif_result_per_cargo_audit_advisory() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let cargo_root = tmp.path().join("workspace-crate");
    std::fs::create_dir_all(&cargo_root).expect("cargo root dir");
    let checks = vec![CheckResult {
        name: "Cargo audit".to_string(),
        status: CheckStatus::Failed,
        duration: Duration::from_secs(1),
        output: sample_cargo_audit_output(),
        cached: false,
        provenance: Some(CheckProvenance {
            command: "cargo audit --json".to_string(),
            tool_version: None,
            cwd: cargo_root.display().to_string(),
            exit_code: Some(1),
            started_at: "2026-01-01T00:00:00Z".to_string(),
            finished_at: "2026-01-01T00:00:01Z".to_string(),
            hard_fail_signatures: vec![],
            cache_key: None,
            target_sha: None,
            tree_state: None,
        }),
    }];

    let summary =
        generate_inline_findings(tmp.path(), &checks, &[], None, None).expect("inline findings");
    assert_eq!(summary.status, "failed");
    assert_eq!(summary.findings_count, 2);
    assert_eq!(summary.dashboard_findings.len(), 3);
    assert!(summary.dashboard_findings.iter().any(|finding| {
        finding.check_id == "cargo_audit_baseline"
            && finding.message.contains("new=0")
            && finding.message.contains("pre-existing=2")
            && finding.message.contains("resolved=0")
            && finding.message.contains("unknown-baseline=0")
    }));

    let sarif_raw =
        std::fs::read_to_string(tmp.path().join("INLINE_FINDINGS.sarif")).expect("read");
    let sarif: serde_json::Value = serde_json::from_str(&sarif_raw).expect("parse sarif");
    let runs = sarif["runs"].as_array().expect("runs array");
    assert_eq!(runs.len(), 1);
    let run = &runs[0];
    assert_eq!(
        run["tool"]["driver"]["name"].as_str(),
        Some("prview-inline")
    );
    let results = run["results"].as_array().expect("results array");
    assert_eq!(results.len(), 2);
    assert_eq!(results[0]["ruleId"].as_str(), Some("RUSTSEC-2024-0001"));
    assert_eq!(results[0]["level"].as_str(), Some("error"));
    assert_eq!(
        results[0]["locations"][0]["physicalLocation"]["artifactLocation"]["uri"].as_str(),
        Some(cargo_root.join("Cargo.lock").to_string_lossy().as_ref())
    );
    assert_eq!(
        results[1]["locations"][0]["physicalLocation"]["region"]["startLine"].as_u64(),
        Some(1)
    );
    assert_eq!(results[1]["level"].as_str(), Some("warning"));
    assert!(
        results[0]["message"]["text"]
            .as_str()
            .is_some_and(|text| text.contains("example-crate@0.3.1"))
    );
    // Verify fingerprints are present.
    assert!(
        results[0]["partialFingerprints"]["primaryLocationLineHash"]
            .as_str()
            .is_some(),
        "cargo audit findings should have fingerprints"
    );

    let rules = run["tool"]["driver"]["rules"]
        .as_array()
        .expect("rules array");
    assert_eq!(rules.len(), 2);
    assert_eq!(rules[0]["id"].as_str(), Some("RUSTSEC-2024-0001"));
}

#[test]
fn inline_findings_splits_semgrep_introduced_vs_preexisting() {
    // Two semgrep findings: one in a file touched by the PR (introduced),
    // one in an untouched file (preexisting). The SARIF run must carry an
    // explicit introduced_count / preexisting_count split, and each result
    // must be classified (TOOLING-08).
    let tmp = tempfile::tempdir().expect("tempdir");
    let semgrep_output = "\
┌──────────────────┐
│ 2 Code Findings  │
└──────────────────┘

    src/changed.rs
    ❯❱ rust.lang.security.audit.example.example
          A finding in a changed file.
          Details: https://sg.run/aaaa

          10┆ let x = touched();

    src/untouched.rs
    ❯❱ rust.lang.security.audit.example.example
          A finding in an untouched file.
          Details: https://sg.run/bbbb

          20┆ let y = inherited();
"
    .to_string();
    let checks = vec![CheckResult {
        name: "Semgrep scan".to_string(),
        status: CheckStatus::Warnings,
        duration: Duration::from_secs(1),
        output: semgrep_output,
        cached: false,
        provenance: None,
    }];
    let diffs = vec![Diff {
        target: "feature".to_string(),
        base: "main".to_string(),
        target_commit_id: "abc123".to_string(),
        base_commit_id: "def456".to_string(),
        files: vec![FileChange {
            path: "src/changed.rs".to_string(),
            status: FileStatus::Modified,
            additions: 1,
            deletions: 0,
        }],
        stats: DiffStats {
            files_changed: 1,
            additions: 1,
            deletions: 0,
            copied: 0,
        },
        commits: vec![],
    }];

    generate_inline_findings(tmp.path(), &checks, &diffs, None, None).expect("inline findings");

    let sarif_raw =
        std::fs::read_to_string(tmp.path().join("INLINE_FINDINGS.sarif")).expect("read");
    let sarif: serde_json::Value = serde_json::from_str(&sarif_raw).expect("parse sarif");
    let runs = sarif["runs"].as_array().expect("runs array");
    assert_eq!(runs.len(), 1, "inline SARIF is aggregated into one run");

    let run = &runs[0];
    assert_eq!(
        run["tool"]["driver"]["name"].as_str(),
        Some("prview-inline")
    );

    let rules = run["tool"]["driver"]["rules"]
        .as_array()
        .expect("rules array");
    let semgrep_summary = rules
        .iter()
        .find(|r| r["id"].as_str() == Some("prview.summary.semgrep"))
        .expect("semgrep summary rule");
    let props = &semgrep_summary["properties"];
    assert_eq!(
        props["introduced_count"].as_u64(),
        Some(1),
        "one finding is in the diff"
    );
    assert_eq!(
        props["preexisting_count"].as_u64(),
        Some(1),
        "one finding is inherited"
    );

    let results = run["results"].as_array().expect("results array");
    let introduced = results
        .iter()
        .find(|r| r["properties"]["classification"].as_str() == Some("introduced"))
        .expect("an introduced result");
    assert_eq!(
        introduced["locations"][0]["physicalLocation"]["artifactLocation"]["uri"].as_str(),
        Some("src/changed.rs")
    );
    assert!(
        results
            .iter()
            .any(|r| r["properties"]["classification"].as_str() == Some("preexisting")),
        "a preexisting result must be present"
    );
}

#[test]
fn inline_findings_skips_geiger_metric_header_lines() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let checks = vec![CheckResult {
        name: "Cargo geiger".to_string(),
        status: CheckStatus::Warnings,
        duration: std::time::Duration::from_secs(1),
        output: "\
Metric output format: x/y=z%
3/15 unsafe functions in 2 crates
crate foo uses unsafe via transitive dependency
"
        .to_string(),
        cached: false,
        provenance: None,
    }];

    let summary =
        generate_inline_findings(tmp.path(), &checks, &[], None, None).expect("inline findings");
    assert_eq!(summary.findings_count, 1);
    assert_eq!(
        summary.dashboard_findings[0].message,
        "crate foo uses unsafe via transitive dependency"
    );

    let sarif_raw =
        std::fs::read_to_string(tmp.path().join("INLINE_FINDINGS.sarif")).expect("read");
    let sarif: serde_json::Value = serde_json::from_str(&sarif_raw).expect("parse sarif");
    let runs = sarif["runs"].as_array().expect("runs array");
    let run = runs
        .iter()
        .find(|r| r["tool"]["driver"]["name"].as_str() == Some("prview-inline"))
        .expect("should have inline fallback run");
    let message = run["results"][0]["message"]["text"]
        .as_str()
        .expect("sarif message");
    assert!(message.contains("crate foo uses unsafe via transitive dependency"));
    assert!(!message.contains("Metric output format"));
}

#[test]
fn pr_review_summarizes_cargo_audit_without_dumping_json() {
    let diffs = vec![Diff {
        target: "feature".to_string(),
        base: "main".to_string(),
        target_commit_id: "abc123".to_string(),
        base_commit_id: "def456".to_string(),
        files: vec![FileChange {
            path: "Cargo.lock".to_string(),
            status: FileStatus::Modified,
            additions: 4,
            deletions: 2,
        }],
        stats: DiffStats {
            files_changed: 1,
            additions: 4,
            deletions: 2,
            copied: 0,
        },
        commits: vec![],
    }];
    let checks = vec![CheckResult {
        name: "Cargo audit".to_string(),
        status: CheckStatus::Failed,
        duration: Duration::from_secs(1),
        output: sample_cargo_audit_output(),
        cached: false,
        provenance: None,
    }];

    let tmp = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(tmp.path().join("30_context")).expect("context dir");
    std::fs::write(
        tmp.path().join("30_context/cargo-tree.txt"),
        sample_cargo_tree_output(),
    )
    .expect("cargo tree");
    let config = create_test_config(PolicyConfig::default());

    generate_pr_review(
        tmp.path(),
        &config,
        &diffs,
        &checks,
        &[],
        &CoverageDelta {
            total_source: 0,
            covered_count: 0,
            pct: None,
            uncovered: vec![],
            covered: vec![],
            non_code_count: 0,
            ghost_tests: vec![],
        },
        None,
    )
    .expect("pr review");

    let content = std::fs::read_to_string(tmp.path().join("PR_REVIEW.md")).expect("read");
    assert!(content.contains("## Review Findings"));
    assert!(content.contains("dependency security issue in `Cargo.lock`"));
    assert!(
        content.contains("Advisory summary: 2 security advisories affecting 2 locked dependencies")
    );
    assert!(content.contains("RUSTSEC-2024-0001, RUSTSEC-2024-0002"));
    assert!(content.contains("Dependency paths:"));
    assert!(content.contains("Full advisory list: `00_summary/FAILURES_SUMMARY.md`"));
    assert!(content.contains("Per-advisory SARIF: `30_context/INLINE_FINDINGS.sarif`"));
    assert!(content.contains("Detailed advisory breakdown is intentionally deduplicated here"));
    assert!(content.contains("rmcp-memex v0.1.0 (/workspace/rmcp-memex) -> example-crate v0.3.1"));
    assert!(!content.contains("Key advisories:"));
    assert!(!content.contains("Unsound transmute in example crate"));
    assert!(!content.contains("\"vulnerabilities\""));
    assert!(!content.contains("\"list\""));
}

#[test]
fn cargo_audit_informational_summary_extracts_unmaintained_warnings() {
    let output = r#"{"vulnerabilities":{"found":false,"count":0,"list":[]},"warnings":{"unmaintained":[{"kind":"unmaintained","package":{"name":"paste","version":"1.0.15"},"advisory":{"id":"RUSTSEC-2024-0436"}}]}}"#;
    let summary = cargo_audit_informational_summary(output);
    assert!(summary.is_some(), "should detect informational warnings");
    let s = summary.unwrap();
    assert!(s.contains("paste"), "should mention the package");
    assert!(s.contains("unmaintained"), "should mention the kind");
    assert!(
        s.contains("1 informational advisory"),
        "should count correctly"
    );
}

#[test]
fn cargo_audit_informational_summary_returns_none_for_clean_output() {
    let output = r#"{"vulnerabilities":{"found":false,"count":0,"list":[]},"warnings":{}}"#;
    assert!(
        cargo_audit_informational_summary(output).is_none(),
        "no warnings → None"
    );
}

#[test]
fn pr_review_surfaces_cargo_audit_informational_warnings_when_check_passes() {
    let audit_output = r#"{"vulnerabilities":{"found":false,"count":0,"list":[]},"warnings":{"unmaintained":[{"kind":"unmaintained","package":{"name":"paste","version":"1.0.15"},"advisory":{"id":"RUSTSEC-2024-0436"}}]}}"#;
    let diffs = vec![Diff {
        target: "feature".to_string(),
        base: "main".to_string(),
        target_commit_id: "abc123".to_string(),
        base_commit_id: "def456".to_string(),
        files: vec![],
        stats: DiffStats {
            files_changed: 0,
            additions: 0,
            deletions: 0,
            copied: 0,
        },
        commits: vec![],
    }];
    let checks = vec![CheckResult {
        name: "Cargo audit".to_string(),
        status: CheckStatus::Passed,
        duration: Duration::from_secs(1),
        output: audit_output.to_string(),
        cached: false,
        provenance: None,
    }];

    let tmp = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(tmp.path().join("30_context")).expect("context dir");
    let config = create_test_config(PolicyConfig::default());

    generate_pr_review(
        tmp.path(),
        &config,
        &diffs,
        &checks,
        &[],
        &CoverageDelta {
            total_source: 0,
            covered_count: 0,
            pct: None,
            uncovered: vec![],
            covered: vec![],
            non_code_count: 0,
            ghost_tests: vec![],
        },
        None,
    )
    .expect("pr review");

    let content = std::fs::read_to_string(tmp.path().join("PR_REVIEW.md")).expect("read");
    assert!(
        content.contains("Cargo audit note:"),
        "should surface informational note"
    );
    assert!(content.contains("paste"), "should mention the package");
    assert!(
        content.contains("unmaintained"),
        "should mention the advisory kind"
    );
    // Should NOT show Review Findings section (check passed)
    assert!(
        !content.contains("## Review Findings"),
        "no review findings for passing check"
    );
}

#[test]
fn is_test_file_detects_common_patterns() {
    // Rust
    assert!(is_test_file("src/foo_test.rs"));
    assert!(is_test_file("tests/integration.rs"));
    assert!(is_test_file("src/tests/unit.rs"));

    // Python
    assert!(is_test_file("tests/test_main.py"));
    assert!(is_test_file("src/test_utils.py"));

    // JS/TS
    assert!(is_test_file("src/App.test.tsx"));
    assert!(is_test_file("lib/utils.spec.ts"));
    assert!(is_test_file("src/__tests__/foo.ts"));

    // Non-test files
    assert!(!is_test_file("src/main.rs"));
    assert!(!is_test_file("src/lib.rs"));
    assert!(!is_test_file("package.json"));
}

#[test]
fn compute_diff_stat_parses_patch() {
    let patch = "\
diff --git a/src/main.rs b/src/main.rs
--- a/src/main.rs
+++ b/src/main.rs
@@ -1,3 +1,5 @@
+use foo;
+use bar;
 fn main() {
-    old_line();
+    new_line();
 }
diff --git a/src/lib.rs b/src/lib.rs
--- a/src/lib.rs
+++ b/src/lib.rs
@@ -1,2 +1,3 @@
+pub mod new_mod;
 pub fn lib_fn() {}
";
    let stats = compute_diff_stat(patch);
    assert_eq!(stats.len(), 2);
    assert_eq!(stats[0].0, "src/main.rs");
    assert_eq!(stats[0].1, 3); // +use foo, +use bar, +new_line
    assert_eq!(stats[0].2, 1); // -old_line
    assert_eq!(stats[1].0, "src/lib.rs");
    assert_eq!(stats[1].1, 1);
    assert_eq!(stats[1].2, 0);
}

#[test]
fn extract_file_line_handles_all_documented_formats() {
    // Python/mypy: path/file.py:27: error
    assert_eq!(
        extract_file_line_from_output("src/lib.py:27: error: something"),
        Some(("src/lib.py".to_string(), 27))
    );
    // Rust compiler: --> path/file.rs:42:5
    assert_eq!(
        extract_file_line_from_output("  --> src/lib.rs:42:5"),
        Some(("src/lib.rs".to_string(), 42))
    );
    // Rust compiler without column: --> path/file.rs:42
    assert_eq!(
        extract_file_line_from_output("  --> src/lib.rs:42"),
        Some(("src/lib.rs".to_string(), 42))
    );
    // TypeScript tsc: path/file.ts(27,5): error
    assert_eq!(
        extract_file_line_from_output("src/foo.ts(27,5): error TS2345"),
        Some(("src/foo.ts".to_string(), 27))
    );
    // Single file at root via --> branch
    assert_eq!(
        extract_file_line_from_output("  --> lib.rs:10:1"),
        Some(("lib.rs".to_string(), 10))
    );
    // No file:line pattern
    assert_eq!(extract_file_line_from_output("All checks passed"), None);
    // Bare word with colon (no path separator) should NOT match
    assert_eq!(
        extract_file_line_from_output("error: something went wrong"),
        None
    );
    // Regression: a minified-JS fragment whose tail looks like `path:line`
    // (Semgrep over dagre.min.js) must NOT be scraped as a file location.
    assert_eq!(
        extract_file_line_from_output(
            "sted=nested[key]}return object}module.exports=baseSet},{\"./_assignValue\":75"
        ),
        None
    );
}

#[test]
fn is_packaging_junk_matches_os_cruft() {
    assert!(is_packaging_junk(Path::new("run/.DS_Store")));
    assert!(is_packaging_junk(Path::new("Thumbs.db")));
    assert!(!is_packaging_junk(Path::new("report.json")));
    assert!(!is_packaging_junk(Path::new("10_diff/full.patch")));
}

#[test]
fn is_pathish_candidate_rejects_code_fragments() {
    assert!(is_pathish_candidate("src/lib.rs"));
    assert!(is_pathish_candidate("editors/vscode/src/client.ts"));
    // No path separator.
    assert!(!is_pathish_candidate("error"));
    // Embedded code punctuation.
    assert!(!is_pathish_candidate(
        "sted=nested[key]}return object}module.exports=baseSet},{\"./_assignValue\""
    ));
    // Bracket notation alone (array indexing) is a code fragment, not a path
    // (PR #10 review by @gemini-code-assist).
    assert!(!is_pathish_candidate("nested[key]/module"));
    // Whitespace.
    assert!(!is_pathish_candidate("a/b c"));
}

#[test]
fn ai_index_coverage_signal_says_not_measured_for_zero_of_zero() {
    use crate::artifacts::signal::COVERAGE_NOT_MEASURED;
    use crate::git::DiffStats;

    let tmp = tempfile::tempdir().expect("tempdir");
    let out = tmp.path();
    let config = create_test_config(PolicyConfig::default());
    let diffs = vec![Diff {
        base: "main".to_string(),
        target: "feat/x".to_string(),
        base_commit_id: "aaa".to_string(),
        target_commit_id: "bbb".to_string(),
        files: vec![],
        stats: DiffStats {
            files_changed: 0,
            additions: 0,
            deletions: 0,
            copied: 0,
        },
        commits: vec![],
    }];

    let empty = CoverageDelta {
        total_source: 0,
        covered_count: 0,
        pct: None,
        uncovered: vec![],
        covered: vec![],
        non_code_count: 0,
        ghost_tests: vec![],
    };
    generate_ai_index(out, &config, &diffs, &[], &empty).expect("ai index");
    let index = std::fs::read_to_string(out.join("AI_INDEX.md")).expect("AI_INDEX.md");
    assert!(
        index.contains(&format!(
            "Coverage signal: 0/0 changed code files ({COVERAGE_NOT_MEASURED})"
        )),
        "0/0 must be labelled not-measured, got:\n{index}"
    );
    assert!(
        !index.contains("(100%)"),
        "0/0 must never render as 100%, got:\n{index}"
    );

    // 0/N stays a real 0% measurement.
    let measured = CoverageDelta {
        total_source: 4,
        covered_count: 0,
        pct: Some(0),
        uncovered: vec![],
        covered: vec![],
        non_code_count: 0,
        ghost_tests: vec![],
    };
    generate_ai_index(out, &config, &diffs, &[], &measured).expect("ai index");
    let index = std::fs::read_to_string(out.join("AI_INDEX.md")).expect("AI_INDEX.md");
    assert!(
        index.contains("Coverage signal: 0/4 changed code files (0%)"),
        "0/N must render as 0%, got:\n{index}"
    );
    assert!(!index.contains(COVERAGE_NOT_MEASURED));
}

#[test]
fn generate_ai_index_writes_reading_order_and_verdict() {
    use crate::git::DiffStats;

    let tmp = tempfile::tempdir().expect("tempdir");
    let out = tmp.path();
    std::fs::create_dir_all(out.join("00_summary")).expect("summary dir");
    std::fs::write(
            out.join("00_summary/MERGE_GATE.json"),
            r#"{"target":"feat/x","decision":{"verdict":"CONDITIONAL","decision_reason":"2 checks failed","allow_merge":true}}"#,
        )
        .expect("gate");
    std::fs::write(out.join("report.json"), "{}").expect("report");
    std::fs::write(out.join("review.html"), "<html></html>").expect("review html");
    std::fs::write(out.join("dashboard.html"), "<html></html>").expect("dashboard");

    let config = create_test_config(PolicyConfig::default());
    let diffs = vec![Diff {
        base: "main".to_string(),
        target: "feat/x".to_string(),
        base_commit_id: "aaa".to_string(),
        target_commit_id: "bbb".to_string(),
        files: vec![],
        stats: DiffStats {
            files_changed: 0,
            additions: 0,
            deletions: 0,
            copied: 0,
        },
        commits: vec![],
    }];
    let coverage = CoverageDelta {
        total_source: 0,
        covered_count: 0,
        pct: None,
        uncovered: vec![],
        covered: vec![],
        non_code_count: 0,
        ghost_tests: vec![],
    };

    generate_ai_index(out, &config, &diffs, &[], &coverage).expect("ai index");

    let index = std::fs::read_to_string(out.join("AI_INDEX.md")).expect("AI_INDEX.md exists");
    assert!(index.contains("# AI Review Index"));
    assert!(index.contains("## Recommended reading order"));
    assert!(index.contains("CONDITIONAL"));
    assert!(index.contains("feat/x"));
    // Lists artifacts that exist.
    assert!(index.contains("00_summary/MERGE_GATE.json"));
    assert!(index.contains("report.json"));
    assert!(index.contains("review.html"));
    // The HTML dashboard is a key human artifact and is listed when present
    // (PR #10 review by @gemini-code-assist).
    assert!(index.contains("dashboard.html"));
    // Never points at an artifact that was not produced.
    assert!(!index.contains("30_context/DEPS_DELTA.json"));
}

#[test]
fn standard_review_html_is_generated_from_pack_markdown() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let out = tmp.path();
    fs::create_dir_all(out.join("00_summary")).unwrap();
    fs::write(
        out.join("00_summary/MERGE_GATE.json"),
        r#"{"decision":{"verdict":"HOLD","decision_reason":"review required"}}"#,
    )
    .unwrap();
    fs::write(
        out.join("00_summary/MERGE_GATE.md"),
        "# Merge Gate\n\nHold.\n",
    )
    .unwrap();
    fs::write(
        out.join("00_summary/FAILURES_SUMMARY.md"),
        "# Failures\n\n- cargo test failed\n",
    )
    .unwrap();
    fs::write(
        out.join("REVIEW_SUMMARY.md"),
        "# PR Review Summary\n\n## Gate Decision\n\n**Verdict:** `HOLD`\n",
    )
    .unwrap();
    fs::write(out.join("AI_INDEX.md"), "# AI Review Index\n").unwrap();
    fs::write(out.join("dashboard.html"), "<html></html>").unwrap();

    generate_standard_review_html(out).unwrap();

    let html = fs::read_to_string(out.join("review.html")).unwrap();
    assert!(html.contains("prview standard review"));
    assert!(html.contains("HOLD"));
    assert!(html.contains("review required"));
    assert!(html.contains("dashboard.html"));
    assert!(html.contains("cargo test failed"));
}

#[test]
fn changed_tests_filters_correctly() {
    use crate::git::{DiffStats, FileChange, FileStatus};

    let diffs = vec![Diff {
        base: "main".to_string(),
        target: "feature".to_string(),
        base_commit_id: "aaa".to_string(),
        target_commit_id: "bbb".to_string(),
        files: vec![
            FileChange {
                path: "src/lib.rs".to_string(),
                status: FileStatus::Modified,
                additions: 10,
                deletions: 5,
            },
            FileChange {
                path: "tests/integration.rs".to_string(),
                status: FileStatus::Added,
                additions: 80,
                deletions: 0,
            },
            FileChange {
                path: "src/App.test.tsx".to_string(),
                status: FileStatus::Modified,
                additions: 3,
                deletions: 2,
            },
        ],
        stats: DiffStats {
            files_changed: 3,
            additions: 63,
            deletions: 7,
            copied: 0,
        },
        commits: vec![],
    }];

    let tmp = tempfile::tempdir().expect("tempdir");
    generate_changed_tests(&diffs, tmp.path()).expect("changed tests");
    let content = std::fs::read_to_string(tmp.path().join("changed-tests.txt")).expect("read");
    assert!(content.contains("tests/integration.rs"));
    assert!(content.contains("src/App.test.tsx"));
    assert!(!content.contains("src/lib.rs"));
}

#[test]
fn pr_review_counts_code_test_and_non_code_separately() {
    use crate::git::{DiffStats, FileChange, FileStatus};
    use crate::heuristics::{DeadParrot, HeuristicsResult, LoctreeAnalysis, TwinsAnalysis};

    let diffs = vec![Diff {
        base: "main".to_string(),
        target: "feature".to_string(),
        base_commit_id: "aaa".to_string(),
        target_commit_id: "bbb".to_string(),
        files: vec![
            FileChange {
                path: "src/lib.rs".to_string(),
                status: FileStatus::Modified,
                additions: 10,
                deletions: 5,
            },
            FileChange {
                path: "tests/integration.rs".to_string(),
                status: FileStatus::Added,
                additions: 80,
                deletions: 0,
            },
            FileChange {
                path: "README.md".to_string(),
                status: FileStatus::Modified,
                additions: 6,
                deletions: 1,
            },
            FileChange {
                path: "Makefile".to_string(),
                status: FileStatus::Added,
                additions: 8,
                deletions: 0,
            },
        ],
        stats: DiffStats {
            files_changed: 4,
            additions: 104,
            deletions: 6,
            copied: 0,
        },
        commits: vec![],
    }];

    let tmp = tempfile::tempdir().expect("tempdir");
    let config = create_test_config(PolicyConfig::default());
    let heuristics = HeuristicsResult {
        loctree: Some(LoctreeAnalysis {
            twins: TwinsAnalysis {
                dead_parrots: vec![DeadParrot {
                    file: "src/lib.rs".to_string(),
                    symbol: "unused_helper".to_string(),
                    kind: "function".to_string(),
                    line: 7,
                }],
                ..Default::default()
            },
            available: true,
            ..Default::default()
        }),
        ..Default::default()
    };

    generate_pr_review(
        tmp.path(),
        &config,
        &diffs,
        &[],
        &[],
        &CoverageDelta {
            total_source: 1,
            covered_count: 1,
            pct: Some(100),
            uncovered: vec![],
            covered: vec![],
            non_code_count: 0,
            ghost_tests: vec![],
        },
        Some(&heuristics),
    )
    .expect("pr review");
    let content = std::fs::read_to_string(tmp.path().join("PR_REVIEW.md")).expect("read");

    assert!(content.contains("**Code:** 1"));
    assert!(content.contains("**Tests:** 1"));
    assert!(content.contains("**Non-code:** 2"));
    assert!(content.contains("| Non-code files | 2 |"));
    assert!(content.contains("## Structural Signals"));
    assert!(
        content
            .contains("Hotspots: 1 file(s) crossed the hotspot threshold (`>=80` changed lines).")
    );
    assert!(content.contains("Top hotspots: `tests/integration.rs` (80)"));
    assert!(content.contains("Loctree twins: 0 exact twin pair(s) and 1 unused symbol(s)."));
}

#[test]
fn pr_review_uses_coverage_delta_for_warning_summary() {
    let diffs = vec![Diff {
        target: "feature".to_string(),
        base: "main".to_string(),
        target_commit_id: "abc123".to_string(),
        base_commit_id: "def456".to_string(),
        files: vec![FileChange {
            path: "src/lib.rs".to_string(),
            status: FileStatus::Modified,
            additions: 10,
            deletions: 2,
        }],
        stats: DiffStats {
            files_changed: 1,
            additions: 10,
            deletions: 2,
            copied: 0,
        },
        commits: vec![],
    }];

    let tmp = tempfile::tempdir().expect("tempdir");
    let config = create_test_config(PolicyConfig::default());

    generate_pr_review(
        tmp.path(),
        &config,
        &diffs,
        &[],
        &[],
        &CoverageDelta {
            total_source: 4,
            covered_count: 1,
            pct: Some(25),
            uncovered: vec![crate::artifacts::signal::CoverageFile {
                status: 'M',
                path: "src/lib.rs".to_string(),
            }],
            covered: vec![],
            non_code_count: 0,
            ghost_tests: vec![],
        },
        None,
    )
    .expect("pr review");
    let content = std::fs::read_to_string(tmp.path().join("PR_REVIEW.md")).expect("read");

    assert!(content.contains("Coverage review signal: 25% heuristic coverage (1/4)"));
    assert!(
            content.contains(
                "Rust caveat: coverage heuristic may miss inline `#[cfg(test)]` modules inside changed `.rs` files."
            )
        );
}

#[test]
fn pr_review_surfaces_quick_wins_for_rust_signal_gaps_and_cargo_audit() {
    let diffs = vec![Diff {
        target: "feature".to_string(),
        base: "main".to_string(),
        target_commit_id: "abc123".to_string(),
        base_commit_id: "def456".to_string(),
        files: vec![FileChange {
            path: "Cargo.lock".to_string(),
            status: FileStatus::Modified,
            additions: 5,
            deletions: 2,
        }],
        stats: DiffStats {
            files_changed: 1,
            additions: 5,
            deletions: 2,
            copied: 0,
        },
        commits: vec![],
    }];
    let checks = vec![CheckResult {
        name: "Cargo audit".to_string(),
        status: CheckStatus::Failed,
        duration: Duration::from_secs(1),
        output: sample_cargo_audit_output(),
        cached: false,
        provenance: None,
    }];

    let tmp = tempfile::tempdir().expect("tempdir");
    let mut config = create_test_config(PolicyConfig::default());
    config.run_tests = false;
    config.run_lint = false;
    let skipped = [
        crate::checks::SkippedCheck {
            id: "cargo_test".to_string(),
            name: "Cargo Test".to_string(),
            reason: "tests disabled".to_string(),
        },
        crate::checks::SkippedCheck {
            id: "clippy".to_string(),
            name: "Clippy".to_string(),
            reason: "lint disabled".to_string(),
        },
    ];
    generate_pr_review(
        tmp.path(),
        &config,
        &diffs,
        &checks,
        &skipped,
        &CoverageDelta {
            total_source: 0,
            covered_count: 0,
            pct: None,
            uncovered: vec![],
            covered: vec![],
            non_code_count: 0,
            ghost_tests: vec![],
        },
        None,
    )
    .expect("pr review");
    let content = std::fs::read_to_string(tmp.path().join("PR_REVIEW.md")).expect("read");

    assert!(content.contains("## Quick Wins"));
    assert!(content.contains("Enable `cargo test` for this run"));
    assert!(content.contains("Enable `cargo clippy` for this run"));
    assert!(content.contains("Bump `example-crate` to `>=0.3.2`"));
    assert!(content.contains(
        "Not executed by this PrView run. Reason: tests disabled. External CI status not included."
    ));
}

#[test]
fn plan_context_artifacts_marks_tauri_info_deferred_for_fast_remote_only() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let mut config = create_test_config(PolicyConfig::default());
    config.repo_root = tmp.path().to_path_buf();
    config.remote_only = true;
    config.profile.has_cargo = true;
    config.profile.cargo_root = Some(tmp.path().join("src-tauri"));
    std::fs::create_dir_all(tmp.path().join("src-tauri/capabilities")).expect("create tauri dir");
    // Create src-tauri/Cargo.toml so is_tauri_project() detects this as a Tauri project.
    std::fs::write(
        tmp.path().join("src-tauri/Cargo.toml"),
        "[package]\nname = \"app\"\n",
    )
    .expect("create tauri Cargo.toml");

    let diffs = vec![Diff {
        base: "main".to_string(),
        target: "feature/desktop".to_string(),
        base_commit_id: "def5678def5678def5678def5678def5678de".to_string(),
        target_commit_id: "abc1234abc1234abc1234abc1234abc1234ab".to_string(),
        files: vec![FileChange {
            path: "src-tauri/tauri.conf.json".to_string(),
            status: FileStatus::Modified,
            additions: 2,
            deletions: 1,
        }],
        stats: DiffStats {
            files_changed: 1,
            additions: 2,
            deletions: 1,
            copied: 0,
        },
        commits: vec![],
    }];

    // No shared snapshot in this fixture, so the reviewed tree is the repo root.
    let decisions = plan_context_artifacts(
        &config,
        &config.repo_root.clone(),
        &diffs,
        &[],
        &crate::ledger::TaskLedger::new(),
    );
    let tauri_info = decisions
        .iter()
        .find(|decision| decision.key == "tauri_info")
        .expect("tauri info decision");

    assert!(!tauri_info.generated);
    assert!(tauri_info.recommended);
    assert!(
        tauri_info
            .reason
            .contains("Tauri config/build files changed")
    );
}

#[test]
fn tauri_context_dir_preserves_local_path_and_rebases_the_snapshot() {
    let repo = tempfile::tempdir().expect("repo");
    let snapshot = tempfile::tempdir().expect("snapshot");
    let local_tauri = repo.path().join("desktop/src-tauri");
    let mut config = create_test_config(PolicyConfig::default());
    config.repo_root = repo.path().to_path_buf();
    config.profile.cargo_root = Some(local_tauri.clone());

    assert_eq!(
        tauri_dir_in_context_tree(&config, repo.path()),
        local_tauri,
        "local reviews keep the previously selected cargo root",
    );
    assert_eq!(
        tauri_dir_in_context_tree(&config, snapshot.path()),
        snapshot.path().join("desktop/src-tauri"),
        "off-HEAD reviews project that relative layout onto the reviewed tree",
    );
}

#[test]
fn static_tauri_commands_follow_the_shared_reviewed_tree() {
    let publication_home = tempfile::tempdir().expect("publication home");
    let _publication_home =
        crate::config::override_test_prview_home(publication_home.path().to_path_buf());
    let repo = tempfile::tempdir().expect("repo");
    run_git_fixture(repo.path(), &["init", "-q", "-b", "main"]);
    std::fs::create_dir_all(repo.path().join("src-tauri/src")).expect("base tauri layout");
    write_commit_fixture(
        repo.path(),
        "src-tauri/Cargo.toml",
        "[package]\nname = \"desktop\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    );
    write_commit_fixture(repo.path(), "src-tauri/src/lib.rs", "mod local;\n");
    let base_sha = write_commit_fixture(
        repo.path(),
        "src-tauri/src/local.rs",
        "#[tauri::command]\npub fn base_command() {}\n",
    );

    run_git_fixture(repo.path(), &["checkout", "-q", "-b", "feature"]);
    std::fs::remove_file(repo.path().join("src-tauri/src/local.rs")).expect("remove base command");
    std::fs::create_dir_all(repo.path().join("src-tauri/src/commands"))
        .expect("target command layout");
    std::fs::write(
        repo.path().join("src-tauri/src/lib.rs"),
        "#[path = \"commands/target.rs\"]\nmod target;\n",
    )
    .expect("target crate root");
    std::fs::write(
        repo.path().join("src-tauri/src/commands/target.rs"),
        "#[tauri::command]\npub fn target_command() {}\n",
    )
    .expect("target command");
    run_git_fixture(repo.path(), &["add", "-A"]);
    run_git_fixture(
        repo.path(),
        &[
            "-c",
            "user.name=prview test",
            "-c",
            "user.email=prview@example.test",
            "commit",
            "-q",
            "-m",
            "target tauri layout",
        ],
    );
    let target_sha = String::from_utf8(
        git_cmd()
            .args(["rev-parse", "HEAD"])
            .current_dir(repo.path())
            .output()
            .expect("target rev-parse")
            .stdout,
    )
    .expect("UTF-8 target sha")
    .trim()
    .to_owned();

    run_git_fixture(repo.path(), &["checkout", "-q", "main"]);
    std::fs::remove_dir_all(repo.path().join("src-tauri")).expect("remove local tauri layout");
    std::fs::create_dir_all(repo.path().join("local-shell/src")).expect("different local layout");
    std::fs::write(
        repo.path().join("local-shell/src/local.rs"),
        "#[tauri::command]\npub fn local_only_command() {}\n",
    )
    .expect("local-only command");

    let snapshot = crate::git::create_worktree_snapshot(repo.path(), &target_sha)
        .expect("shared reviewed snapshot");
    let ledger = crate::ledger::TaskLedger::new();
    ledger.set_shared_snapshot(Some(snapshot));
    let diffs = [Diff {
        base: "main".to_string(),
        target: "feature".to_string(),
        base_commit_id: base_sha.clone(),
        target_commit_id: target_sha.clone(),
        files: vec![
            FileChange {
                path: "src-tauri/src/local.rs".to_string(),
                status: FileStatus::Deleted,
                additions: 0,
                deletions: 2,
            },
            FileChange {
                path: "src-tauri/src/commands/target.rs".to_string(),
                status: FileStatus::Added,
                additions: 2,
                deletions: 0,
            },
            FileChange {
                path: "src-tauri/src/lib.rs".to_string(),
                status: FileStatus::Modified,
                additions: 2,
                deletions: 1,
            },
        ],
        stats: DiffStats {
            files_changed: 3,
            additions: 4,
            deletions: 3,
            copied: 0,
        },
        commits: vec![],
    }];
    let output = tempfile::tempdir().expect("output");
    let pack = output.path().join("pack");

    generate_fixture_pack_with_ledger_and_diffs(
        repo.path(),
        &pack,
        &target_sha,
        &base_sha,
        &crate::governor::ResourceGovernor::new(),
        &ledger,
        &diffs,
    )
    .expect("reviewed-tree pack");

    let commands = std::fs::read_to_string(pack.join("30_context/tauri-commands.txt"))
        .expect("reviewed Tauri commands artifact");
    assert!(
        commands.contains("[ADDED] src/commands/target.rs:target_command"),
        "target snapshot command missing: {commands}",
    );
    assert!(
        commands.contains("[REMOVED] src/local.rs:base_command"),
        "base command delta missing: {commands}",
    );
    assert!(
        !commands.contains("local_only_command") && !commands.contains("local-shell"),
        "local checkout layout leaked into reviewed Tauri truth: {commands}",
    );
}

// ---- PRV-203: Ownership Map ----

#[test]
fn test_parse_codeowners() {
    let content = r#"
# This is a comment
*.rs @rust-team
src/checks/ @quality-team @rust-team
docs/* @docs-team

# Empty line above
Cargo.toml @infra
"#;
    let entries = parse_codeowners(content);
    assert_eq!(entries.len(), 4);
    assert_eq!(entries[0].pattern, "*.rs");
    assert_eq!(entries[0].owners, vec!["@rust-team"]);
    assert_eq!(entries[1].pattern, "src/checks/");
    assert_eq!(entries[1].owners, vec!["@quality-team", "@rust-team"]);
    assert_eq!(entries[2].pattern, "docs/*");
    assert_eq!(entries[2].owners, vec!["@docs-team"]);
    assert_eq!(entries[3].pattern, "Cargo.toml");
    assert_eq!(entries[3].owners, vec!["@infra"]);
}

#[test]
fn test_find_owner_codeowners_match() {
    let entries = vec![
        OwnershipEntry {
            pattern: "*.rs".into(),
            owners: vec!["@rust-team".into()],
        },
        OwnershipEntry {
            pattern: "src/checks/".into(),
            owners: vec!["@quality-team".into()],
        },
    ];
    // src/checks/foo.rs matches both *.rs and src/checks/ — last match wins
    assert_eq!(find_owner("src/checks/foo.rs", &entries), "@quality-team");
    // src/main.rs matches *.rs only
    assert_eq!(find_owner("src/main.rs", &entries), "@rust-team");
}

#[test]
fn test_find_owner_fallback_module() {
    let entries: Vec<OwnershipEntry> = vec![];
    assert_eq!(find_owner("src/checks/foo.rs", &entries), "checks");
    assert_eq!(find_owner("tests/unit.rs", &entries), "tests");
    assert_eq!(find_owner("src/main.rs", &entries), "src");
}

#[test]
fn test_find_owner_unassigned_root() {
    let entries: Vec<OwnershipEntry> = vec![];
    assert_eq!(find_owner("Cargo.toml", &entries), "root");
}

#[test]
fn test_codeowners_pattern_extension_glob() {
    assert!(codeowners_pattern_matches("*.rs", "src/main.rs"));
    assert!(codeowners_pattern_matches("*.rs", "deep/nested/file.rs"));
    assert!(!codeowners_pattern_matches("*.rs", "src/main.ts"));
}

#[test]
fn test_codeowners_pattern_directory_slash() {
    assert!(codeowners_pattern_matches(
        "src/checks/",
        "src/checks/foo.rs"
    ));
    assert!(codeowners_pattern_matches(
        "src/checks/",
        "src/checks/sub/bar.rs"
    ));
    assert!(!codeowners_pattern_matches(
        "src/checks/",
        "src/checksum.rs"
    ));
}

#[test]
fn test_codeowners_pattern_directory_star() {
    assert!(codeowners_pattern_matches("docs/*", "docs/README.md"));
    assert!(codeowners_pattern_matches("docs/*", "docs/api/index.html"));
    assert!(!codeowners_pattern_matches(
        "docs/*",
        "documentation/file.md"
    ));
}

#[test]
fn test_codeowners_pattern_exact_match() {
    assert!(codeowners_pattern_matches("Cargo.toml", "Cargo.toml"));
    assert!(!codeowners_pattern_matches("Cargo.toml", "src/Cargo.toml"));
}

#[test]
fn test_codeowners_pattern_prefix_match() {
    assert!(codeowners_pattern_matches("src", "src/main.rs"));
    assert!(!codeowners_pattern_matches("src", "srclib/foo.rs"));
}

#[test]
fn test_codeowners_pattern_double_star_extension() {
    assert!(codeowners_pattern_matches("**/*.rs", "src/main.rs"));
    assert!(codeowners_pattern_matches(
        "**/*.rs",
        "deep/nested/dir/file.rs"
    ));
    assert!(codeowners_pattern_matches("**/*.rs", "file.rs"));
    assert!(!codeowners_pattern_matches("**/*.rs", "src/main.ts"));
    assert!(!codeowners_pattern_matches("**/*.rs", "src/rs/file.txt"));
}

#[test]
fn test_codeowners_pattern_double_star_dirname() {
    assert!(codeowners_pattern_matches("**/vendor/", "vendor/lib.rs"));
    assert!(codeowners_pattern_matches(
        "**/vendor/",
        "src/vendor/lib.rs"
    ));
    assert!(codeowners_pattern_matches(
        "**/vendor/",
        "deep/nested/vendor/file.txt"
    ));
    assert!(!codeowners_pattern_matches(
        "**/vendor/",
        "vendorlib/foo.rs"
    ));
    assert!(!codeowners_pattern_matches(
        "**/vendor/",
        "src/vendorlib/foo.rs"
    ));
}

#[test]
fn test_codeowners_pattern_double_star_filename() {
    assert!(codeowners_pattern_matches("**/Makefile", "Makefile"));
    assert!(codeowners_pattern_matches("**/Makefile", "src/Makefile"));
    assert!(codeowners_pattern_matches(
        "**/Makefile",
        "deep/nested/Makefile"
    ));
    assert!(!codeowners_pattern_matches("**/Makefile", "Makefile.bak"));
}

#[test]
fn test_codeowners_last_match_wins() {
    let entries = vec![
        OwnershipEntry {
            pattern: "*.rs".into(),
            owners: vec!["@general".into()],
        },
        OwnershipEntry {
            pattern: "src/".into(),
            owners: vec!["@src-team".into()],
        },
        OwnershipEntry {
            pattern: "src/checks/".into(),
            owners: vec!["@quality-team".into()],
        },
    ];
    // All three patterns match, but last wins
    assert_eq!(find_owner("src/checks/foo.rs", &entries), "@quality-team");
    // First two match, second wins
    assert_eq!(find_owner("src/main.rs", &entries), "@src-team");
}

// ---- PRV-204: Flaky Score computation ----

/// Helper: create a temporary directory structure with report.json files
/// and return the path to a "current run" directory inside it.
fn setup_flaky_test_dir(runs: &[(&str, &str)]) -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let branch_dir = tmp.path().join("branch");
    fs::create_dir_all(&branch_dir).unwrap();

    for (dir_name, report_content) in runs {
        let run_dir = branch_dir.join(dir_name);
        fs::create_dir_all(&run_dir).unwrap();
        fs::write(run_dir.join("report.json"), report_content).unwrap();
    }

    // Current run dir (newest timestamp)
    let current = branch_dir.join("20260302-180000");
    fs::create_dir_all(&current).unwrap();

    (tmp, current)
}

fn report_json_with_checks(checks: &[(&str, &str, &str)]) -> String {
    let checks_json: Vec<String> = checks.iter().map(|(id, name, status)| {
            format!(r#"{{"id":"{}","name":"{}","status":"{}","blocking":false,"cached":false,"duration_ms":1000}}"#, id, name, status)
        }).collect();
    format!(
        r#"{{"schema_version":"1","gate":{{"quality_pass":true,"allow_merge":true}},"checks":[{}],"diff":{{"files_changed":1}},"quality":{{}}}}"#,
        checks_json.join(",")
    )
}

#[test]
fn test_flaky_scores_empty_with_single_run() {
    let report = report_json_with_checks(&[("cargo", "cargo check", "PASS")]);
    let (_tmp, current) = setup_flaky_test_dir(&[("20260302-100000", &report)]);
    let scores = compute_flaky_scores(&current, 20);
    assert!(
        scores.is_empty(),
        "Single run should produce no flaky scores"
    );
}

#[test]
fn test_flaky_scores_stable_checks() {
    let report1 = report_json_with_checks(&[
        ("cargo", "cargo check", "PASS"),
        ("clippy", "clippy", "PASS"),
    ]);
    let report2 = report_json_with_checks(&[
        ("cargo", "cargo check", "PASS"),
        ("clippy", "clippy", "PASS"),
    ]);
    let (_tmp, current) =
        setup_flaky_test_dir(&[("20260301-100000", &report1), ("20260302-100000", &report2)]);
    let scores = compute_flaky_scores(&current, 20);
    assert!(
        scores.is_empty(),
        "Stable checks should produce no flaky scores"
    );
}

#[test]
fn test_flaky_scores_one_flaky_check() {
    let report1 = report_json_with_checks(&[
        ("cargo", "cargo check", "PASS"),
        ("cargo_test", "cargo test", "PASS"),
    ]);
    let report2 = report_json_with_checks(&[
        ("cargo", "cargo check", "PASS"),
        ("cargo_test", "cargo test", "FAIL"),
    ]);
    let report3 = report_json_with_checks(&[
        ("cargo", "cargo check", "PASS"),
        ("cargo_test", "cargo test", "PASS"),
    ]);
    let (_tmp, current) = setup_flaky_test_dir(&[
        ("20260301-100000", &report1),
        ("20260301-120000", &report2),
        ("20260302-100000", &report3),
    ]);
    let scores = compute_flaky_scores(&current, 20);
    assert_eq!(scores.len(), 1, "Should detect one flaky check");
    assert_eq!(scores[0].check_id, "cargo_test");
    assert_eq!(scores[0].transitions, 2);
    assert!(
        (scores[0].flaky_score - 1.0).abs() < f64::EPSILON,
        "Flaky score should be 1.0"
    );
    assert_eq!(scores[0].confidence, "low");
}

#[test]
fn test_flaky_scores_confidence_levels() {
    // Build 8 runs where cargo_test flips once at the boundary
    let mut runs: Vec<(String, String)> = Vec::new();
    for i in 0..8 {
        let status = if i < 4 { "PASS" } else { "FAIL" };
        let ts = format!("2026030{}-{:02}0000", i / 3 + 1, i % 10);
        let report = report_json_with_checks(&[("cargo_test", "cargo test", status)]);
        runs.push((ts, report));
    }
    let runs_ref: Vec<(&str, &str)> = runs.iter().map(|(a, b)| (a.as_str(), b.as_str())).collect();
    let (_tmp, current) = setup_flaky_test_dir(&runs_ref);
    let scores = compute_flaky_scores(&current, 20);
    assert_eq!(scores.len(), 1);
    assert_eq!(
        scores[0].confidence, "high",
        "8 runs should be high confidence"
    );
}

#[test]
fn test_flaky_scores_skips_malformed_reports() {
    let good_report = report_json_with_checks(&[("cargo", "cargo check", "PASS")]);
    let (_tmp, current) = setup_flaky_test_dir(&[
        ("20260301-100000", &good_report),
        ("20260301-120000", "not valid json at all"),
        ("20260302-100000", &good_report),
    ]);
    // Should not panic; malformed report is silently skipped
    let scores = compute_flaky_scores(&current, 20);
    // Two valid runs with same status => stable
    assert!(
        scores.is_empty(),
        "Malformed reports should be skipped gracefully"
    );
}

#[test]
fn test_flaky_scores_sorted_by_score_descending() {
    // Check A: flips every run (score = 1.0)
    // Check B: flips once in 3 runs (score = 0.5)
    let r1 = report_json_with_checks(&[("a", "Check A", "PASS"), ("b", "Check B", "PASS")]);
    let r2 = report_json_with_checks(&[("a", "Check A", "FAIL"), ("b", "Check B", "FAIL")]);
    let r3 = report_json_with_checks(&[("a", "Check A", "PASS"), ("b", "Check B", "FAIL")]);
    let (_tmp, current) = setup_flaky_test_dir(&[
        ("20260301-100000", &r1),
        ("20260301-120000", &r2),
        ("20260302-100000", &r3),
    ]);
    let scores = compute_flaky_scores(&current, 20);
    assert_eq!(scores.len(), 2);
    assert_eq!(
        scores[0].check_id, "a",
        "Highest flaky score should be first"
    );
    assert!(
        scores[0].flaky_score > scores[1].flaky_score,
        "Should be sorted descending"
    );
}

// -----------------------------------------------------------------------
// PRV-205: Lint metrics tests
// -----------------------------------------------------------------------

#[test]
fn test_parse_lint_issues_clippy_output() {
    let output = r#"warning: unused variable `x`
  --> src/foo.rs:42:9
   |
42 |     let x = 5;
   |         ^ help: if this is intentional, prefix it with an underscore

warning: unused import: `std::io`
  --> src/bar.rs:3:5
   |
3  | use std::io;
   |     ^^^^^^^
"#;
    let issues = parse_lint_issues(output);
    assert_eq!(issues.len(), 2);
    assert!(issues.contains(&"src/foo.rs".to_string()));
    assert!(issues.contains(&"src/bar.rs".to_string()));
}

#[test]
fn test_parse_lint_issues_eslint_output() {
    let output = r#"/home/user/project/src/app.js:10:5: warning 'foo' is defined but never used
/home/user/project/src/utils.ts:25:1: error Missing semicolon
src/index.tsx:3:8: warning Unexpected console statement
"#;
    let issues = parse_lint_issues(output);
    assert_eq!(issues.len(), 3);
    // Full paths get normalized but keep structure
    assert!(issues.iter().any(|p| p.contains("app.js")));
    assert!(issues.iter().any(|p| p.contains("utils.ts")));
    assert!(issues.iter().any(|p| p.contains("index.tsx")));
}

#[test]
fn test_parse_lint_issues_ruff_output() {
    let output = r#"src/foo.py:15:1: E501 Line too long (120 > 79)
src/bar.py:8:1: F401 `os` imported but unused
"#;
    let issues = parse_lint_issues(output);
    assert_eq!(issues.len(), 2);
    assert!(issues.contains(&"src/foo.py".to_string()));
    assert!(issues.contains(&"src/bar.py".to_string()));
}

#[test]
fn test_parse_lint_issues_empty_output() {
    let issues = parse_lint_issues("");
    assert!(issues.is_empty());

    let issues2 = parse_lint_issues("All checks passed!\n");
    assert!(issues2.is_empty());
}

#[test]
fn test_build_regression_patch_text_is_none_when_empty() {
    assert_eq!(build_regression_patch_text(&[]), None);
}

#[test]
fn test_build_regression_patch_text_truncates_on_char_boundary_and_appends_note() {
    let prefix = "a".repeat(MAX_PATCH_TEXT_BYTES - 1);
    let patch_text = format!("{prefix}ż");

    let built = build_regression_patch_text(&[patch_text]).expect("patch text");

    assert!(built.starts_with(&prefix));
    assert!(!built.contains('ż'));
    assert!(built.contains("Patch text truncated (>2 MB)"));
    assert!(built.is_char_boundary(prefix.len()));
}

#[test]
fn test_compute_lint_metrics_all_new() {
    use crate::git::{Diff, DiffStats, FileChange, FileStatus};

    let checks = vec![CheckResult {
        name: "cargo clippy".into(),
        status: CheckStatus::Warnings,
        duration: Duration::from_secs(5),
        output: "warning: unused\n  --> src/main.rs:10:5\nwarning: unused\n  --> src/lib.rs:20:1\n"
            .into(),
        cached: false,
        provenance: None,
    }];
    let diffs = vec![Diff {
        base: "main".into(),
        target: "feature/x".into(),
        base_commit_id: "aaa".into(),
        target_commit_id: "bbb".into(),
        files: vec![
            FileChange {
                path: "src/main.rs".into(),
                status: FileStatus::Modified,
                additions: 10,
                deletions: 2,
            },
            FileChange {
                path: "src/lib.rs".into(),
                status: FileStatus::Modified,
                additions: 5,
                deletions: 1,
            },
        ],
        stats: DiffStats {
            files_changed: 2,
            additions: 15,
            deletions: 3,
            copied: 0,
        },
        commits: vec![],
    }];

    let metrics = compute_lint_metrics(&checks, &diffs);
    assert_eq!(metrics.len(), 1);
    assert_eq!(metrics[0].check_name, "cargo clippy");
    assert_eq!(metrics[0].new_issues, 2);
    assert_eq!(metrics[0].legacy_issues, 0);
    assert_eq!(metrics[0].total_issues, 2);
    assert_eq!(metrics[0].changed_files_with_issues.len(), 2);
}

#[test]
fn test_compute_lint_metrics_mixed_new_and_legacy() {
    use crate::git::{Diff, DiffStats, FileChange, FileStatus};

    let checks = vec![CheckResult {
        name: "cargo clippy".into(),
        status: CheckStatus::Warnings,
        duration: Duration::from_secs(5),
        output:
            "warning: unused\n  --> src/main.rs:10:5\nwarning: unused\n  --> src/legacy.rs:20:1\n"
                .into(),
        cached: false,
        provenance: None,
    }];
    let diffs = vec![Diff {
        base: "main".into(),
        target: "feature/x".into(),
        base_commit_id: "aaa".into(),
        target_commit_id: "bbb".into(),
        files: vec![FileChange {
            path: "src/main.rs".into(),
            status: FileStatus::Modified,
            additions: 10,
            deletions: 2,
        }],
        stats: DiffStats {
            files_changed: 1,
            additions: 10,
            deletions: 2,
            copied: 0,
        },
        commits: vec![],
    }];

    let metrics = compute_lint_metrics(&checks, &diffs);
    assert_eq!(metrics.len(), 1);
    assert_eq!(metrics[0].new_issues, 1, "src/main.rs is in the diff");
    assert_eq!(
        metrics[0].legacy_issues, 1,
        "src/legacy.rs is NOT in the diff"
    );
    assert_eq!(metrics[0].total_issues, 2);
    assert_eq!(metrics[0].changed_files_with_issues, vec!["src/main.rs"]);
}

#[test]
fn test_compute_lint_metrics_skips_error_checks() {
    let checks = vec![CheckResult {
        name: "cargo clippy".into(),
        status: CheckStatus::Error,
        duration: Duration::from_secs(1),
        output: "INTERNAL ERROR: some garbage\n  --> src/main.rs:1:1\n".into(),
        cached: false,
        provenance: None,
    }];
    let diffs: Vec<Diff> = vec![];

    let metrics = compute_lint_metrics(&checks, &diffs);
    assert!(metrics.is_empty(), "Error-status checks should be skipped");
}

#[test]
fn test_compute_lint_metrics_non_lint_checks_ignored() {
    let checks = vec![
        CheckResult {
            name: "cargo check".into(),
            status: CheckStatus::Passed,
            duration: Duration::from_secs(2),
            output: "Compiling project\n".into(),
            cached: false,
            provenance: None,
        },
        CheckResult {
            name: "cargo test".into(),
            status: CheckStatus::Passed,
            duration: Duration::from_secs(10),
            output: "test result: ok. 42 passed\n".into(),
            cached: false,
            provenance: None,
        },
    ];
    let diffs: Vec<Diff> = vec![];

    let metrics = compute_lint_metrics(&checks, &diffs);
    assert!(
        metrics.is_empty(),
        "Non-lint checks should not produce metrics"
    );
}

#[test]
fn test_is_lint_check_identification() {
    assert!(is_lint_check("cargo clippy"));
    assert!(is_lint_check("ESLint"));
    assert!(is_lint_check("ruff"));
    assert!(is_lint_check("mypy"));
    assert!(is_lint_check("pylint check"));
    assert!(is_lint_check("biome lint"));
    assert!(is_lint_check("stylelint"));
    assert!(!is_lint_check("cargo check"));
    assert!(!is_lint_check("cargo test"));
    assert!(!is_lint_check("cargo audit"));
    assert!(!is_lint_check("vitest"));
}

#[test]
fn test_normalize_lint_path() {
    assert_eq!(normalize_lint_path("./src/foo.rs"), "src/foo.rs");
    assert_eq!(normalize_lint_path("/src/foo.rs"), "src/foo.rs");
    assert_eq!(normalize_lint_path("src/foo.rs"), "src/foo.rs");
    assert_eq!(normalize_lint_path("  src/foo.rs  "), "src/foo.rs");
}

#[test]
fn test_normalize_lint_path_absolute_unix() {
    // P1 fix: absolute paths must be relativized to repo-relative form
    assert_eq!(
        normalize_lint_path("/Users/dev/Git/myapp/src/app.ts"),
        "src/app.ts"
    );
    assert_eq!(
        normalize_lint_path("/home/ci/project/src/utils/helpers.rs"),
        "src/utils/helpers.rs"
    );
    // Prefix depth >= 2 slashes → cut
    assert_eq!(
        normalize_lint_path("/opt/ci/build/lib/parser.py"),
        "lib/parser.py"
    );
    // Prefix depth < 2 slashes → stay as-is
    assert_eq!(
        normalize_lint_path("/opt/build/lib/parser.py"),
        "opt/build/lib/parser.py"
    );
    // No known marker — returns as-is (minus leading /)
    assert_eq!(
        normalize_lint_path("/weird/path/foo.rs"),
        "weird/path/foo.rs"
    );
}

#[test]
fn test_normalize_lint_path_windows() {
    // P2 fix: Windows backslash paths and drive letters
    assert_eq!(
        normalize_lint_path(r"C:\Users\dev\project\src\foo.rs"),
        "src/foo.rs"
    );
    assert_eq!(normalize_lint_path(r"src\bar\baz.ts"), "src/bar/baz.ts");
    assert_eq!(normalize_lint_path(r".\src\foo.rs"), "src/foo.rs");
    // Shallow depth after drive strip — stays as-is
    assert_eq!(
        normalize_lint_path(r"D:\builds\app\lib\utils.py"),
        "builds/app/lib/utils.py"
    );
    // Deep Windows path (prefix depth >= 2) gets relativized
    assert_eq!(
        normalize_lint_path(r"C:\Users\dev\project\src\foo.rs"),
        "src/foo.rs"
    );
}

#[test]
fn test_parse_lint_issues_windows_paths() {
    // Windows clippy-like output with backslashes
    let output = r"warning: unused variable
  --> src\foo.rs:42:9

error: mismatched types
  --> src\bar\baz.rs:10:5
";
    let issues = parse_lint_issues(output);
    assert_eq!(issues.len(), 2);
    assert!(issues.contains(&"src/foo.rs".to_string()));
    assert!(issues.contains(&"src/bar/baz.rs".to_string()));
}

#[test]
fn test_parse_lint_issues_eslint_block_format() {
    // ESLint/Stylelint default formatter: path on own line, indented issues below
    let output = r#"
/Users/dev/project/src/components/App.tsx
  10:5  warning  Unexpected console statement  no-console
  25:1  error    Missing semicolon             semi

/Users/dev/project/src/utils/helpers.ts
  3:8   warning  'foo' is defined but never used  no-unused-vars

src/index.tsx
  42:10  error   'bar' is not defined  no-undef
  50:3   warning  Unexpected var       no-var
"#;
    let issues = parse_lint_issues(output);
    // 2 issues from App.tsx + 1 from helpers.ts + 2 from index.tsx = 5
    assert_eq!(issues.len(), 5);
    let app_count = issues.iter().filter(|p| p.contains("App.tsx")).count();
    assert_eq!(app_count, 2, "App.tsx should have 2 issues");
    let helpers_count = issues.iter().filter(|p| p.contains("helpers.ts")).count();
    assert_eq!(helpers_count, 1, "helpers.ts should have 1 issue");
    let index_count = issues.iter().filter(|p| p.contains("index.tsx")).count();
    assert_eq!(index_count, 2, "index.tsx should have 2 issues");
}

#[test]
fn test_parse_lint_issues_ignores_false_positive_block_paths_without_issue_lines() {
    let output = r#"
/Users/dev/project/src/components/App.tsx
This is just a heading, not a lint issue

src/index.tsx
  42:10  error   'bar' is not defined  no-undef
"#;

    let issues = parse_lint_issues(output);
    assert_eq!(issues, vec!["src/index.tsx".to_string()]);
}

#[test]
fn test_normalize_lint_path_src_tauri_collision() {
    // P2: relative paths with nested markers must NOT be truncated
    assert_eq!(
        normalize_lint_path("src-tauri/src/lib.rs"),
        "src-tauri/src/lib.rs"
    );
    assert_eq!(
        normalize_lint_path("packages/app/src/main.ts"),
        "packages/app/src/main.ts"
    );
    // Deep absolute paths (>=3 segments prefix) still get relativized
    assert_eq!(
        normalize_lint_path("/Users/dev/Git/myapp/src/app.ts"),
        "src/app.ts"
    );
    assert_eq!(
        normalize_lint_path("/home/ci/project/lib/parser.py"),
        "lib/parser.py"
    );
}

#[test]
fn test_compute_lint_metrics_clean_lint() {
    use crate::git::{Diff, DiffStats, FileChange, FileStatus};

    let checks = vec![CheckResult {
        name: "cargo clippy".into(),
        status: CheckStatus::Passed,
        duration: Duration::from_secs(5),
        output: "Checking project v0.1.0\n    Finished `dev` profile [unoptimized + debuginfo]\n"
            .into(),
        cached: false,
        provenance: None,
    }];
    let diffs = vec![Diff {
        base: "main".into(),
        target: "feature/x".into(),
        base_commit_id: "aaa".into(),
        target_commit_id: "bbb".into(),
        files: vec![FileChange {
            path: "src/main.rs".into(),
            status: FileStatus::Modified,
            additions: 10,
            deletions: 2,
        }],
        stats: DiffStats {
            files_changed: 1,
            additions: 10,
            deletions: 2,
            copied: 0,
        },
        commits: vec![],
    }];

    let metrics = compute_lint_metrics(&checks, &diffs);
    assert_eq!(metrics.len(), 1);
    assert_eq!(metrics[0].total_issues, 0);
    assert_eq!(metrics[0].new_issues, 0);
    assert_eq!(metrics[0].legacy_issues, 0);
}

#[test]
fn test_generate_heuristics_gate_result_with_regression() {
    use crate::heuristics::{HeuristicsRegression, HeuristicsResult, HeuristicsSummary};

    let regression = HeuristicsRegression {
        base_sha: "aaaaaaa".to_string(),
        target_sha: "bbbbbbb".to_string(),
        dead_exports_delta: 3,
        cycles_delta: -1,
        dead_parrots_delta: 0,
        base_dead_exports: 2,
        target_dead_exports: 5,
        base_circular_imports: 4,
        target_circular_imports: 3,
        base_dead_parrots: 0,
        target_dead_parrots: 0,
        regression_detected: true,
        improvement_detected: true,
    };

    let heuristics = HeuristicsResult {
        summary: HeuristicsSummary {
            dead_exports: 5,
            circular_imports: 3,
            dead_parrots: 0,
            exact_twins: 0,
            total_files: 20,
            total_loc: 1000,
        },
        regression: Some(regression),
        ..Default::default()
    };

    let tmp = tempfile::tempdir().expect("tempdir");
    generate_heuristics_gate_result(tmp.path(), Some(&heuristics))
        .expect("generate_heuristics_gate_result");

    // Verify the result JSON was written
    let result_path = tmp.path().join("heuristics_loctree.result.json");
    assert!(result_path.exists(), "result JSON should exist");

    let raw = std::fs::read_to_string(&result_path).expect("read result json");
    let value: serde_json::Value = serde_json::from_str(&raw).expect("parse result json");

    // Gate fields
    assert_eq!(value["gate"].as_str(), Some("heuristics_loctree"));
    assert_eq!(value["status"].as_str(), Some("warnings"));
    assert_eq!(value["dead_exports"].as_u64(), Some(5));
    assert_eq!(value["circular_imports"].as_u64(), Some(3));
    assert_eq!(value["unused_symbols"].as_u64(), Some(0));

    // Regression block must be present and correct
    let reg = &value["regression"];
    assert!(!reg.is_null(), "regression field should be present in JSON");
    assert_eq!(reg["dead_exports_delta"].as_i64(), Some(3));
    assert_eq!(reg["cycles_delta"].as_i64(), Some(-1));
    assert_eq!(reg["regression_detected"].as_bool(), Some(true));
    assert_eq!(reg["improvement_detected"].as_bool(), Some(true));
    assert_eq!(reg["base_sha"].as_str(), Some("aaaaaaa"));
    assert_eq!(reg["target_sha"].as_str(), Some("bbbbbbb"));

    // Log file should also mention regression and unused symbols
    let log_path = tmp.path().join("heuristics_loctree.log");
    assert!(log_path.exists(), "log file should exist");
    let log = std::fs::read_to_string(&log_path).expect("read log");
    assert!(
        log.contains("Unused symbols:"),
        "log should contain Unused symbols line"
    );
    assert!(
        log.contains("Regression"),
        "log should contain regression section"
    );
    assert!(
        log.contains("Unused symbols delta:"),
        "log regression should use 'Unused symbols delta' label"
    );
    assert!(log.contains("aaaaaaa"), "log should reference base SHA");
}

// ---- Pre-existing vs introduced quality failure gate tests ----

#[test]
fn quality_failure_summary_has_new_failures_with_introduced() {
    let mut summary = QualityFailureSummary::default();
    push_quality_failure(
        &mut summary,
        "ESLint".to_string(),
        QualityFailureClass::Introduced,
        QualityFailureOrigin::Failure,
    );
    assert!(summary.has_new_failures());
}

#[test]
fn quality_failure_summary_has_new_failures_with_mixed() {
    let mut summary = QualityFailureSummary::default();
    push_quality_failure(
        &mut summary,
        "ESLint".to_string(),
        QualityFailureClass::Mixed,
        QualityFailureOrigin::Failure,
    );
    assert!(summary.has_new_failures());
}

#[test]
fn quality_failure_summary_has_new_failures_with_unclassified() {
    let mut summary = QualityFailureSummary::default();
    push_quality_failure(
        &mut summary,
        "cargo test".to_string(),
        QualityFailureClass::Unclassified,
        QualityFailureOrigin::Failure,
    );
    assert!(summary.has_new_failures());
}

#[test]
fn quality_failure_summary_no_new_failures_when_only_preexisting() {
    let mut summary = QualityFailureSummary::default();
    push_quality_failure(
        &mut summary,
        "Cargo audit".to_string(),
        QualityFailureClass::Preexisting,
        QualityFailureOrigin::Failure,
    );
    push_quality_failure(
        &mut summary,
        "ESLint".to_string(),
        QualityFailureClass::Preexisting,
        QualityFailureOrigin::Failure,
    );
    assert!(!summary.has_new_failures());
    // quality_failures still lists them (backward compat)
    assert_eq!(summary.quality_failures.len(), 2);
}

#[test]
fn quality_failure_summary_no_new_failures_when_empty() {
    let summary = QualityFailureSummary::default();
    assert!(!summary.has_new_failures());
}

#[test]
fn preexisting_failures_do_not_block_gate() {
    let config = test_config_builder()
        .target(Some("feature/preexisting-test"))
        .bases(&["main"])
        .profile(test_js_profile(true))
        .execution_mode(ExecutionMode::Standard)
        .run_tests(false)
        .run_lint(true)
        .do_fetch(false)
        .use_cache(false)
        .create_zip(false)
        .policy(PolicyConfig::default())
        .build();
    let checks = vec![
        CheckResult {
            name: "ESLint".to_string(),
            status: CheckStatus::Failed,
            duration: Duration::from_secs(1),
            output: "src/legacy.ts:5:1: error no-var".to_string(),
            cached: false,
            provenance: None,
        },
        CheckResult {
            name: "Prettier".to_string(),
            status: CheckStatus::Failed,
            duration: Duration::from_secs(1),
            output: "src/old-style.ts:1:1: error formatting".to_string(),
            cached: false,
            provenance: None,
        },
    ];
    // All findings are outside the diff
    let inline = InlineFindingsSummary {
        status: "warnings".to_string(),
        findings_count: 2,
        dashboard_findings: vec![
            DashboardFinding {
                level: "error",
                check_name: "ESLint".to_string(),
                check_id: "eslint".to_string(),
                message: "no-var".to_string(),
                in_diff: Some(false),
            },
            DashboardFinding {
                level: "error",
                check_name: "Prettier".to_string(),
                check_id: "prettier".to_string(),
                message: "formatting".to_string(),
                in_diff: Some(false),
            },
        ],
    };
    let resolved_target = ResolvedRef {
        name: "feature/preexisting-test".to_string(),
        commit_id: "abc1234abc1234abc1234abc1234abc1234ab".to_string(),
        is_remote: false,
    };
    let resolved_bases = vec![ResolvedRef {
        name: "origin/main".to_string(),
        commit_id: "def5678def5678def5678def5678def5678de".to_string(),
        is_remote: true,
    }];
    let tmp = tempfile::tempdir().expect("tempdir");

    generate_merge_gate_test!(
        tmp.path(),
        &config,
        &checks,
        None,
        &inline,
        &[],
        &CoverageDelta {
            total_source: 0,
            covered_count: 0,
            pct: None,
            uncovered: vec![],
            covered: vec![],
            non_code_count: 0,
            ghost_tests: vec![],
        },
        &[],
        &resolved_target,
        &resolved_bases,
    )
    .expect("merge gate");

    let raw = std::fs::read_to_string(tmp.path().join("MERGE_GATE.json")).expect("read gate");
    let gate: serde_json::Value = serde_json::from_str(&raw).expect("parse gate");
    // Pre-existing failures: quality_pass=true, gate does not block
    assert_eq!(gate["decision"]["quality_pass"].as_bool(), Some(true));
    assert_eq!(gate["decision"]["verdict"].as_str(), Some("PASS"));
    assert_eq!(
        gate["decision"]["merge_recommendation"].as_str(),
        Some("approve")
    );
    // Both appear in quality_failures (backward compat)
    assert_eq!(
        gate["decision"]["quality_failures"]
            .as_array()
            .map(|a| a.len()),
        Some(2)
    );
    // Both classified as pre-existing
    assert_eq!(
        gate["decision"]["preexisting_quality_failures"]
            .as_array()
            .map(|a| a.len()),
        Some(2)
    );
    assert!(
        gate["decision"]["introduced_quality_failures"]
            .as_array()
            .map(|a| a.is_empty())
            .unwrap_or(true)
    );
    // Pre-existing failures surfaced as review caveat
    assert!(
        gate["decision"]["review_caveats"]
            .as_array()
            .is_some_and(|caveats| caveats
                .iter()
                .any(|c| c.as_str().is_some_and(|s| s.contains("Pre-existing"))))
    );
}

#[test]
fn introduced_failures_still_block_gate() {
    let config = test_config_builder()
        .target(Some("feature/introduced-test"))
        .bases(&["main"])
        .profile(test_js_profile(true))
        .execution_mode(ExecutionMode::Standard)
        .run_tests(false)
        .run_lint(true)
        .do_fetch(false)
        .use_cache(false)
        .create_zip(false)
        .policy(PolicyConfig::default())
        .build();
    let checks = vec![CheckResult {
        name: "ESLint".to_string(),
        status: CheckStatus::Failed,
        duration: Duration::from_secs(1),
        output: "src/new-file.ts:5:1: error no-any".to_string(),
        cached: false,
        provenance: None,
    }];
    // Finding is IN the diff (introduced)
    let inline = InlineFindingsSummary {
        status: "warnings".to_string(),
        findings_count: 1,
        dashboard_findings: vec![DashboardFinding {
            level: "error",
            check_name: "ESLint".to_string(),
            check_id: "eslint".to_string(),
            message: "no-any".to_string(),
            in_diff: Some(true),
        }],
    };
    let resolved_target = ResolvedRef {
        name: "feature/introduced-test".to_string(),
        commit_id: "abc1234abc1234abc1234abc1234abc1234ab".to_string(),
        is_remote: false,
    };
    let resolved_bases = vec![ResolvedRef {
        name: "origin/main".to_string(),
        commit_id: "def5678def5678def5678def5678def5678de".to_string(),
        is_remote: true,
    }];
    let tmp = tempfile::tempdir().expect("tempdir");

    generate_merge_gate_test!(
        tmp.path(),
        &config,
        &checks,
        None,
        &inline,
        &[],
        &CoverageDelta {
            total_source: 0,
            covered_count: 0,
            pct: None,
            uncovered: vec![],
            covered: vec![],
            non_code_count: 0,
            ghost_tests: vec![],
        },
        &[],
        &resolved_target,
        &resolved_bases,
    )
    .expect("merge gate");

    let raw = std::fs::read_to_string(tmp.path().join("MERGE_GATE.json")).expect("read gate");
    let gate: serde_json::Value = serde_json::from_str(&raw).expect("parse gate");

    // Introduced failures: quality_pass=false, gate blocks
    assert_eq!(gate["decision"]["quality_pass"].as_bool(), Some(false));
    assert_eq!(
        gate["decision"]["introduced_quality_failures"]
            .as_array()
            .map(|a| a.len()),
        Some(1)
    );
    assert!(
        gate["decision"]["preexisting_quality_failures"]
            .as_array()
            .map(|a| a.is_empty())
            .unwrap_or(true)
    );
    // Introduced failures map to an explicit review verdict, not a soft green.
    assert_eq!(gate["decision"]["verdict"].as_str(), Some("CONDITIONAL"));
    assert_eq!(
        gate["decision"]["merge_recommendation"].as_str(),
        Some("review_required")
    );
}

#[test]
fn mixed_failures_include_both_preexisting_and_introduced_in_output() {
    let config = test_config_builder()
        .target(Some("feature/mixed-test"))
        .bases(&["main"])
        .profile(test_js_profile(true))
        .execution_mode(ExecutionMode::Standard)
        .run_tests(false)
        .run_lint(true)
        .do_fetch(false)
        .use_cache(false)
        .create_zip(false)
        .policy(PolicyConfig::default())
        .build();
    let checks = vec![
        CheckResult {
            name: "ESLint".to_string(),
            status: CheckStatus::Failed,
            duration: Duration::from_secs(1),
            output: "src/new.ts:1: error\nsrc/old.ts:1: error".to_string(),
            cached: false,
            provenance: None,
        },
        CheckResult {
            name: "Prettier".to_string(),
            status: CheckStatus::Failed,
            duration: Duration::from_secs(1),
            output: "src/legacy.ts:1: error".to_string(),
            cached: false,
            provenance: None,
        },
    ];
    // ESLint has findings both in and out of diff (mixed)
    // Prettier has findings only out of diff (pre-existing)
    let inline = InlineFindingsSummary {
        status: "warnings".to_string(),
        findings_count: 3,
        dashboard_findings: vec![
            DashboardFinding {
                level: "error",
                check_name: "ESLint".to_string(),
                check_id: "eslint".to_string(),
                message: "new error".to_string(),
                in_diff: Some(true),
            },
            DashboardFinding {
                level: "error",
                check_name: "ESLint".to_string(),
                check_id: "eslint".to_string(),
                message: "old error".to_string(),
                in_diff: Some(false),
            },
            DashboardFinding {
                level: "error",
                check_name: "Prettier".to_string(),
                check_id: "prettier".to_string(),
                message: "formatting".to_string(),
                in_diff: Some(false),
            },
        ],
    };
    let resolved_target = ResolvedRef {
        name: "feature/mixed-test".to_string(),
        commit_id: "abc1234abc1234abc1234abc1234abc1234ab".to_string(),
        is_remote: false,
    };
    let resolved_bases = vec![ResolvedRef {
        name: "origin/main".to_string(),
        commit_id: "def5678def5678def5678def5678def5678de".to_string(),
        is_remote: true,
    }];
    let tmp = tempfile::tempdir().expect("tempdir");

    generate_merge_gate_test!(
        tmp.path(),
        &config,
        &checks,
        None,
        &inline,
        &[],
        &CoverageDelta {
            total_source: 0,
            covered_count: 0,
            pct: None,
            uncovered: vec![],
            covered: vec![],
            non_code_count: 0,
            ghost_tests: vec![],
        },
        &[],
        &resolved_target,
        &resolved_bases,
    )
    .expect("merge gate");

    let raw = std::fs::read_to_string(tmp.path().join("MERGE_GATE.json")).expect("read gate");
    let gate: serde_json::Value = serde_json::from_str(&raw).expect("parse gate");

    // Mixed ESLint has new findings so quality_pass = false
    assert_eq!(gate["decision"]["quality_pass"].as_bool(), Some(false));
    // ESLint is mixed, Prettier is pre-existing
    assert_eq!(
        gate["decision"]["mixed_quality_failures"]
            .as_array()
            .map(|a| a.len()),
        Some(1)
    );
    assert_eq!(
        gate["decision"]["preexisting_quality_failures"]
            .as_array()
            .map(|a| a.len()),
        Some(1)
    );
    // Both appear in quality_failures (backward compat)
    assert_eq!(
        gate["decision"]["quality_failures"]
            .as_array()
            .map(|a| a.len()),
        Some(2)
    );
    // Details show both classifications
    let details = gate["decision"]["quality_failure_details"]
        .as_array()
        .expect("details array");
    assert!(
        details
            .iter()
            .any(|d| d["name"] == "ESLint" && d["classification"] == "mixed")
    );
    assert!(
        details
            .iter()
            .any(|d| d["name"] == "Prettier" && d["classification"] == "pre-existing")
    );
}

// ── generate_review_summary tests ─────────────────────────────────

#[test]
fn review_summary_includes_gate_and_checks() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let out = tmp.path();

    // Create expected input files
    fs::create_dir_all(out.join("00_summary")).unwrap();
    fs::write(
        out.join("00_summary/MERGE_GATE.json"),
        r#"{"decision": {"verdict": "PASS", "decision_reason": "all checks green"}}"#,
    )
    .unwrap();
    fs::write(
        out.join("PR_REVIEW.md"),
        "# PR Review\n\nLooks good overall.\n",
    )
    .unwrap();

    fs::create_dir_all(out.join("30_context")).unwrap();
    fs::write(out.join("30_context/PATTERN_SCAN.json"), "{}").unwrap();
    fs::write(out.join("30_context/DEPS_DELTA.json"), "{}").unwrap();
    fs::write(out.join("report.json"), "{}").unwrap();

    generate_review_summary(out).unwrap();

    let summary = fs::read_to_string(out.join("REVIEW_SUMMARY.md")).unwrap();
    assert!(summary.contains("# PR Review Summary"));
    assert!(summary.contains("## Gate Decision"));
    assert!(summary.contains("PASS"));
    assert!(summary.contains("## Review"));
    assert!(summary.contains("Looks good overall"));
    assert!(summary.contains("## Available Artifacts"));
    assert!(summary.contains("## Available Artifacts"));
    assert!(summary.contains("PATTERN_SCAN.json"));
    assert!(summary.contains("DEPS_DELTA.json"));
}

#[test]
fn review_summary_handles_missing_files() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let out = tmp.path();

    // No input files exist — function should still succeed
    generate_review_summary(out).unwrap();

    let summary = fs::read_to_string(out.join("REVIEW_SUMMARY.md")).unwrap();
    assert!(summary.contains("# PR Review Summary"));
    assert!(summary.contains("## Available Artifacts"));
    // Should NOT have Gate/Review/Artifact Map sections
    assert!(
        !summary.contains("## Gate Decision"),
        "No gate section when MERGE_GATE.json is missing"
    );
    assert!(
        !summary.contains("## Review"),
        "No review section when PR_REVIEW.md is missing"
    );
    assert!(summary.contains("## Artifact Map"));
}

#[test]
fn review_summary_partial_sources_gate_only() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let out = tmp.path();

    // Only MERGE_GATE.json present
    fs::create_dir_all(out.join("00_summary")).unwrap();
    fs::write(
        out.join("00_summary/MERGE_GATE.json"),
        r#"{"decision": {"verdict": "BLOCK", "decision_reason": "tests failed"}}"#,
    )
    .unwrap();

    generate_review_summary(out).unwrap();

    let summary = fs::read_to_string(out.join("REVIEW_SUMMARY.md")).unwrap();
    assert!(summary.contains("## Gate Decision"));
    assert!(summary.contains("BLOCK"));
    assert!(
        !summary.contains("## Review"),
        "No review section when PR_REVIEW.md is missing"
    );
    assert!(summary.contains("## Artifact Map"));
}

#[test]
fn test_regression_signal_rs_conflict() {
    // prview uses a split signal/ module directory. If signal.rs gets recreated
    // (e.g. by a bad merge or branch switch), the Rust compiler will fail with
    // "file for module `signal` found at both signal.rs and signal/mod.rs".
    // This test ensures signal.rs does not exist in the source tree alongside signal/mod.rs.
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".to_string());
    let src_dir = std::path::Path::new(&manifest_dir)
        .join("src")
        .join("artifacts");
    let signal_dir = src_dir.join("signal");
    let signal_rs = src_dir.join("signal.rs");

    if signal_dir.is_dir() {
        assert!(
            !signal_rs.exists(),
            "CRITICAL REGRESSION: Both `signal/` directory and `signal.rs` file exist! \
                This causes a Rust compilation error. Delete `signal.rs`."
        );
    }
}

// ── run_sanity_checks: timings_outputs_present (TOOLING-10) ──────────

/// Build a minimal valid out_dir (required files + a RUN.json with the
/// given `context_commands` / `context_artifacts`) for sanity testing.
fn write_sanity_run(out_dir: &Path, run_extra: serde_json::Value) {
    let summary = out_dir.join("00_summary");
    fs::create_dir_all(&summary).unwrap();
    fs::write(summary.join("system_meta.txt"), "meta").unwrap();
    fs::write(summary.join("git_meta.txt"), "git").unwrap();
    fs::write(summary.join("MANIFEST.json"), r#"{"files":[]}"#).unwrap();
    let mut run = serde_json::json!({ "checks": [] });
    if let serde_json::Value::Object(extra) = run_extra {
        for (k, v) in extra {
            run[k] = v;
        }
    }
    fs::write(
        summary.join("RUN.json"),
        serde_json::to_string_pretty(&run).unwrap(),
    )
    .unwrap();
}

#[test]
fn sanity_flags_completed_context_command_with_missing_output() {
    let tmp = tempfile::TempDir::new().unwrap();
    write_sanity_run(
        tmp.path(),
        serde_json::json!({
            "context_commands": [{
                "label": "ai-index",
                "artifact": "30_context/AI_INDEX.md",
                "status": "completed",
                "duration_secs": 0.1,
            }]
        }),
    );

    let result = run_sanity_checks(tmp.path()).unwrap();
    assert!(
        result
            .failures
            .iter()
            .any(|f| f.contains("ai-index") && f.contains("AI_INDEX.md")),
        "missing completed-command output must be flagged, got: {:?}",
        result.failures
    );
    let sanity_text = fs::read_to_string(tmp.path().join("00_summary/SANITY.json")).unwrap();
    let sanity: serde_json::Value = serde_json::from_str(&sanity_text).unwrap();
    let check = sanity["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "timings_outputs_present")
        .expect("timings_outputs_present check present");
    assert_eq!(check["passed"], false);
}

#[test]
fn sanity_passes_when_completed_command_output_exists() {
    let tmp = tempfile::TempDir::new().unwrap();
    write_sanity_run(
        tmp.path(),
        serde_json::json!({
            "context_commands": [{
                "label": "ai-index",
                "artifact": "30_context/AI_INDEX.md",
                "status": "completed",
                "duration_secs": 0.1,
            }]
        }),
    );
    let ctx = tmp.path().join("30_context");
    fs::create_dir_all(&ctx).unwrap();
    fs::write(ctx.join("AI_INDEX.md"), "# index").unwrap();

    let result = run_sanity_checks(tmp.path()).unwrap();
    let sanity_text = fs::read_to_string(tmp.path().join("00_summary/SANITY.json")).unwrap();
    let sanity: serde_json::Value = serde_json::from_str(&sanity_text).unwrap();
    let check = sanity["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "timings_outputs_present")
        .expect("timings_outputs_present check present");
    assert_eq!(
        check["passed"], true,
        "existing output must pass; failures: {:?}",
        result.failures
    );
}

#[test]
fn invalid_sanity_is_rejected_before_publication_helper() {
    let sanity = SanityResult {
        valid: false,
        checks_run: 5,
        checks_passed: 4,
        failures: vec!["completed context artifact is missing".to_owned()],
    };
    let publication_started = std::cell::Cell::new(false);

    let result = (|| -> Result<()> {
        ensure_sanity_valid(&sanity)?;
        publication_started.set(true);
        Ok(())
    })();

    let error = result.expect_err("invalid SANITY must abort finalization");
    assert!(error.to_string().contains("4/5 checks passed"));
    assert!(error.to_string().contains("context artifact is missing"));
    assert!(!publication_started.get());

    let production = include_str!("mod.rs");
    let sanity_guard = production
        .find("ensure_sanity_valid(&sanity)?")
        .expect("production sanity guard");
    let zip = production
        .find("if config.create_zip")
        .expect("ZIP finalization");
    let publication = production
        .find("begin_latest_publication(&publication, &out_dir)")
        .expect("publication helper");
    assert!(sanity_guard < zip);
    assert!(sanity_guard < publication);
}

#[test]
fn sanity_ignores_timed_out_command_with_missing_output() {
    // A timed-out command legitimately may not have written its artifact.
    let tmp = tempfile::TempDir::new().unwrap();
    write_sanity_run(
        tmp.path(),
        serde_json::json!({
            "context_commands": [{
                "label": "slow-cmd",
                "artifact": "30_context/slow.log",
                "status": "timed_out",
                "duration_secs": 30.0,
            }]
        }),
    );

    let result = run_sanity_checks(tmp.path()).unwrap();
    assert!(
        !result.failures.iter().any(|f| f.contains("slow-cmd")),
        "timed-out command must not be flagged for missing output"
    );
}

#[test]
fn sanity_clean_run_still_requires_failures_summary() {
    // FAILURES_SUMMARY.md is now written on every run (clean runs get a stub),
    // so even a clean run's "MERGE_GATE + FAILURES_SUMMARY" timing label must
    // flag the file when it is absent. The former conditional exemption is gone.
    let tmp = tempfile::TempDir::new().unwrap();
    write_sanity_run(
        tmp.path(),
        serde_json::json!({
            "timings": [{ "label": "MERGE_GATE + FAILURES_SUMMARY", "secs": 0.1 }]
        }),
    );
    // MERGE_GATE.json exists (always generated); FAILURES_SUMMARY.md does not.
    fs::write(
        tmp.path().join("00_summary/MERGE_GATE.json"),
        r#"{"decision":{}}"#,
    )
    .unwrap();
    // A passing quality gate result, no failures — yet the summary is required.
    let quality = tmp.path().join("20_quality");
    fs::create_dir_all(&quality).unwrap();
    fs::write(quality.join("clippy.result.json"), r#"{"status":"passed"}"#).unwrap();

    let result = run_sanity_checks(tmp.path()).unwrap();
    assert!(
        result
            .failures
            .iter()
            .any(|f| f.contains("FAILURES_SUMMARY.md")),
        "clean run with missing FAILURES_SUMMARY.md must be flagged, got: {:?}",
        result.failures
    );
}

#[test]
fn sanity_run_with_failures_still_requires_failures_summary() {
    // When a check failed, FAILURES_SUMMARY.md IS generated; if it is missing
    // the timing-output invariant must still catch it.
    let tmp = tempfile::TempDir::new().unwrap();
    write_sanity_run(
        tmp.path(),
        serde_json::json!({
            "timings": [{ "label": "MERGE_GATE + FAILURES_SUMMARY", "secs": 0.1 }]
        }),
    );
    fs::write(
        tmp.path().join("00_summary/MERGE_GATE.json"),
        r#"{"decision":{}}"#,
    )
    .unwrap();
    let quality = tmp.path().join("20_quality");
    fs::create_dir_all(&quality).unwrap();
    fs::write(quality.join("clippy.result.json"), r#"{"status":"failed"}"#).unwrap();

    let result = run_sanity_checks(tmp.path()).unwrap();
    assert!(
        result
            .failures
            .iter()
            .any(|f| f.contains("FAILURES_SUMMARY.md")),
        "failed run with missing FAILURES_SUMMARY.md must be flagged, got: {:?}",
        result.failures
    );
}

#[test]
fn sanity_flags_generated_context_artifact_with_missing_file() {
    let tmp = tempfile::TempDir::new().unwrap();
    write_sanity_run(
        tmp.path(),
        serde_json::json!({
            "context_artifacts": [{
                "key": "loctree",
                "path": "30_context/loctree.json",
                "generated": true,
                "recommended": true,
                "reason": "",
            }]
        }),
    );

    let result = run_sanity_checks(tmp.path()).unwrap();
    assert!(
        result
            .failures
            .iter()
            .any(|f| f.contains("loctree") && f.contains("loctree.json")),
        "generated-but-missing artifact must be flagged, got: {:?}",
        result.failures
    );
}

// ── run_sanity_checks: gate_status_consistency (PR #13 review, thread 3512836502) ──

/// Build a pack whose gate claims `passed` while its log carries a hard-fail
/// signature — the contradiction gate_status_consistency must catch.
fn write_gate_status_mismatch_pack(out: &Path) {
    let summary = out.join("00_summary");
    fs::create_dir_all(&summary).unwrap();
    fs::write(summary.join("system_meta.txt"), "meta").unwrap();
    fs::write(summary.join("git_meta.txt"), "git").unwrap();
    fs::write(summary.join("MANIFEST.json"), r#"{"files":[]}"#).unwrap();
    fs::write(summary.join("RUN.json"), r#"{"checks":[]}"#).unwrap();
    let quality = out.join("20_quality");
    fs::create_dir_all(&quality).unwrap();
    fs::write(quality.join("clippy.result.json"), r#"{"status":"passed"}"#).unwrap();
    fs::write(
        quality.join("clippy.log"),
        "thread 'main' panicked at src/x.rs:1:1:\nboom\n",
    )
    .unwrap();
}

#[test]
fn sanity_gate_status_consistency_catches_passed_gate_with_failing_log() {
    let tmp = tempfile::TempDir::new().unwrap();
    write_gate_status_mismatch_pack(tmp.path());
    let result = run_sanity_checks(tmp.path()).unwrap();
    assert!(
        result
            .failures
            .iter()
            .any(|f| f.contains("clippy") && f.contains("hard fail signatures")),
        "passed gate with a hard-fail log must be flagged, got: {:?}",
        result.failures
    );
}

#[test]
fn sanity_gate_status_consistency_reads_logs_under_relative_out_dir() {
    // Regression: with a RELATIVE out_dir (`prview -o ./rel`, config passes it
    // through un-canonicalized), passing an absolute / re-prefixed `requested`
    // to read_to_string_within double-prefixed the path, so the log was never
    // read and this check silently failed open. It must now read the log and
    // catch the same contradiction the absolute case catches.
    //
    // Use a RELATIVE path under `target/` (git-ignored, safe) instead of
    // mutating the process-global cwd — chdir in a parallel suite is flaky and
    // can break any concurrently-running test that resolves relative paths.
    let rel = Path::new("target/tmp_sanity_relative_out_dir");
    if rel.exists() {
        fs::remove_dir_all(rel).unwrap();
    }
    write_gate_status_mismatch_pack(rel);
    let result = run_sanity_checks(rel);
    let _ = fs::remove_dir_all(rel);
    let result = result.unwrap();
    assert!(
        result
            .failures
            .iter()
            .any(|f| f.contains("clippy") && f.contains("hard fail signatures")),
        "under a relative out_dir the log must still be read and the mismatch flagged, got: {:?}",
        result.failures
    );
}

#[test]
fn standard_review_html_renders_markdown_through_mdrender() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let out = tmp.path();

    let fixture = "\
# Review Summary

Some **bold** narrative with `inline code`.

- [x] done
- [ ] pending

| Check | Status |
|-------|--------|
| Build | Passed |

> [!WARNING]
> Careful here.

```rust
fn main() {
    println!(\"hi\");
}
```

Trailing <script>alert('xss')</script> injection.
";
    fs::write(out.join("REVIEW_SUMMARY.md"), fixture).expect("write REVIEW_SUMMARY.md");

    generate_standard_review_html(out).expect("generate_standard_review_html");
    let html = std::fs::read_to_string(out.join("review.html")).expect("read review.html");

    // Renderer wrapper + scoped stylesheet are present.
    assert!(html.contains("<div class=\"mdr\">"), "mdr wrapper missing");
    assert!(
        html.contains(".mdr .markdown-alert"),
        "mdr stylesheet missing"
    );

    // GFM constructs survive into the export.
    assert!(html.contains("<th>Check</th>"), "table header missing");
    assert!(
        html.contains("contains-task-list"),
        "task-list class missing"
    );
    assert!(
        html.contains("markdown-alert markdown-alert-warning"),
        "warning callout missing"
    );

    // syntect inline highlighting (self-contained span styles).
    assert!(
        html.contains("<span style=\"color:#"),
        "fenced-code highlight spans missing"
    );

    // Sanitization: the injected script must not survive.
    assert!(
        !html.contains("<script"),
        "script tag leaked into review.html"
    );
    assert!(!html.contains("alert('xss')"), "script body leaked");
}

// ── 00_summary/PROVENANCE.json ─────────────────────────────────────────────

fn provenance_fixture_repo() -> (tempfile::TempDir, String) {
    let tmp = tempfile::tempdir().expect("tempdir");
    run_git_fixture(tmp.path(), &["init", "-q", "-b", "main"]);
    let head = write_commit_fixture(tmp.path(), "own.rs", "pub fn own() -> u8 { 1 }\n");
    (tmp, head)
}

fn provenance_check(name: &str, cached: bool, provenance: Option<CheckProvenance>) -> CheckResult {
    CheckResult {
        name: name.to_string(),
        status: CheckStatus::Passed,
        duration: Duration::from_secs(1),
        output: String::new(),
        cached,
        provenance,
    }
}

fn snapshot_provenance(target_sha: &str) -> CheckProvenance {
    CheckProvenance {
        command: "cargo check".to_string(),
        tool_version: None,
        cwd: "[external]/snapshot".to_string(),
        target_sha: Some(target_sha.to_string()),
        tree_state: Some(crate::checks::TreeState::Snapshot),
        exit_code: Some(0),
        started_at: "2026-08-22T10:00:00+02:00".to_string(),
        finished_at: "2026-08-22T10:00:01+02:00".to_string(),
        hard_fail_signatures: vec![],
        cache_key: Some("commit-deadbeef".to_string()),
    }
}

fn write_provenance_fixture(
    repo_root: &Path,
    out: &Path,
    checks: &[CheckResult],
) -> serde_json::Value {
    write_provenance_fixture_with_diffs(repo_root, out, checks, &[])
}

/// Base tip the operator names, distinct from the merge base a diverged branch
/// is actually diffed against.
const PROVENANCE_BASE_TIP: &str = "def5678def5678def5678def5678def5678de";

fn write_provenance_fixture_with_diffs(
    repo_root: &Path,
    out: &Path,
    checks: &[CheckResult],
    diffs: &[Diff],
) -> serde_json::Value {
    write_provenance_fixture_with_skips(repo_root, out, checks, &[], diffs)
}

fn write_provenance_fixture_with_skips(
    repo_root: &Path,
    out: &Path,
    checks: &[CheckResult],
    skipped_checks: &[crate::checks::SkippedCheck],
    diffs: &[Diff],
) -> serde_json::Value {
    let repo = Repository::open(repo_root).expect("open repo");
    let worktree = capture_worktree_provenance(repo_root);
    let resolved_target = ResolvedRef {
        name: "feature/provenance".to_string(),
        commit_id: "abc1234abc1234abc1234abc1234abc1234ab".to_string(),
        is_remote: false,
    };
    let resolved_bases = vec![ResolvedRef {
        name: "origin/main".to_string(),
        commit_id: PROVENANCE_BASE_TIP.to_string(),
        is_remote: true,
    }];

    generate_provenance_json(ProvenanceJsonInput {
        dir: out,
        repo: &repo,
        checks,
        skipped_checks,
        diffs,
        resolved_target: &resolved_target,
        resolved_bases: &resolved_bases,
        worktree_clean: worktree.clean,
        worktree_status_digest: worktree.status_digest.as_deref(),
    })
    .expect("generate_provenance_json");

    serde_json::from_str(
        &fs::read_to_string(out.join("PROVENANCE.json")).expect("read PROVENANCE.json"),
    )
    .expect("parse PROVENANCE.json")
}

#[test]
fn provenance_json_records_pack_level_substrate() {
    let (repo_tmp, head) = provenance_fixture_repo();
    let out = tempfile::tempdir().expect("out tempdir");

    let checks = [
        provenance_check("Cargo check", false, Some(snapshot_provenance("abc1234"))),
        // A cache hit replays the ORIGINAL execution's provenance; only the
        // cached flag separates it from a fresh run.
        provenance_check("Clippy", true, Some(snapshot_provenance("abc1234"))),
        // A check with no provenance at all must still appear, with nulls —
        // silence about a gate is exactly what this file exists to prevent.
        provenance_check("heuristics_loctree", false, None),
    ];

    let json = write_provenance_fixture(repo_tmp.path(), out.path(), &checks);

    assert_eq!(json["schema_version"], "1.0");
    assert_eq!(json["target_sha"], "abc1234abc1234abc1234abc1234abc1234ab");
    assert_eq!(json["base_sha"], "def5678def5678def5678def5678def5678de");
    assert_eq!(json["head_sha"], head);
    assert_eq!(json["worktree"]["clean"], true);
    assert!(
        json["worktree"]["status_digest"]
            .as_str()
            .expect("digest")
            .starts_with("sha256:"),
        "clean tree must still carry a digest of its (empty) status"
    );

    let rows = json["checks"].as_array().expect("checks array");
    assert_eq!(rows.len(), 3, "every check gets a row");

    let cargo = &rows[0];
    assert_eq!(cargo["id"], check_id_from_name("Cargo check"));
    assert_eq!(cargo["cwd"], "[external]/snapshot");
    assert_eq!(cargo["target_sha"], "abc1234");
    assert_eq!(cargo["tree_state"], "snapshot");
    assert_eq!(cargo["started_at"], "2026-08-22T10:00:00+02:00");
    assert_eq!(cargo["cached"], false);

    let clippy = &rows[1];
    assert_eq!(clippy["cached"], true);
    assert_eq!(
        clippy["tree_state"], "snapshot",
        "a cache hit must carry the substrate it was produced on"
    );

    let heuristics = &rows[2];
    assert!(heuristics["cwd"].is_null());
    assert!(heuristics["tree_state"].is_null());
}

#[test]
fn provenance_json_base_sha_is_the_commit_the_diff_used() {
    // Diverged branches: the patch is generated from the merge base, while
    // `resolved_bases` still holds the base TIP the operator named. Recording
    // the tip would name a commit no diff in the pack was computed against.
    let (repo_tmp, _head) = provenance_fixture_repo();
    let out = tempfile::tempdir().expect("out tempdir");
    let merge_base = "1111111111111111111111111111111111111111";

    let diffs = vec![Diff {
        target: "feature/provenance".to_string(),
        base: "origin/main".to_string(),
        target_commit_id: "abc1234abc1234abc1234abc1234abc1234ab".to_string(),
        base_commit_id: merge_base.to_string(),
        files: vec![],
        stats: DiffStats {
            files_changed: 0,
            additions: 0,
            deletions: 0,
            copied: 0,
        },
        commits: vec![],
    }];

    let json = write_provenance_fixture_with_diffs(repo_tmp.path(), out.path(), &[], &diffs);
    assert_eq!(
        json["base_sha"], merge_base,
        "base_sha must name the baseline the patch was produced from",
    );
    assert_ne!(
        json["base_sha"], PROVENANCE_BASE_TIP,
        "the base tip is not what the diff compared against",
    );
}

#[test]
fn provenance_json_records_checks_that_never_ran() {
    // A gate ruled out before it ran — tests disabled, a tool absent — used to
    // vanish from the manifest entirely, leaving a consumer unable to tell a
    // deliberate skip from a check that was never part of this run. The row is
    // all nulls because nothing was read; the reason is what it is there for.
    let (repo_tmp, _head) = provenance_fixture_repo();
    let out = tempfile::tempdir().expect("out tempdir");

    let checks = [provenance_check(
        "Cargo check",
        false,
        Some(snapshot_provenance("abc1234")),
    )];
    let skipped = [crate::checks::SkippedCheck {
        id: "cargo_test".to_string(),
        name: "Cargo test".to_string(),
        reason: "tests disabled".to_string(),
    }];

    let json =
        write_provenance_fixture_with_skips(repo_tmp.path(), out.path(), &checks, &skipped, &[]);

    let rows = json["checks"].as_array().expect("checks array");
    assert_eq!(rows.len(), 2, "a configured gate has a row either way");
    assert!(
        rows[0]["skipped"].is_null(),
        "a check that ran is marked by the absence of a reason",
    );

    let row = &rows[1];
    assert_eq!(row["id"], "cargo_test");
    assert_eq!(row["skipped"], "tests disabled");
    assert!(row["cwd"].is_null(), "a skip read no tree");
    assert!(row["target_sha"].is_null());
    assert!(row["tree_state"].is_null());
    assert!(row["started_at"].is_null());
    assert_eq!(row["cached"], false);
}

#[test]
fn provenance_json_records_every_baseline_of_a_multi_base_run() {
    // `--base a --base b` produces one patch per base, each with its own merge
    // base. Recording only the first left the second patch unplaceable: the
    // pack contains a diff whose baseline the manifest never names.
    let (repo_tmp, _head) = provenance_fixture_repo();
    let out = tempfile::tempdir().expect("out tempdir");

    let diff = |base: &str, base_commit: &str| Diff {
        target: "feature/provenance".to_string(),
        base: base.to_string(),
        target_commit_id: "abc1234abc1234abc1234abc1234abc1234ab".to_string(),
        base_commit_id: base_commit.to_string(),
        files: vec![],
        stats: DiffStats {
            files_changed: 0,
            additions: 0,
            deletions: 0,
            copied: 0,
        },
        commits: vec![],
    };
    let first = "1111111111111111111111111111111111111111";
    let second = "2222222222222222222222222222222222222222";
    let diffs = vec![diff("origin/main", first), diff("origin/release", second)];

    let json = write_provenance_fixture_with_diffs(repo_tmp.path(), out.path(), &[], &diffs);

    let bases = json["bases"].as_array().expect("bases array");
    assert_eq!(bases.len(), 2, "one row per patch in the pack");
    assert_eq!(bases[0]["name"], "origin/main");
    assert_eq!(bases[0]["sha"], first);
    assert_eq!(bases[1]["name"], "origin/release");
    assert_eq!(bases[1]["sha"], second);
    assert_eq!(
        json["base_sha"], first,
        "the scalar stays the first baseline, so older consumers keep reading it",
    );
}

#[test]
fn provenance_json_bases_fall_back_to_resolved_refs_without_a_diff() {
    // No diff at all (`--current-only`, or a base pointing at the target): the
    // resolved refs are then the only baselines there are, and the array must
    // still agree with the scalar.
    let (repo_tmp, _head) = provenance_fixture_repo();
    let out = tempfile::tempdir().expect("out tempdir");

    let json = write_provenance_fixture_with_diffs(repo_tmp.path(), out.path(), &[], &[]);

    let bases = json["bases"].as_array().expect("bases array");
    assert_eq!(bases.len(), 1);
    assert_eq!(bases[0]["name"], "origin/main");
    assert_eq!(bases[0]["sha"], PROVENANCE_BASE_TIP);
    assert_eq!(json["base_sha"], PROVENANCE_BASE_TIP);
}

#[test]
fn provenance_json_worktree_reflects_dirty_tree() {
    let (repo_tmp, _head) = provenance_fixture_repo();
    let out = tempfile::tempdir().expect("out tempdir");

    let clean = write_provenance_fixture(repo_tmp.path(), out.path(), &[]);
    assert_eq!(clean["worktree"]["clean"], true);

    fs::write(repo_tmp.path().join("uncommitted.rs"), "pub fn oops() {}\n").expect("dirty file");

    let dirty = write_provenance_fixture(repo_tmp.path(), out.path(), &[]);
    assert_eq!(
        dirty["worktree"]["clean"], false,
        "an untracked file makes the tree dirty"
    );
    assert_ne!(
        dirty["worktree"]["status_digest"], clean["worktree"]["status_digest"],
        "the digest must fingerprint WHAT is dirty, not just that something is"
    );
}

#[test]
fn worktree_digest_separates_runs_that_differ_only_in_content() {
    // Two runs can dirty exactly the same paths with exactly the same status
    // codes and still have judged different bytes. A status-set-only digest
    // collides there, so the fingerprint promises more than it delivers.
    let (repo_tmp, _head) = provenance_fixture_repo();
    let repo = repo_tmp.path();

    // `own.rs` is the fixture's committed file — editing it yields an `M` entry.
    let tracked = repo.join("own.rs");
    fs::write(&tracked, "pub fn v1() {}\n").expect("write tracked");
    let untracked = repo.join("scratch.rs");
    fs::write(&untracked, "pub fn a() {}\n").expect("write untracked");
    let first = capture_worktree_provenance(repo);

    // Same paths, same `M`/`??` codes — different bytes.
    fs::write(&tracked, "pub fn v2_completely_different() {}\n").expect("rewrite tracked");
    fs::write(&untracked, "pub fn b() {}\n").expect("rewrite untracked");
    let second = capture_worktree_provenance(repo);

    assert_eq!(first.clean, second.clean, "both runs are dirty");
    assert_ne!(
        first.status_digest, second.status_digest,
        "differently-dirty runs must be distinguishable, which is what the digest claims",
    );

    // Restoring the exact bytes restores the exact fingerprint: the digest is a
    // function of the tree, not of time or run order.
    fs::write(&tracked, "pub fn v1() {}\n").expect("restore tracked");
    fs::write(&untracked, "pub fn a() {}\n").expect("restore untracked");
    assert_eq!(
        capture_worktree_provenance(repo).status_digest,
        first.status_digest,
        "the same tree state must fingerprint identically",
    );
}

/// A nested repository is ONE status entry: git never recurses into another
/// repository, so the digest saw only "a directory is there". Two very different
/// nested trees — a submodule sitting on another commit, or carrying edits —
/// fingerprinted identically, which is exactly the collision the content digest
/// exists to prevent.
#[test]
fn worktree_digest_separates_nested_repositories_by_their_own_state() {
    let (repo_tmp, _head) = provenance_fixture_repo();
    let repo = repo_tmp.path();

    let nested = repo.join("vendor");
    fs::create_dir_all(&nested).expect("nested dir");
    run_git_fixture(&nested, &["init", "-q", "-b", "main"]);
    write_commit_fixture(&nested, "lib.rs", "pub fn v1() {}\n");
    let first = capture_worktree_provenance(repo);

    // The nested repository moves to another commit; the superproject sees the
    // same single entry with the same status code.
    write_commit_fixture(&nested, "lib.rs", "pub fn v2() {}\n");
    let moved = capture_worktree_provenance(repo);
    assert_ne!(
        first.status_digest, moved.status_digest,
        "a nested repository on another commit is another substrate",
    );

    // Uncommitted work inside it does not move its HEAD, and must still count.
    fs::write(nested.join("scratch.rs"), "pub fn draft() {}\n").expect("nested edit");
    let dirty = capture_worktree_provenance(repo);
    assert_ne!(
        moved.status_digest, dirty.status_digest,
        "edits inside a nested repository change what a scan would read",
    );

    // Same HEAD, same dirty paths, different bytes: a clean/dirty flag says
    // these are the same substrate, and cargo (or any other check that compiles
    // the vendored tree) reads different code in each.
    fs::write(nested.join("scratch.rs"), "pub fn something_else() {}\n").expect("nested rewrite");
    let differently_dirty = capture_worktree_provenance(repo);
    assert_ne!(
        dirty.status_digest, differently_dirty.status_digest,
        "a nested repository dirtied differently is a different substrate",
    );

    // And it is still a function of the tree: restoring the bytes restores the
    // fingerprint.
    fs::write(nested.join("scratch.rs"), "pub fn draft() {}\n").expect("nested restore");
    assert_eq!(
        capture_worktree_provenance(repo).status_digest,
        dirty.status_digest,
        "the same nested tree must fingerprint identically",
    );
}
