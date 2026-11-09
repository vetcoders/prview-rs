//! Explicit consent is independent of profiles, project files and old results.
use clap::Parser;
use prview::checks::{Check, CheckStatus, PROJECT_EXECUTION_NOT_AUTHORIZED};
use std::path::Path;

fn fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join(".gitignore"), "/target/\n").unwrap();
    std::fs::write(
        dir.path().join("Cargo.lock"),
        "version = 4\n\n[[package]]\nname = \"consent-fixture\"\nversion = \"0.0.0\"\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("Cargo.toml"),
        "[package]\nname='consent-fixture'\nversion='0.0.0'\nedition='2021'\n",
    )
    .unwrap();
    std::fs::create_dir(dir.path().join("src")).unwrap();
    std::fs::write(
        dir.path().join("src/lib.rs"),
        "pub fn value() -> u8 { 1 }\n",
    )
    .unwrap();
    commit(dir.path());
    dir
}

fn commit(root: &Path) {
    let repo = git2::Repository::init(root).unwrap();
    let mut index = repo.index().unwrap();
    index
        .add_all(["*"], git2::IndexAddOption::DEFAULT, None)
        .unwrap();
    index.write().unwrap();
    let oid = index.write_tree().unwrap();
    let tree = repo.find_tree(oid).unwrap();
    let sig = git2::Signature::now("fixture", "fixture@example.invalid").unwrap();
    let parent = repo.head().ok().and_then(|h| h.peel_to_commit().ok());
    let parents: Vec<_> = parent.iter().collect();
    repo.commit(Some("HEAD"), &sig, &sig, "fixture", &tree, &parents)
        .unwrap();
}

#[test]
fn project_execution_cli_requires_the_explicit_flag() {
    for args in [
        vec!["prview"],
        vec!["prview", "--deep"],
        vec!["prview", "--quick"],
        vec!["prview", "--full-tests"],
        vec!["prview", "--remote-only"],
    ] {
        assert!(
            !prview::Cli::try_parse_from(args)
                .unwrap()
                .allow_project_execution
        );
    }
    for args in [
        vec!["prview", "--allow-project-execution", "--deep"],
        vec!["prview", "fix", "--allow-project-execution"],
        vec!["prview", "gate", "--allow-project-execution"],
    ] {
        assert!(
            prview::Cli::try_parse_from(args)
                .unwrap()
                .allow_project_execution
        );
    }
}

#[tokio::test]
async fn project_execution_direct_checks_default_to_not_run() {
    use prview::checks::*;
    let dir = fixture();
    // Project files cannot grant authority, including the merge policy.
    std::fs::write(
        dir.path().join("prview.toml"),
        "allow_project_execution = true\n",
    )
    .unwrap();
    let config = prview::Config::for_state_viewer(dir.path()).unwrap();
    assert!(!config.allow_project_execution);
    let checks: Vec<Box<dyn Check>> = vec![
        Box::new(CargoCheck),
        Box::new(ClippyCheck),
        Box::new(CargoTestCheck),
        Box::new(RustfmtCheck),
        Box::new(CargoAuditCheck),
        Box::new(CargoGeigerCheck),
        Box::new(TypeScriptCheck),
        Box::new(ESLintCheck),
        Box::new(StylelintCheck),
        Box::new(VitestCheck),
        Box::new(RuffCheck),
        Box::new(MypyCheck),
        Box::new(PytestCheck),
        Box::new(SemgrepCheck),
    ];
    for check in checks {
        let result = check.run(&config).await.unwrap();
        assert_eq!(result.status, CheckStatus::Skipped, "{}", check.name());
        assert_eq!(result.output, PROJECT_EXECUTION_NOT_AUTHORIZED);
        assert!(result.provenance.is_none());
        assert!(!result.cached);
    }
    assert!(!dir.path().join("target").exists());
}

#[tokio::test]
async fn project_execution_both_dispatchers_skip_before_cache_or_scope() {
    let dir = fixture();
    let mut config = prview::Config::for_state_viewer(dir.path()).unwrap();
    config.run_tests = true;
    config.run_lint = true;
    config.run_security = true;
    config.quiet = true;
    let app = prview::App::from_config(config.clone()).unwrap();
    for events in [false, true] {
        let ledger = prview::ledger::TaskLedger::new();
        let (results, skipped) = if events {
            prview::checks::run_all_with_events(&config, &ledger, &app.governor(), |_| {})
                .await
                .unwrap()
        } else {
            prview::checks::run_all(&config, &ledger, &app.governor())
                .await
                .unwrap()
        };
        assert!(results.is_empty());
        assert!(!skipped.is_empty());
        assert!(
            skipped
                .iter()
                .all(|s| s.reason == PROJECT_EXECUTION_NOT_AUTHORIZED)
        );
        assert!(!dir.path().join("target").exists());
    }
}

