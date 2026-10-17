//! Pure synthetic dispatch controls: no external commands or scanners.
use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

pub(crate) fn fixture() -> (tempfile::TempDir, tempfile::TempDir, Config) {
    let parent = std::env::temp_dir().join("caller-guard-fixtures");
    std::fs::create_dir_all(&parent).unwrap();
    let root = tempfile::tempdir_in(&parent).unwrap();
    let home = tempfile::tempdir_in(&parent).unwrap();
    let mut options = git2::RepositoryInitOptions::new();
    options.initial_head("main");
    let repo = git2::Repository::init_opts(root.path(), &options).unwrap();
    std::fs::write(root.path().join("source.txt"), "original\n").unwrap();
    std::fs::write(root.path().join(".gitignore"), ".generated/\n").unwrap();
    let mut index = repo.index().unwrap();
    index.add_path(Path::new("source.txt")).unwrap();
    index.add_path(Path::new(".gitignore")).unwrap();
    index.write().unwrap();
    let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
    let sig = git2::Signature::now("Fixture", "fixture@example.invalid").unwrap();
    repo.commit(Some("HEAD"), &sig, &sig, "fixture", &tree, &[])
        .unwrap();
    let mut config = crate::config::test_config();
    config.repo_root = root.path().to_path_buf();
    config.target = None;
    config.pinned_target = None;
    config.pinned_diff_bases = None;
    config.quiet = true;
    config.json = true;
    (root, home, config)
}