#[cfg(unix)]
#[test]
fn project_execution_state_does_not_invoke_git_filters() {
    use std::os::unix::fs::PermissionsExt;
    let dir = fixture();
    let marker = dir.path().join("marker");
    let filter = dir.path().join("filter");
    std::fs::write(
        &filter,
        format!("#!/bin/sh\nprintf x >> '{}'\ncat\n", marker.display()),
    )
    .unwrap();
    std::fs::set_permissions(&filter, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(
        dir.path().join(".gitattributes"),
        "src/lib.rs filter=fixture\n",
    )
    .unwrap();
    git2::Repository::open(dir.path())
        .unwrap()
        .config()
        .unwrap()
        .set_str("filter.fixture.clean", filter.to_str().unwrap())
        .unwrap();
    std::fs::write(dir.path().join("src/lib.rs"), "pub fn changed() {}\n").unwrap();
    let error = prview::state::collect_state(
        dir.path(),
        &prview::state::StateOpts {
            fast: false,
            json: true,
            hot: true,
        },
    )
    .unwrap_err();
    assert!(error.to_string().contains("filter=fixture"), "{error}");
    assert!(!marker.exists());
}

/// Native runtime acceptance: deliberately not ignored when the harness cannot
/// establish process ownership. Such a failure is an environment gap, not PASS.
#[tokio::test]
async fn project_execution_authorized_cargo_build_script_runs_once() {
    let dir = fixture();
    let owned = tempfile::tempdir().unwrap();
    let marker = owned.path().join("build-marker");
    std::fs::write(dir.path().join("build.rs"), format!("fn main() {{ use std::io::Write; let mut f = std::fs::OpenOptions::new().create(true).append(true).open({:?}).unwrap(); writeln!(f, \"executed\").unwrap(); }}", marker)).unwrap();
    let mut config = prview::Config::for_state_viewer(dir.path()).unwrap();
    let check = prview::checks::CargoCheck;
    assert_eq!(
        check.run(&config).await.unwrap().status,
        CheckStatus::Skipped
    );
    assert!(!marker.exists());
    config.allow_project_execution = true;
    let result = check.run(&config).await.unwrap();
    assert_eq!(result.status, CheckStatus::Passed, "{}", result.output);
    assert_eq!(std::fs::read_to_string(marker).unwrap().lines().count(), 1);
    assert!(result.provenance.is_some());
}

#[test]
fn project_execution_cli_optin_after_inert_pack_runs_the_owned_build_once() {
    let dir = fixture();
    let owned = tempfile::tempdir().unwrap();
    let marker = owned.path().join("build-marker");
    std::fs::write(dir.path().join("build.rs"), format!("fn main() {{ use std::io::Write; let mut f = std::fs::OpenOptions::new().create(true).append(true).open({:?}).unwrap(); writeln!(f, \"executed\").unwrap(); }}", marker)).unwrap();
    commit(dir.path());
    for allowed in [false, true] {
        let pack = owned.path().join(if allowed { "allowed" } else { "inert" });
        let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_prview"));
        command.arg("HEAD");
        command
            .current_dir(dir.path())
            .env("PRVIEW_HOME", owned.path().join("home"))
            .args([
                "--quick",
                "--skip-security",
                "--no-heuristics",
                "--no-fetch",
                "--no-zip",
                "--json",
                "--quiet",
                "--output-dir",
            ])
            .arg(&pack);
        if allowed {
            command.arg("--allow-project-execution");
        }
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let summary: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        let run: serde_json::Value =
            serde_json::from_slice(&std::fs::read(pack.join("00_summary/RUN.json")).unwrap())
                .unwrap();
        assert_eq!(run["project_execution"]["allowed"], allowed);
        if allowed {
            assert_eq!(std::fs::read_to_string(&marker).unwrap().lines().count(), 1);
            let result: serde_json::Value = serde_json::from_slice(
                &std::fs::read(pack.join("20_quality/cargo.result.json")).unwrap(),
            )
            .unwrap();
            assert_eq!(result["status"], "passed");
        } else {
            assert!(!marker.exists());
            assert_eq!(summary["allow_merge"], false);
            assert_eq!(summary["analysis_status"], "incomplete");
        }
    }
}

#[cfg(unix)]
#[test]
fn project_execution_snapshot_never_executes_checkout_drivers() {
    use std::os::unix::fs::PermissionsExt;
    let dir = fixture();
    let owned = tempfile::tempdir().unwrap();
    let marker = owned.path().join("marker");
    let filter = owned.path().join("filter");
    std::fs::write(
        &filter,
        format!("#!/bin/sh\nprintf x >> '{}'\ncat\n", marker.display()),
    )
    .unwrap();
    std::fs::set_permissions(&filter, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(
        dir.path().join(".gitattributes"),
        "src/lib.rs filter=fixture\n",
    )
    .unwrap();
    commit(dir.path());
    let repo = git2::Repository::open(dir.path()).unwrap();
    repo.config()
        .unwrap()
        .set_str("filter.fixture.smudge", filter.to_str().unwrap())
        .unwrap();
    let snapshot = prview::git::create_worktree_snapshot(dir.path(), "HEAD").unwrap();
    assert_eq!(
        std::fs::read(snapshot.worktree_path.join("src/lib.rs")).unwrap(),
        std::fs::read(dir.path().join("src/lib.rs")).unwrap()
    );
    let readback = git2::Repository::open(&snapshot.worktree_path).unwrap();
    assert!(readback.statuses(None).unwrap().is_empty());
    assert!(!marker.exists());
}

#[test]
fn project_execution_state_preserves_staged_rename_accounting() {
    let dir = fixture();
    std::fs::rename(
        dir.path().join("src/lib.rs"),
        dir.path().join("src/renamed.rs"),
    )
    .unwrap();
    let repo = git2::Repository::open(dir.path()).unwrap();
    let mut index = repo.index().unwrap();
    index.remove_path(Path::new("src/lib.rs")).unwrap();
    index.add_path(Path::new("src/renamed.rs")).unwrap();
    index.write().unwrap();
    let state = prview::state::collect_state(
        dir.path(),
        &prview::state::StateOpts {
            fast: false,
            json: true,
            hot: true,
        },
    )
    .unwrap();
    assert_eq!(state.files_changed, 1);
    assert_eq!(state.insertions, 0);
    assert_eq!(state.deletions, 0);
    assert!(state.hot_files.is_empty());
}