struct Mock {
    name: &'static str,
    mutation: Option<&'static str>,
    ran: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl Check for Mock {
    fn name(&self) -> &str {
        self.name
    }
    fn check_eligibility(&self, _: &Config) -> CheckEligibility {
        CheckEligibility::Run
    }
    fn cache_key(&self, _: &Config) -> Option<String> {
        Some(self.name.into())
    }
    async fn run(&self, config: &Config) -> Result<CheckResult> {
        self.ran.fetch_add(1, Ordering::SeqCst);
        match self.mutation {
            Some("tracked") => std::fs::write(config.repo_root.join("source.txt"), "modified\n")?,
            Some("untracked") => {
                std::fs::write(config.repo_root.join("new-source.txt"), "new source\n")?
            }
            Some("index") => {
                let repo = git2::Repository::open(&config.repo_root)?;
                let mut index = repo.index()?;
                index.remove_path(Path::new("source.txt"))?;
                index.write()?;
            }
            Some("head") => git2::Repository::open(&config.repo_root)?.set_head_detached(
                git2::Repository::open(&config.repo_root)?
                    .head()?
                    .target()
                    .unwrap(),
            )?,
            Some("ignored") => {
                std::fs::create_dir(config.repo_root.join(".generated"))?;
                std::fs::write(config.repo_root.join(".generated/output"), "generated\n")?;
            }
            _ => {}
        }
        Ok(CheckResult {
            name: self.name.into(),
            status: CheckStatus::Passed,
            duration: Duration::ZERO,
            output: "mock".into(),
            cached: false,
            provenance: None,
        })
    }
}

async fn run_mock_pair(
    config: &Config,
    home: &Path,
    mutation: Option<&'static str>,
    events: bool,
) -> (
    Result<(Vec<CheckResult>, Vec<SkippedCheck>)>,
    usize,
    usize,
    bool,
) {
    let first = Arc::new(AtomicUsize::new(0));
    let queued = Arc::new(AtomicUsize::new(0));
    let cache = Cache::with_dir(home.join("cache"), true);
    let probe = Cache::with_dir(home.join("cache"), true);
    let checks: Vec<Box<dyn Check>> = vec![
        Box::new(Mock {
            name: "Guard writer",
            mutation,
            ran: Arc::clone(&first),
        }),
        Box::new(Mock {
            name: "Guard queued",
            mutation: None,
            ran: Arc::clone(&queued),
        }),
    ];
    let ledger = TaskLedger::new();
    let governor = Arc::new(ResourceGovernor::with_budget(1, 1));
    let result = if events {
        run_all_checks_with_events(checks, cache, config, &ledger, &governor, |_| {}).await
    } else {
        run_all_checks(checks, cache, config, &ledger, &governor).await
    };
    (
        result,
        first.load(Ordering::SeqCst),
        queued.load(Ordering::SeqCst),
        probe.get("Guard writer", "Guard writer").is_some(),
    )
}

#[tokio::test]
async fn issue69_caller_guard_headless_refuses_drift_and_queued_cache() {
    for mutation in ["tracked", "untracked", "index", "head"] {
        let (_root, home, config) = fixture();
        let _home = crate::config::override_test_prview_home(home.path());
        let (outcome, first, queued, cached) =
            run_mock_pair(&config, home.path(), Some(mutation), false).await;
        assert!(
            outcome.is_err(),
            "caller {mutation} drift must abort dispatch"
        );
        assert_eq!(first, 1);
        assert_eq!(
            queued, 0,
            "queued check must never start after observed drift"
        );
        assert!(!cached, "a source-mutating result must not enter cache");
        let text = outcome.unwrap_err().to_string();
        assert!(text.contains("caller drift"), "{text}");
        assert!(
            home.path().join("diagnostics").is_dir(),
            "durable evidence outside a normal pack"
        );
        assert!(!home.path().join("index.jsonl").exists());
        assert!(!home.path().join("runs").exists());
    }
}

#[tokio::test]
async fn issue69_caller_guard_event_dispatcher_refuses_drift() {
    let (_root, home, config) = fixture();
    let _home = crate::config::override_test_prview_home(home.path());
    let (outcome, _, queued, cached) =
        run_mock_pair(&config, home.path(), Some("tracked"), true).await;
    assert!(
        outcome.is_err(),
        "TUI must propagate source drift before pack publication"
    );
    assert_eq!(queued, 0);
    assert!(!cached);
}

#[tokio::test]
async fn issue69_caller_guard_allows_initial_dirty_and_ignored_output() {
    let (root, home, config) = fixture();
    std::fs::write(root.path().join("source.txt"), "initial dirt\n").unwrap();
    std::fs::write(root.path().join("untracked.txt"), "initial source\n").unwrap();
    let _home = crate::config::override_test_prview_home(home.path());
    let (outcome, first, queued, _) =
        run_mock_pair(&config, home.path(), Some("ignored"), false).await;
    assert!(
        outcome.is_ok(),
        "initial dirty source and Git-ignored output are legitimate"
    );
    assert_eq!((first, queued), (1, 1));
}

#[tokio::test]
async fn issue69_caller_guard_latched_check_is_never_executed() {
    let (root, home, config) = fixture();
    let _home = crate::config::override_test_prview_home(home.path());
    let ledger = TaskLedger::new();
    ledger
        .initialize_caller_guard(&config.repo_root)
        .await
        .unwrap();
    std::fs::write(root.path().join("source.txt"), "modified\n").unwrap();
    assert!(
        ledger
            .verify_caller("mock-observation", None)
            .await
            .is_err()
    );
    let ran = Arc::new(AtomicUsize::new(0));
    let cache = Cache::with_dir(home.path().join("cache"), true);
    let result = execute_live_check(
        Box::new(Mock {
            name: "Guard delayed",
            mutation: None,
            ran: Arc::clone(&ran),
        }),
        &config,
        &cache,
        Some(&ledger),
    )
    .await;
    assert_eq!(ran.load(Ordering::SeqCst), 0);
    assert!(result.provenance.is_none());
    assert!(ledger.ensure_caller_valid().is_err());
}

#[tokio::test]
async fn issue69_caller_guard_fatal_dispatch_drops_active_mock_future() {
    struct DropMarker(Arc<AtomicUsize>);
    impl Drop for DropMarker {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
    struct ParallelMock {
        active: bool,
        ready: Arc<tokio::sync::Notify>,
        dropped: Arc<AtomicUsize>,
    }
    #[async_trait::async_trait]
    impl Check for ParallelMock {
        fn name(&self) -> &str {
            if self.active {
                "Guard active"
            } else {
                "Guard parallel writer"
            }
        }
        fn check_eligibility(&self, _: &Config) -> CheckEligibility {
            CheckEligibility::Run
        }
        fn resource_weight(&self) -> crate::governor::Weight {
            crate::governor::Weight::Light
        }
        async fn run(&self, config: &Config) -> Result<CheckResult> {
            if self.active {
                let _drop = DropMarker(Arc::clone(&self.dropped));
                self.ready.notify_one();
                std::future::pending::<()>().await;
                unreachable!();
            }
            self.ready.notified().await;
            std::fs::write(config.repo_root.join("source.txt"), "modified\n")?;
            Ok(CheckResult {
                name: self.name().into(),
                status: CheckStatus::Passed,
                duration: Duration::ZERO,
                output: "mock".into(),
                cached: false,
                provenance: None,
            })
        }
    }
    let (_root, home, config) = fixture();
    let _home = crate::config::override_test_prview_home(home.path());
    let ready = Arc::new(tokio::sync::Notify::new());
    let dropped = Arc::new(AtomicUsize::new(0));
    let checks: Vec<Box<dyn Check>> = [true, false]
        .into_iter()
        .map(|active| {
            Box::new(ParallelMock {
                active,
                ready: Arc::clone(&ready),
                dropped: Arc::clone(&dropped),
            }) as Box<dyn Check>
        })
        .collect();
    let ledger = TaskLedger::new();
    let governor = Arc::new(ResourceGovernor::with_budget(2, 1));
    let cache = Cache::with_dir(home.path().join("cache"), false);
    let outcome = tokio::time::timeout(
        Duration::from_secs(2),
        run_all_checks(checks, cache, &config, &ledger, &governor),
    )
    .await
    .unwrap();
    assert!(outcome.is_err());
    assert!(
        !governor.is_cancelled(),
        "source drift is not operator cancellation"
    );
    assert_eq!(
        dropped.load(Ordering::SeqCst),
        1,
        "fatal Result drops the active future"
    );
}

#[tokio::test]
async fn issue69_caller_guard_internal_storage_refused_before_mock_or_cache() {
    for events in [false, true] {
        let (root, home, mut config) = fixture();
        let storage = root.path().join("new-home");
        config.output_dir = Some(home.path().join("external-output"));
        let _home = crate::config::override_test_prview_home(&storage);
        let (outcome, first, queued, cached) = run_mock_pair(&config, &storage, None, events).await;
        assert!(outcome.unwrap_err().to_string().contains("variant A"));
        assert_eq!((first, queued), (0, 0));
        assert!(!cached);
        assert!(
            !storage.exists(),
            "refusal precedes cache/diagnostic writes"
        );
        assert!(!config.output_dir.as_ref().unwrap().exists());
        assert!(caller_integrity::validate_storage_namespaces(&config).is_err());
    }
}
