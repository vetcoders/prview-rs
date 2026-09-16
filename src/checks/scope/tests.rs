//! Unit tests for the test-scope decision.
//!
//! Everything here is pure: no `cargo metadata` is spawned, no test runner is
//! invoked, and no filesystem is read. The workspace fixtures are the JSON
//! `cargo metadata --no-deps` emits, so the parser is exercised against the
//! real document shape rather than a hand-made struct.

use super::*;
use crate::config::{DetectedProfile, ProfileKind};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn package(name: &str, dir: &str, deps: &[(&str, Option<&str>)], proc_macro: bool) -> String {
    let dependencies = deps
        .iter()
        .map(|(dep, path)| match path {
            Some(path) => format!(r#"{{"name":"{dep}","path":"{path}"}}"#),
            None => format!(r#"{{"name":"{dep}"}}"#),
        })
        .collect::<Vec<_>>()
        .join(",");
    let kind = if proc_macro { "proc-macro" } else { "lib" };
    format!(
        r#"{{"name":"{name}","manifest_path":"{dir}/Cargo.toml",
            "targets":[{{"kind":["{kind}"],"name":"{name}"}}],
            "dependencies":[{dependencies}]}}"#
    )
}

fn metadata(packages: &[String]) -> Vec<u8> {
    format!(
        r#"{{"packages":[{}],"workspace_root":"/w"}}"#,
        packages.join(",")
    )
    .into_bytes()
}

fn workspace(packages: &[String]) -> CargoWorkspace {
    CargoWorkspace::from_metadata_json(&metadata(packages)).expect("workspace parses")
}

fn profile(js: bool, cargo: bool) -> DetectedProfile {
    DetectedProfile {
        kind: ProfileKind::Mixed,
        has_package_json: js,
        has_tsconfig: js,
        has_cargo: cargo,
        has_pyproject: false,
        has_python_source: false,
        has_js_source: js,
        cargo_root: cargo.then(|| std::path::PathBuf::from("/w")),
        rust_dirs: Vec::new(),
        is_workspace: cargo,
    }
}

fn changed(path: &str, status: FileStatus) -> ChangedPath {
    ChangedPath {
        path: path.to_string(),
        status,
        old_path: None,
    }
}

fn modified(path: &str) -> ChangedPath {
    changed(path, FileStatus::Modified)
}

fn trustworthy(paths: Vec<ChangedPath>) -> ChangeSet {
    ChangeSet::new(paths, true)
}

fn never_generated(_: &str) -> bool {
    false
}

/// The repo root the fixtures' member directories descend from. Changed paths
/// are repo-relative, exactly as a diff reports them.
const REPO_ROOT: &str = "/w";

/// The ordinary substrate: the run materialised a snapshot of the reviewed
/// commit, so whatever the operator's own checkout looks like is irrelevant.
fn reviewed_snapshot() -> ReviewedTree {
    ReviewedTree::Snapshot(PathBuf::from(REPO_ROOT))
}

/// A check result carrying whatever evidence of narrowing the check left.
///
/// The report is a statement about EXECUTION, so every reporting test below has
/// to name what the check did, not only what the decision said.
fn result_with(name: &str, status: CheckStatus, executed: Option<ExecutedScope>) -> CheckResult {
    CheckResult {
        name: name.to_string(),
        status,
        duration: std::time::Duration::ZERO,
        output: String::new(),
        cached: false,
        provenance: executed.map(|executed| crate::checks::CheckProvenance {
            command: "cargo test --all-targets --no-fail-fast -p core".to_string(),
            tool_version: None,
            cwd: REPO_ROOT.to_string(),
            target_sha: None,
            tree_state: None,
            exit_code: Some(0),
            executed_scope: Some(executed),
            started_at: String::new(),
            finished_at: String::new(),
            hard_fail_signatures: Vec::new(),
            cache_key: None,
        }),
    }
}

/// A check that ran and left no scope evidence at all — the shape of every
/// check written before this field existed.
fn ran_without_evidence(name: &str) -> CheckResult {
    result_with(name, CheckStatus::Passed, None)
}

fn narrowed(name: &str, selected: usize, selector: &str) -> CheckResult {
    result_with(
        name,
        CheckStatus::Passed,
        Some(ExecutedScope::ChangeScoped {
            selected,
            selector: selector.to_string(),
        }),
    )
}

fn decide_with(
    change_set: Option<&ChangeSet>,
    profile: &DetectedProfile,
    workspace: Option<&Result<CargoWorkspace, WorkspaceError>>,
) -> ScopeDecisions {
    decide_on(change_set, profile, workspace, &reviewed_snapshot())
}

fn decide_on(
    change_set: Option<&ChangeSet>,
    profile: &DetectedProfile,
    workspace: Option<&Result<CargoWorkspace, WorkspaceError>>,
    reviewed_tree: &ReviewedTree,
) -> ScopeDecisions {
    decide_classified(
        change_set,
        profile,
        workspace,
        reviewed_tree,
        &builtin_classifier(),
    )
}

/// The shipped defaults: built-in neutral rules on, no repository patterns.
fn builtin_classifier() -> PathClassifier {
    PathClassifier::new(true, &[]).0
}

fn decide_classified(
    change_set: Option<&ChangeSet>,
    profile: &DetectedProfile,
    workspace: Option<&Result<CargoWorkspace, WorkspaceError>>,
    reviewed_tree: &ReviewedTree,
    classifier: &PathClassifier,
) -> ScopeDecisions {
    decide(
        change_set,
        &ScopeInputs {
            reviewed_tree,
            profile,
            cargo_workspace: workspace,
            is_generated: &never_generated,
            classifier,
        },
    )
}

/// A two-member workspace whose `app` depends on `core` by path.
fn app_and_core() -> Result<CargoWorkspace, WorkspaceError> {
    Ok(workspace(&[
        package("core", "/w/crates/core", &[], false),
        package(
            "app",
            "/w/crates/app",
            &[("core", Some("/w/crates/core"))],
            false,
        ),
    ]))
}

fn full_reason(decision: &ScopeDecision) -> &str {
    match decision {
        ScopeDecision::Full { reason, .. } => reason,
        ScopeDecision::ChangeScoped { .. } => {
            panic!("expected a full run, got a change-scoped selection")
        }
    }
}

fn selected(decision: &ScopeDecision) -> &[String] {
    match decision {
        ScopeDecision::ChangeScoped { selected, .. } => selected,
        ScopeDecision::Full { reason, .. } => {
            panic!("expected a change-scoped selection, got: {reason}")
        }
    }
}

// ---------------------------------------------------------------------------
// File → package mapping
// ---------------------------------------------------------------------------

#[test]
fn the_longest_matching_member_directory_owns_a_file() {
    // A nested member sits inside the outer member's directory, so both are
    // prefixes of the file. Only the longest one is the real owner.
    let ws = workspace(&[
        package("outer", "/w", &[], false),
        package("inner", "/w/crates/inner", &[], false),
    ]);
    assert_eq!(
        ws.package_for_file(Path::new("/w/crates/inner/src/lib.rs")),
        Some("inner")
    );
    assert_eq!(
        ws.package_for_file(Path::new("/w/src/main.rs")),
        Some("outer")
    );
}

#[test]
fn a_file_inside_no_member_has_no_owner() {
    let ws = workspace(&[package("core", "/w/crates/core", &[], false)]);
    assert_eq!(
        ws.package_for_file(Path::new("/elsewhere/src/lib.rs")),
        None
    );
}

#[test]
fn a_sibling_directory_sharing_a_name_prefix_is_not_a_member_match() {
    // `/w/crates/core-extra` must not be attributed to `/w/crates/core`:
    // prefix matching is per path component, not per byte.
    let ws = workspace(&[
        package("core", "/w/crates/core", &[], false),
        package("core-extra", "/w/crates/core-extra", &[], false),
    ]);
    assert_eq!(
        ws.package_for_file(Path::new("/w/crates/core-extra/src/lib.rs")),
        Some("core-extra")
    );
}

#[test]
fn a_member_without_a_package_name_makes_the_whole_mapping_unusable() {
    // `-p` needs a name. A member we cannot name is not a member we can skip
    // quietly — it makes every selection in this workspace unsound.
    let raw = br#"{"packages":[{"manifest_path":"/w/Cargo.toml","targets":[],"dependencies":[]}]}"#;
    assert!(matches!(
        CargoWorkspace::from_metadata_json(raw),
        Err(WorkspaceError::Unparsable(_))
    ));
}

// ---------------------------------------------------------------------------
// Reverse path-dependency graph
// ---------------------------------------------------------------------------

#[test]
fn a_change_reaches_every_path_dependent_transitively() {
    // c → b → a: touching `a` must also select `b` and `c`.
    let ws = workspace(&[
        package("a", "/w/a", &[], false),
        package("b", "/w/b", &[("a", Some("/w/a"))], false),
        package("c", "/w/c", &[("b", Some("/w/b"))], false),
    ]);
    let closure = ws.reverse_closure("a");
    assert_eq!(
        closure.into_iter().collect::<Vec<_>>(),
        vec!["a".to_string(), "b".to_string(), "c".to_string()]
    );
}

#[test]
fn a_dependency_cycle_terminates_instead_of_looping() {
    let ws = workspace(&[
        package("a", "/w/a", &[("b", Some("/w/b"))], false),
        package("b", "/w/b", &[("a", Some("/w/a"))], false),
    ]);
    assert_eq!(
        ws.reverse_closure("a").into_iter().collect::<Vec<_>>(),
        vec!["a".to_string(), "b".to_string()]
    );
}

#[test]
fn a_registry_dependency_creates_no_edge_inside_the_workspace() {
    // Registry deps are resolved from the index; one member changing cannot
    // affect another member through them.
    let ws = workspace(&[
        package("a", "/w/a", &[], false),
        package("b", "/w/b", &[("serde", None), ("a", None)], false),
    ]);
    assert_eq!(
        ws.reverse_closure("a").into_iter().collect::<Vec<_>>(),
        vec!["a".to_string()]
    );
}

/// Falsified on Sztudio 2026-09-15 (cargo 1.93.1): `cargo metadata --no-deps`
/// resolves `dep = { workspace = true }` BEFORE emitting, so an inherited
/// dependency carries the same `path` a directly declared one does. Inherited
/// workspace dependencies therefore need no escalation — the fixture below is
/// exactly the document cargo produced for that workspace.
#[test]
fn an_inherited_workspace_dependency_still_carries_its_path() {
    let ws = workspace(&[
        package("core", "/w/core", &[], false),
        package("app", "/w/app", &[("core", Some("/w/core"))], false),
    ]);
    assert_eq!(
        ws.reverse_closure("core").into_iter().collect::<Vec<_>>(),
        vec!["app".to_string(), "core".to_string()],
        "an inherited path dependency is an edge like any other"
    );
}

#[test]
fn a_renamed_path_dependency_resolves_through_its_directory() {
    // `dep = { package = "core", path = "…" }` declares a different name than
    // the package it points at. The directory is what identifies the member.
    let ws = workspace(&[
        package("core", "/w/core", &[], false),
        package("app", "/w/app", &[("core-alias", Some("/w/core"))], false),
    ]);
    assert_eq!(
        ws.reverse_closure("core").into_iter().collect::<Vec<_>>(),
        vec!["app".to_string(), "core".to_string()]
    );
}

// ---------------------------------------------------------------------------
// Escalation table (contract §6), one case per row
// ---------------------------------------------------------------------------

#[test]
fn a_manifest_or_lockfile_escalates_both_ecosystems() {
    let set = trustworthy(vec![modified("Cargo.lock"), modified("src/lib.rs")]);
    let decisions = decide_with(Some(&set), &profile(true, true), Some(&app_and_core()));
    assert_eq!(
        full_reason(&decisions.cargo),
        reason::manifest("Cargo.lock")
    );
    assert_eq!(
        full_reason(&decisions.vitest),
        reason::manifest("Cargo.lock")
    );
}

#[test]
fn vitest_tooling_config_escalates_only_vitest() {
    let set = trustworthy(vec![
        modified("vitest.config.ts"),
        modified("crates/core/src/lib.rs"),
    ]);
    let decisions = decide_with(Some(&set), &profile(true, true), Some(&app_and_core()));
    assert_eq!(
        full_reason(&decisions.vitest),
        reason::test_tooling_config("vitest.config.ts")
    );
    assert_eq!(
        selected(&decisions.cargo),
        ["app".to_string(), "core".to_string()]
    );
}

#[test]
fn a_tsconfig_counts_as_vitest_tooling_config() {
    let set = trustworthy(vec![modified("packages/ui/tsconfig.build.json")]);
    let decisions = decide_with(Some(&set), &profile(true, true), Some(&app_and_core()));
    assert_eq!(
        full_reason(&decisions.vitest),
        reason::test_tooling_config("packages/ui/tsconfig.build.json")
    );
}

#[test]
fn rust_build_config_escalates_only_cargo() {
    for path in [
        "crates/core/build.rs",
        ".cargo/config.toml",
        "rust-toolchain.toml",
    ] {
        let set = trustworthy(vec![modified(path), modified("src/app.ts")]);
        let decisions = decide_with(Some(&set), &profile(true, true), Some(&app_and_core()));
        assert_eq!(
            full_reason(&decisions.cargo),
            reason::rust_build_config(path)
        );
        assert_eq!(selected(&decisions.vitest), ["src/app.ts".to_string()]);
    }
}

#[test]
fn a_proc_macro_crate_escalates_the_whole_cargo_workspace() {
    let ws = Ok(workspace(&[
        package("macros", "/w/crates/macros", &[], true),
        package("app", "/w/crates/app", &[], false),
    ]));
    let set = trustworthy(vec![modified("crates/macros/src/lib.rs")]);
    let decisions = decide_with(Some(&set), &profile(true, true), Some(&ws));
    assert_eq!(full_reason(&decisions.cargo), reason::proc_macro("macros"));
}

#[test]
fn a_deleted_file_escalates_the_ecosystem_that_owned_it() {
    let set = trustworthy(vec![changed("src/gone.ts", FileStatus::Deleted)]);
    let decisions = decide_with(Some(&set), &profile(true, true), Some(&app_and_core()));
    assert_eq!(
        full_reason(&decisions.vitest),
        reason::removed_or_renamed("src/gone.ts")
    );
    // Rust saw no change at all, so it stays scoped — with nothing selected.
    assert!(selected(&decisions.cargo).is_empty());
}

#[test]
fn a_rename_escalates_the_ecosystem_the_content_left() {
    // The content moved from Rust into TypeScript. What became unreachable is
    // the Rust side, so that is the side that must run everything.
    let set = trustworthy(vec![ChangedPath {
        path: "src/app.ts".to_string(),
        status: FileStatus::Renamed,
        old_path: Some("crates/core/src/app.rs".to_string()),
    }]);
    let decisions = decide_with(Some(&set), &profile(true, true), Some(&app_and_core()));
    assert_eq!(
        full_reason(&decisions.cargo),
        reason::removed_or_renamed("crates/core/src/app.rs")
    );
    // The new path is still a legitimate input for its new owner.
    assert_eq!(selected(&decisions.vitest), ["src/app.ts".to_string()]);
}

#[test]
fn more_than_one_diff_base_escalates_both_ecosystems() {
    let set = ChangeSet::new(vec![modified("src/app.ts")], false);
    let decisions = decide_with(Some(&set), &profile(true, true), Some(&app_and_core()));
    assert_eq!(full_reason(&decisions.cargo), reason::MULTIPLE_BASES);
    assert_eq!(full_reason(&decisions.vitest), reason::MULTIPLE_BASES);
}

#[test]
fn no_change_set_at_all_escalates_both_ecosystems() {
    let decisions = decide_with(None, &profile(true, true), Some(&app_and_core()));
    assert_eq!(full_reason(&decisions.cargo), reason::NO_CHANGE_SET);
    assert_eq!(full_reason(&decisions.vitest), reason::NO_CHANGE_SET);
}

/// When the checks read the operator's own checkout — which is what happens on
/// an on-`HEAD` review, where `plan_check_run` hands them the repository root —
/// uncommitted work IS what the tools compile, while the commit-range change
/// set cannot list it. Selecting from a set that is missing files the tools
/// will read is the silent narrowing the contract forbids.
#[test]
fn a_dirty_tree_the_checks_actually_read_escalates_both() {
    let set = trustworthy(vec![modified("src/app.ts")]);
    let decisions = decide_on(
        Some(&set),
        &profile(true, true),
        Some(&app_and_core()),
        &ReviewedTree::LocalDirty(PathBuf::from(REPO_ROOT)),
    );
    assert_eq!(
        full_reason(&decisions.cargo),
        reason::CHECKS_READ_AN_UNCOMMITTED_TREE
    );
    assert_eq!(
        full_reason(&decisions.vitest),
        reason::CHECKS_READ_AN_UNCOMMITTED_TREE
    );
}

#[test]
fn an_unidentifiable_reviewed_tree_escalates_both() {
    let set = trustworthy(vec![modified("src/app.ts")]);
    let decisions = decide_on(
        Some(&set),
        &profile(true, true),
        Some(&app_and_core()),
        &ReviewedTree::Unknown(PathBuf::from(REPO_ROOT)),
    );
    assert_eq!(full_reason(&decisions.cargo), reason::UNKNOWN_REVIEWED_TREE);
    assert_eq!(
        full_reason(&decisions.vitest),
        reason::UNKNOWN_REVIEWED_TREE
    );
}

/// A clean operator checkout IS the reviewed commit, so an on-`HEAD` review
/// that reads it scopes normally. Together with the test above this pins both
/// directions of the distinction.
#[test]
fn a_clean_tree_the_checks_read_scopes_like_a_snapshot() {
    let set = trustworthy(vec![modified("src/app.ts")]);
    let decisions = decide_on(
        Some(&set),
        &profile(true, true),
        Some(&app_and_core()),
        &ReviewedTree::LocalClean(PathBuf::from(REPO_ROOT)),
    );
    assert_eq!(selected(&decisions.vitest), ["src/app.ts".to_string()]);
}

/// `ReviewedTree::resolve` is where the two facts meet: what the ledger says
/// the run materialised, and what the operator's checkout looked like. Only
/// when there is NO snapshot does the checkout enter the picture at all.
#[test]
fn the_reviewed_tree_is_read_from_the_snapshot_first_and_the_checkout_second() {
    use crate::checks::TreeState;
    let repo = Path::new("/repo");
    let snapshot = PathBuf::from("/snap");
    for clean in [Some(true), Some(false), None] {
        assert_eq!(
            ReviewedTree::resolve(
                repo,
                Some(snapshot.clone()),
                Some(TreeState::Snapshot),
                clean
            ),
            ReviewedTree::Snapshot(snapshot.clone()),
            "a materialised snapshot settles the substrate on its own"
        );
    }
    assert_eq!(
        ReviewedTree::resolve(repo, None, None, Some(true)),
        ReviewedTree::LocalClean(repo.to_path_buf())
    );
    assert_eq!(
        ReviewedTree::resolve(repo, None, None, Some(false)),
        ReviewedTree::LocalDirty(repo.to_path_buf())
    );
    assert_eq!(
        ReviewedTree::resolve(repo, None, None, None),
        ReviewedTree::Unknown(repo.to_path_buf()),
        "an unreadable checkout is unknown, never assumed clean"
    );
}

/// The half of the substrate a snapshot's mere existence cannot answer.
///
/// `SnapshotDirty` means the worktree carries bytes the reviewed commit does
/// not — a generated lockfile, a tool that wrote into the checkout — and those
/// bytes are what the compiler and the test runner will read. A change set
/// computed between two commits cannot list them, so a selection drawn from it
/// would be narrower than the tree it is about. `SnapshotBorrowedDeps` is the
/// explicit opposite: only the dependency links came from elsewhere, and the
/// reviewed SOURCE is still exactly the commit, which is all test selection
/// reads.
#[test]
fn a_snapshot_that_no_longer_holds_the_reviewed_commit_is_unknown() {
    use crate::checks::TreeState;
    let repo = Path::new("/repo");
    let snapshot = PathBuf::from("/snap");
    let resolve = |state: Option<TreeState>| {
        ReviewedTree::resolve(repo, Some(snapshot.clone()), state, Some(true))
    };

    assert_eq!(
        resolve(Some(TreeState::SnapshotBorrowedDeps)),
        ReviewedTree::Snapshot(snapshot.clone()),
        "borrowed dependencies leave the reviewed SOURCE exact, which is what selection reads"
    );
    for opaque in [
        Some(TreeState::SnapshotDirty),
        Some(TreeState::Foreign),
        Some(TreeState::LocalDirty),
        // A snapshot on disk whose substrate the run never resolved is a tree
        // nobody has identified; "unknown" is the honest name for it.
        None,
    ] {
        assert_eq!(
            resolve(opaque),
            ReviewedTree::Unknown(snapshot.clone()),
            "{opaque:?} is not evidence the snapshot still holds the reviewed commit"
        );
    }
}

/// And the consequence: an unidentified tree escalates both ecosystems rather
/// than selecting from a change set that may not describe it.
#[test]
fn a_dirty_snapshot_escalates_instead_of_selecting() {
    use crate::checks::TreeState;
    let tree = ReviewedTree::resolve(
        Path::new(REPO_ROOT),
        Some(PathBuf::from(REPO_ROOT)),
        Some(TreeState::SnapshotDirty),
        Some(true),
    );
    let set = trustworthy(vec![modified("crates/core/src/lib.rs")]);
    let decisions = decide_on(
        Some(&set),
        &profile(true, true),
        Some(&app_and_core()),
        &tree,
    );
    assert_eq!(full_reason(&decisions.cargo), reason::UNKNOWN_REVIEWED_TREE);
    assert_eq!(
        full_reason(&decisions.vitest),
        reason::UNKNOWN_REVIEWED_TREE
    );
}

#[test]
fn shared_tooling_escalates_the_ecosystem_that_owns_it() {
    let set = trustworthy(vec![modified("packages/ui/__fixtures__/user.ts")]);
    let decisions = decide_with(Some(&set), &profile(true, true), Some(&app_and_core()));
    assert_eq!(
        full_reason(&decisions.vitest),
        reason::shared_tooling("packages/ui/__fixtures__/user.ts")
    );
    assert!(selected(&decisions.cargo).is_empty());
}

#[test]
fn shared_tooling_with_no_single_owner_escalates_both() {
    // A python helper under `tools/` belongs to neither test runner, and a
    // guess in either direction would be a silent narrowing.
    let set = trustworthy(vec![modified("tools/validate_merge_gate.py")]);
    let decisions = decide_with(Some(&set), &profile(true, true), Some(&app_and_core()));
    let expected = reason::shared_tooling("tools/validate_merge_gate.py");
    assert_eq!(full_reason(&decisions.cargo), expected);
    assert_eq!(full_reason(&decisions.vitest), expected);
}

#[test]
fn escalation_is_one_directional_within_a_run() {
    // The lockfile escalates first; a later, perfectly scopeable Rust file must
    // not pull the run back down to a selection.
    let set = trustworthy(vec![
        modified("Cargo.lock"),
        modified("crates/core/src/lib.rs"),
    ]);
    let decisions = decide_with(Some(&set), &profile(true, true), Some(&app_and_core()));
    assert_eq!(
        full_reason(&decisions.cargo),
        reason::manifest("Cargo.lock")
    );
}

/// The explicit negative case from the contract's correction (2026-09-15).
///
/// A dirty operator checkout is NOT a reason to widen the run. When the run
/// materialised a snapshot, the checks never read that tree at all — the review
/// is about the pinned target and the canonical PR diff — so escalating on it
/// would make the feature useless during exactly the work it exists for. Note
/// the pairing with `a_dirty_tree_the_checks_actually_read_escalates_both`: the
/// same dirty checkout means nothing here and everything there, and the fact
/// that decides which is WHICH TREE THE CHECKS READ.
#[test]
fn a_dirty_operator_checkout_is_not_itself_a_reason_to_escalate() {
    let dirty_checkout_with_a_snapshot = ReviewedTree::resolve(
        Path::new(REPO_ROOT),
        Some(PathBuf::from(REPO_ROOT)),
        Some(crate::checks::TreeState::Snapshot),
        Some(false),
    );
    let set = trustworthy(vec![modified("crates/core/src/lib.rs")]);
    let decisions = decide_on(
        Some(&set),
        &profile(true, true),
        Some(&app_and_core()),
        &dirty_checkout_with_a_snapshot,
    );
    assert_eq!(
        selected(&decisions.cargo),
        ["app".to_string(), "core".to_string()],
        "a trustworthy pinned change set scopes regardless of the operator's tree"
    );
}

// ---------------------------------------------------------------------------
// Shared tooling at the repository root
// ---------------------------------------------------------------------------

/// Substring matching on `"/__fixtures__/"` needs a leading separator a
/// root-level path does not have, so every shared directory at the repository
/// root fell through to "ordinary source" and got SCOPED where the contract
/// demands escalation. One case per directory in the class, all at the root.
#[test]
fn a_shared_directory_at_the_repository_root_still_escalates() {
    for directory in [
        "tools",
        "fixtures",
        "__fixtures__",
        "__mocks__",
        "test-helpers",
        "test_helpers",
        "testutils",
    ] {
        let path = format!("{directory}/user.ts");
        let set = trustworthy(vec![modified(&path)]);
        let decisions = decide_with(Some(&set), &profile(true, true), Some(&app_and_core()));
        assert_eq!(
            full_reason(&decisions.vitest),
            reason::shared_tooling(&path),
            "root-level {directory}/ must escalate like a nested one"
        );
    }
}

#[test]
fn a_shared_directory_nested_anywhere_escalates_too() {
    for path in [
        "packages/ui/__fixtures__/user.ts",
        "crates/core/tests/fixtures/sample.rs",
        "apps/web/src/__mocks__/api.ts",
    ] {
        let set = trustworthy(vec![modified(path)]);
        let decisions = decide_with(Some(&set), &profile(true, true), Some(&app_and_core()));
        assert!(
            decisions.cargo.is_full() || decisions.vitest.is_full(),
            "{path} must escalate its owning ecosystem"
        );
    }
}

/// Whole segments, not substrings: a file whose NAME merely starts with a
/// shared directory's name is ordinary source.
#[test]
fn a_file_named_after_a_shared_directory_is_not_shared_tooling() {
    let set = trustworthy(vec![modified("src/fixtures.ts")]);
    let decisions = decide_with(Some(&set), &profile(true, true), Some(&app_and_core()));
    assert_eq!(selected(&decisions.vitest), ["src/fixtures.ts".to_string()]);
}

// ---------------------------------------------------------------------------
// Unsupported inputs (contract §2: an unknown file ends in a full run)
// ---------------------------------------------------------------------------

/// Neither selector can see these. Vitest walks the static import graph and
/// cargo walks package membership; a JSON asset, a template, an `.env` file or
/// a data file is read at runtime through `fs` or `include_str!`, from a path
/// neither graph contains. Contributing nothing to the selection and calling
/// the result change-scoped would be a silent narrowing.
#[test]
fn an_unsupported_input_escalates_both_ecosystems() {
    for path in [
        "src/locales/pl.json",
        "src/templates/email.hbs",
        ".env.test",
        "src/data/seed.csv",
        "assets/logo.svg",
    ] {
        let set = trustworthy(vec![modified(path)]);
        let decisions = decide_with(Some(&set), &profile(true, true), Some(&app_and_core()));
        assert_eq!(
            full_reason(&decisions.vitest),
            reason::unsupported_input(path),
            "{path} is invisible to the vitest import graph"
        );
        assert_eq!(
            full_reason(&decisions.cargo),
            reason::unsupported_input(path),
            "{path} is invisible to cargo package membership too"
        );
    }
}

#[test]
fn generated_output_is_still_not_an_unsupported_input() {
    // Build output is not an unknown file: it is a KNOWN non-source, and the
    // tools never read it as an input. It neither selects nor escalates.
    let is_generated: &dyn Fn(&str) -> bool = &|path: &str| path.starts_with("dist/");
    let set = trustworthy(vec![modified("dist/locales.json"), modified("src/app.ts")]);
    let decisions = decide(
        Some(&set),
        &ScopeInputs {
            reviewed_tree: &reviewed_snapshot(),
            profile: &profile(true, true),
            cargo_workspace: Some(&app_and_core()),
            is_generated,
            classifier: &builtin_classifier(),
        },
    );
    assert_eq!(selected(&decisions.vitest), ["src/app.ts".to_string()]);
}

// ---------------------------------------------------------------------------
// Three-state classification (contract §6a)
// ---------------------------------------------------------------------------

/// One case per built-in rule. Each of these is something whose content no test
/// runner loads, in any ecosystem, by any mechanism we know of — that is the
/// entire bar for the list, and the reason it is this short.
#[test]
fn every_builtin_rule_names_the_path_it_neutralised() {
    for (path, rule) in [
        ("CHANGELOG.md", "root-changelog"),
        ("CHANGELOG", "root-changelog"),
        ("LICENSE", "root-license"),
        ("LICENCE.txt", "root-license"),
        ("docs/architecture.md", "docs-directory"),
        ("doc/usage.rst", "docs-directory"),
        (".github/workflows/ci.yml", "ci-workflow"),
        (".github/workflows/release.yaml", "ci-workflow"),
    ] {
        let set = trustworthy(vec![modified(path), modified("crates/core/src/lib.rs")]);
        let decisions = decide_with(Some(&set), &profile(true, true), Some(&app_and_core()));
        assert_eq!(
            decisions.non_participating,
            vec![NonParticipatingPath {
                path: path.to_string(),
                rule: rule.to_string(),
            }],
            "{path} must be neutral, and must say which rule decided that"
        );
        assert_eq!(
            selected(&decisions.cargo),
            ["app".to_string(), "core".to_string()],
            "{path} must not disturb the selection the real source produced"
        );
    }
}

/// A root `README` is NOT a built-in neutral, and the reason is concrete rather
/// than cautious: Rust crates pull it into the build with
/// `#![doc = include_str!("../README.md")]`, which puts it in front of
/// `cargo test --doc`. A built-in rule calling it neutral would be a false
/// neutral — a silently missed test — in every repository that does that. A
/// repository whose tests demonstrably never read it can opt it in through
/// `[scope] non_participating`, which is what the override is for.
#[test]
fn a_root_readme_is_not_neutral_by_default() {
    let set = trustworthy(vec![modified("README.md")]);
    let decisions = decide_with(Some(&set), &profile(true, true), Some(&app_and_core()));
    assert!(
        decisions.non_participating.is_empty(),
        "README must not be classified neutral by a built-in rule"
    );
    assert_eq!(
        full_reason(&decisions.cargo),
        reason::unsupported_input("README.md"),
        "README is unknown, and unknown escalates"
    );
    assert_eq!(
        full_reason(&decisions.vitest),
        reason::unsupported_input("README.md")
    );

    // ...and a repository that knows better can say so.
    let (opted_in, _) = PathClassifier::new(true, &["README.md".to_string()]);
    let decisions = decide_classified(
        Some(&set),
        &profile(true, true),
        Some(&app_and_core()),
        &reviewed_snapshot(),
        &opted_in,
    );
    assert_eq!(
        decisions.non_participating,
        vec![NonParticipatingPath {
            path: "README.md".to_string(),
            rule: "README.md".to_string(),
        }]
    );
}

/// The classification is narrow ON PURPOSE. These are the cases the operator
/// named as things we may NOT assume are neutral: they are real runtime or test
/// inputs often enough that "probably fine" is not good enough.
#[test]
fn the_classes_the_operator_excluded_are_still_unknown() {
    for path in [
        // Translations drive runtime behaviour and snapshot assertions — and
        // are compiled straight into the binary often enough that this is not
        // hypothetical: prview itself does it in
        // `artifacts/dashboard/assets.rs` with
        // `include_str!("../../../locales/en.json")`.
        "src/locales/pl.json",
        "locales/en.json",
        "i18n/pl.yaml",
        // Markdown is NOT neutral wholesale — only documentation directories
        // and the named root files are.
        "src/components/Button.md",
        "crates/core/tests/cases/expected.md",
        "notes/design.md",
    ] {
        let set = trustworthy(vec![modified(path)]);
        let decisions = decide_with(Some(&set), &profile(true, true), Some(&app_and_core()));
        assert!(
            decisions.non_participating.is_empty(),
            "{path} must not be classified neutral"
        );
        assert_eq!(
            full_reason(&decisions.vitest),
            reason::unsupported_input(path),
            "{path} is unknown, and unknown still escalates"
        );
    }
}

/// Fixtures and `tools/` are escalating classes from §6 and stay that way: the
/// neutral list is checked AFTER them and never overrides a named reason to
/// widen. A document inside a shared tooling directory is a shared tooling
/// change first.
#[test]
fn shared_tooling_wins_over_the_neutral_list() {
    for path in [
        "tools/README.md",
        "tools/validate_merge_gate.py",
        "fixtures/CHANGELOG.md",
        "packages/ui/__fixtures__/docs.md",
    ] {
        let set = trustworthy(vec![modified(path)]);
        let decisions = decide_with(Some(&set), &profile(true, true), Some(&app_and_core()));
        assert!(
            decisions.non_participating.is_empty(),
            "{path} must not be classified neutral"
        );
        assert_eq!(
            full_reason(&decisions.vitest),
            reason::shared_tooling(path),
            "{path} is shared tooling, which escalates"
        );
    }
}

/// A rename counts as neutral only when BOTH ends are. Moving a source file
/// into `docs/` removes a real input, and the side the content left still has
/// to escalate.
#[test]
fn a_rename_out_of_source_into_a_neutral_path_still_escalates() {
    let set = trustworthy(vec![ChangedPath {
        path: "docs/old-module.md".to_string(),
        status: FileStatus::Renamed,
        old_path: Some("src/module.ts".to_string()),
    }]);
    let decisions = decide_with(Some(&set), &profile(true, true), Some(&app_and_core()));
    assert!(decisions.non_participating.is_empty());
    assert_eq!(
        full_reason(&decisions.vitest),
        reason::removed_or_renamed("src/module.ts")
    );
}

#[test]
fn a_deleted_neutral_path_is_still_neutral() {
    // Deleting a changelog cannot change which tests have to run.
    let set = trustworthy(vec![
        changed("CHANGELOG.md", FileStatus::Deleted),
        modified("crates/core/src/lib.rs"),
    ]);
    let decisions = decide_with(Some(&set), &profile(true, true), Some(&app_and_core()));
    assert_eq!(
        decisions.non_participating,
        vec![NonParticipatingPath {
            path: "CHANGELOG.md".to_string(),
            rule: "root-changelog".to_string(),
        }]
    );
    assert_eq!(
        selected(&decisions.cargo),
        ["app".to_string(), "core".to_string()]
    );
}

/// Recognised source always wins: a repository cannot declare its own
/// TypeScript neutral and quietly stop testing it.
#[test]
fn recognised_source_can_never_be_declared_neutral() {
    let (classifier, rejected) = PathClassifier::new(true, &["src/**".to_string()]);
    assert!(rejected.is_empty());
    assert_eq!(
        classifier.classify("src/app.ts"),
        PathClass::Relevant(Ecosystem::Vitest)
    );
    assert_eq!(
        classifier.classify("src/lib.rs"),
        PathClass::Relevant(Ecosystem::Cargo)
    );
}

#[test]
fn a_repository_can_extend_the_neutral_list_through_its_manifest() {
    let (classifier, rejected) =
        PathClassifier::new(true, &["design/**".to_string(), "*.drawio".to_string()]);
    assert!(rejected.is_empty());
    let set = trustworthy(vec![
        modified("design/wireframe.png"),
        modified("board.drawio"),
        modified("crates/core/src/lib.rs"),
    ]);
    let decisions = decide_classified(
        Some(&set),
        &profile(true, true),
        Some(&app_and_core()),
        &reviewed_snapshot(),
        &classifier,
    );
    assert_eq!(
        decisions.non_participating,
        vec![
            NonParticipatingPath {
                path: "design/wireframe.png".to_string(),
                rule: "design/**".to_string(),
            },
            NonParticipatingPath {
                path: "board.drawio".to_string(),
                rule: "*.drawio".to_string(),
            },
        ],
        "a repo-declared rule is reported by the pattern that matched"
    );
    assert_eq!(
        selected(&decisions.cargo),
        ["app".to_string(), "core".to_string()]
    );
}

#[test]
fn a_repository_can_disable_the_builtin_list_and_get_strict_escalation_back() {
    let strict = PathClassifier::strict();
    let set = trustworthy(vec![modified("CHANGELOG.md")]);
    let decisions = decide_classified(
        Some(&set),
        &profile(true, true),
        Some(&app_and_core()),
        &reviewed_snapshot(),
        &strict,
    );
    assert!(decisions.non_participating.is_empty());
    assert_eq!(
        full_reason(&decisions.vitest),
        reason::unsupported_input("CHANGELOG.md"),
        "with the built-ins off, a changelog is just another unknown file"
    );

    // Disabling the built-ins leaves a repository's own patterns in force —
    // that is the difference between "off" and "empty".
    let (own_rules_only, _) = PathClassifier::new(false, &["CHANGELOG.md".to_string()]);
    let decisions = decide_classified(
        Some(&set),
        &profile(true, true),
        Some(&app_and_core()),
        &reviewed_snapshot(),
        &own_rules_only,
    );
    assert_eq!(
        decisions.non_participating,
        vec![NonParticipatingPath {
            path: "CHANGELOG.md".to_string(),
            rule: "CHANGELOG.md".to_string(),
        }]
    );
}

#[test]
fn an_unparsable_repository_pattern_is_dropped_rather_than_applied_loosely() {
    let (classifier, rejected) = PathClassifier::new(true, &["design/[".to_string()]);
    assert_eq!(rejected, vec!["design/[".to_string()]);
    assert_eq!(
        classifier.classify("design/wireframe.png"),
        PathClass::Unknown
    );
}

/// Contract §6a.7 and §8.1. A documentation-only change leaves nothing relevant
/// to select, and the honest outcome is a `Skipped` with a stated reason —
/// never a `passed`, which would claim evidence the run never produced.
#[test]
fn a_documentation_only_change_selects_nothing_and_owes_an_honest_skip() {
    let set = trustworthy(vec![
        modified("CHANGELOG.md"),
        modified("docs/architecture.md"),
        modified("docs/usage.md"),
    ]);
    let decisions = decide_with(Some(&set), &profile(true, true), Some(&app_and_core()));

    assert_eq!(decisions.non_participating.len(), 3);
    for ecosystem in [Ecosystem::Cargo, Ecosystem::Vitest] {
        let decision = decisions.get(ecosystem);
        assert!(
            selected(decision).is_empty(),
            "{ecosystem:?} has nothing to run for a documentation-only change"
        );
        assert_eq!(
            decision.empty_selection_skip_reason(),
            Some(NO_TESTS_RELATED_TO_THE_CHANGE),
            "an empty selection owes a stated skip reason, never a silent pass"
        );
    }
}

#[test]
fn a_selection_that_chose_something_owes_no_skip_reason() {
    let set = trustworthy(vec![modified("crates/core/src/lib.rs")]);
    let decisions = decide_with(Some(&set), &profile(true, true), Some(&app_and_core()));
    assert_eq!(decisions.cargo.empty_selection_skip_reason(), None);
}

/// A full run is not an empty selection: it selected nothing because it is
/// running everything, and calling that "no tests related to the change" would
/// invert the meaning.
#[test]
fn a_full_run_never_claims_an_empty_selection() {
    let set = trustworthy(vec![modified("Cargo.lock")]);
    let decisions = decide_with(Some(&set), &profile(true, true), Some(&app_and_core()));
    assert_eq!(decisions.cargo.empty_selection_skip_reason(), None);
}

/// §6a.6: the classification has to travel to the reader on the check rows that
/// carry a scope, or it cannot be challenged without reading the source.
#[test]
fn the_published_scope_carries_every_neutral_path_and_its_rule() {
    let set = trustworthy(vec![
        modified("CHANGELOG.md"),
        modified("crates/core/src/lib.rs"),
    ]);
    let decisions = decide_with(Some(&set), &profile(true, true), Some(&app_and_core()));
    for check in ["Cargo test", "Vitest"] {
        let report = decisions
            .report_for_check(&ran_without_evidence(check))
            .unwrap_or_else(|| panic!("{check} owns a test scope"));
        assert_eq!(
            report.non_participating,
            vec![NonParticipatingPath {
                path: "CHANGELOG.md".to_string(),
                rule: "root-changelog".to_string(),
            }],
            "{check} must publish path -> rule for every neutral path"
        );
    }
    assert!(
        decisions
            .report_for_check(&ran_without_evidence("Clippy"))
            .is_none(),
        "a check with no test suite still carries no scope at all"
    );
}

#[test]
fn a_run_that_neutralised_nothing_publishes_no_neutral_list() {
    let set = trustworthy(vec![modified("crates/core/src/lib.rs")]);
    let decisions = decide_with(Some(&set), &profile(true, true), Some(&app_and_core()));
    let report = decisions
        .report_for_check(&narrowed("Cargo test", 1, "-p core"))
        .expect("scope");
    assert!(report.non_participating.is_empty());
    let json = serde_json::to_value(&report).expect("serialize scope");
    assert!(
        json.get("non_participating").is_none(),
        "the field is additive and omitted when empty, so an untouched run looks untouched"
    );
}

// ---------------------------------------------------------------------------
// The cargo root must be the REVIEWED one
// ---------------------------------------------------------------------------

/// `profile.cargo_root` is detected in the operator checkout, which on a `--pr`
/// run is a different revision from the one under review. Reading metadata
/// there would describe another revision's members and path edges while the
/// change set describes this one.
#[test]
fn the_cargo_root_is_rebased_onto_the_reviewed_tree() {
    assert_eq!(
        rebase_cargo_root(
            Path::new("/repo/src-tauri"),
            Path::new("/repo"),
            Path::new("/snap")
        ),
        CargoRoot::Reviewed(PathBuf::from("/snap/src-tauri")),
        "a nested cargo root keeps its position inside the reviewed tree"
    );
    assert_eq!(
        rebase_cargo_root(Path::new("/repo"), Path::new("/repo"), Path::new("/snap")),
        CargoRoot::Reviewed(PathBuf::from("/snap")),
        "a cargo root AT the repository root maps to the reviewed root itself"
    );
    assert_eq!(
        rebase_cargo_root(Path::new("/repo"), Path::new("/repo"), Path::new("/repo")),
        CargoRoot::Reviewed(PathBuf::from("/repo")),
        "an on-HEAD review reads the repository root, and the mapping is identity"
    );
}

#[test]
fn a_cargo_root_outside_the_repository_escalates_rather_than_guessing() {
    assert_eq!(
        rebase_cargo_root(
            Path::new("/elsewhere/crate"),
            Path::new("/repo"),
            Path::new("/snap")
        ),
        CargoRoot::Unlocatable
    );
    let unlocatable: Result<CargoWorkspace, WorkspaceError> = Err(WorkspaceError::UnlocatableRoot);
    let set = trustworthy(vec![modified("crates/core/src/lib.rs")]);
    let decisions = decide_with(Some(&set), &profile(true, true), Some(&unlocatable));
    assert_eq!(
        full_reason(&decisions.cargo),
        reason::resolution_failed(reason::REVIEWED_CARGO_ROOT_UNLOCATABLE)
    );
}

// ---------------------------------------------------------------------------
// Metadata failure modes (contract §5.4)
// ---------------------------------------------------------------------------

#[test]
fn every_metadata_failure_escalates_cargo_with_its_own_reason() {
    let cases = [
        (
            WorkspaceError::ExitStatus(Some(101)),
            "cargo metadata exited with code 101",
        ),
        (
            WorkspaceError::ExitStatus(None),
            "cargo metadata was terminated by a signal",
        ),
        (WorkspaceError::Timeout, "cargo metadata timed out"),
        (
            WorkspaceError::NoMembers,
            "cargo metadata reported no workspace members",
        ),
        (
            WorkspaceError::Unparsable("expected value".to_string()),
            "cargo metadata emitted unparsable JSON: expected value",
        ),
        (
            WorkspaceError::Spawn("No such file or directory".to_string()),
            "cargo metadata could not run: No such file or directory",
        ),
    ];
    for (error, detail) in cases {
        let failed: Result<CargoWorkspace, WorkspaceError> = Err(error);
        let set = trustworthy(vec![modified("crates/core/src/lib.rs")]);
        let decisions = decide_with(Some(&set), &profile(true, true), Some(&failed));
        assert_eq!(
            full_reason(&decisions.cargo),
            reason::resolution_failed(detail)
        );
        // A cargo failure never widens the other ecosystem.
        assert!(
            !decisions.vitest.is_full(),
            "a cargo metadata failure must not escalate vitest"
        );
    }
}

#[test]
fn unparsable_metadata_json_is_a_failure_not_an_empty_workspace() {
    assert!(matches!(
        CargoWorkspace::from_metadata_json(b"not json"),
        Err(WorkspaceError::Unparsable(_))
    ));
    assert!(matches!(
        CargoWorkspace::from_metadata_json(br#"{"packages":[]}"#),
        Err(WorkspaceError::NoMembers)
    ));
}

#[test]
fn a_rust_file_outside_every_member_escalates_rather_than_selecting_nothing() {
    let set = trustworthy(vec![modified("docs/scratch.rs")]);
    let decisions = decide_with(Some(&set), &profile(true, true), Some(&app_and_core()));
    assert_eq!(
        full_reason(&decisions.cargo),
        reason::resolution_failed("docs/scratch.rs is outside every workspace member")
    );
}

#[test]
fn a_repo_without_a_cargo_root_reports_that_rather_than_a_selection() {
    let set = trustworthy(vec![modified("src/app.ts")]);
    let decisions = decide_with(Some(&set), &profile(true, false), None);
    assert_eq!(full_reason(&decisions.cargo), reason::NO_CARGO_ROOT);
    assert_eq!(selected(&decisions.vitest), ["src/app.ts".to_string()]);
}

#[test]
fn a_repo_without_js_source_reports_that_rather_than_a_selection() {
    let set = trustworthy(vec![modified("crates/core/src/lib.rs")]);
    let decisions = decide_with(Some(&set), &profile(false, true), Some(&app_and_core()));
    assert_eq!(full_reason(&decisions.vitest), reason::NO_JS_SOURCE);
}

// ---------------------------------------------------------------------------
// Selection shape
// ---------------------------------------------------------------------------

#[test]
fn generated_output_is_not_a_selector_input() {
    let is_generated: &dyn Fn(&str) -> bool = &|path: &str| path.starts_with("dist/");
    let set = trustworthy(vec![modified("dist/bundle.js"), modified("src/app.ts")]);
    let decisions = decide(
        Some(&set),
        &ScopeInputs {
            reviewed_tree: &reviewed_snapshot(),
            profile: &profile(true, true),
            cargo_workspace: Some(&app_and_core()),
            is_generated,
            classifier: &builtin_classifier(),
        },
    );
    assert_eq!(selected(&decisions.vitest), ["src/app.ts".to_string()]);
}

#[test]
fn a_cargo_selection_names_packages_and_a_vitest_selection_names_files() {
    let set = trustworthy(vec![
        modified("crates/core/src/lib.rs"),
        modified("src/app.ts"),
    ]);
    let decisions = decide_with(Some(&set), &profile(true, true), Some(&app_and_core()));
    assert_eq!(
        selected(&decisions.cargo),
        ["app".to_string(), "core".to_string()],
        "a package change selects the package and everything depending on it by path"
    );
    assert_eq!(selected(&decisions.vitest), ["src/app.ts".to_string()]);
    match &decisions.cargo {
        ScopeDecision::ChangeScoped { universe, .. } => assert_eq!(*universe, Some(2)),
        other => panic!("expected a scoped cargo decision, got {other:?}"),
    }
    match &decisions.vitest {
        ScopeDecision::ChangeScoped { universe, .. } => assert_eq!(
            *universe, None,
            "only vitest can enumerate its own test-file universe"
        ),
        other => panic!("expected a scoped vitest decision, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Reporting honesty
// ---------------------------------------------------------------------------

/// The decision alone is never enough. A `ChangeScoped` decision from a check
/// that left no evidence of narrowing describes a run that did not happen, so
/// the report says `full` and names the missing confirmation.
#[test]
fn a_scopeable_decision_without_evidence_is_reported_as_full() {
    let set = trustworthy(vec![modified("src/app.ts")]);
    let decisions = decide_with(Some(&set), &profile(true, false), None);
    let report = decisions
        .report_for_check(&ran_without_evidence("Vitest"))
        .expect("Vitest owns a test scope");
    assert_eq!(report.mode, "full");
    assert_eq!(report.reason, SCOPED_EXECUTION_NOT_CONFIRMED);
    assert_eq!(report.inputs, Some(1));
    assert_eq!(
        report.selected, None,
        "an unconfirmed narrowing must not publish a selection count"
    );
    assert_eq!(report.selector, None);
}

/// And the positive case: evidence from the check is what unlocks
/// `change-scoped`, together with the selector the command actually carried.
#[test]
fn a_confirmed_narrowing_is_reported_from_the_checks_own_evidence() {
    let set = trustworthy(vec![modified("crates/core/src/lib.rs")]);
    let decisions = decide_with(Some(&set), &profile(true, true), Some(&app_and_core()));
    let report = decisions
        .report_for_check(&narrowed("Cargo test", 2, "-p app -p core"))
        .expect("Cargo test owns a test scope");
    assert_eq!(report.mode, "change-scoped");
    assert_eq!(report.reason, CHANGE_SCOPED_SELECTION);
    assert_eq!(report.selected, Some(2));
    assert_eq!(report.selector.as_deref(), Some("-p app -p core"));
    assert_eq!(
        report.universe,
        Some(2),
        "the population the selection was drawn from stays visible"
    );
}

/// A check that escalated at RUNTIME overrides a scopeable decision: what ran
/// is what gets reported, with the runtime reason rather than the decision's
/// optimism.
#[test]
fn a_runtime_escalation_overrides_the_decision_in_the_report() {
    let set = trustworthy(vec![modified("src/app.ts")]);
    let decisions = decide_with(Some(&set), &profile(true, false), None);
    let escalated = result_with(
        "Vitest",
        CheckStatus::Passed,
        Some(ExecutedScope::Full {
            reason: reason::resolution_failed("src/app.ts is missing from the reviewed tree"),
        }),
    );
    let report = decisions
        .report_for_check(&escalated)
        .expect("Vitest owns a test scope");
    assert_eq!(report.mode, "full");
    assert_eq!(
        report.reason,
        reason::resolution_failed("src/app.ts is missing from the reviewed tree")
    );
    assert_eq!(report.selected, None);
    assert_eq!(report.selector, None);
}

/// An empty selection is the one `change-scoped` report with no selector: there
/// was a decision, it selected nothing, and the check honoured it by skipping.
/// `selected: 0` beside a skipped row is the honest shape; `passed` never is.
#[test]
fn an_empty_selection_reports_change_scoped_with_no_selector() {
    let set = trustworthy(vec![modified("CHANGELOG.md")]);
    let decisions = decide_with(Some(&set), &profile(true, true), Some(&app_and_core()));
    let skipped = result_with("Cargo test", CheckStatus::Skipped, None);
    let report = decisions
        .report_for_check(&skipped)
        .expect("Cargo test owns a test scope");
    assert_eq!(report.mode, "change-scoped");
    assert_eq!(report.reason, CHANGE_SCOPED_SELECTION);
    assert_eq!(report.selected, Some(0));
    assert_eq!(report.selector, None);
}

/// The same empty decision from a check that did NOT skip is not evidence of
/// anything: something ran, and nothing says it was narrowed.
#[test]
fn an_empty_selection_a_check_ignored_is_not_reported_as_narrowed() {
    let set = trustworthy(vec![modified("CHANGELOG.md")]);
    let decisions = decide_with(Some(&set), &profile(true, true), Some(&app_and_core()));
    let report = decisions
        .report_for_check(&ran_without_evidence("Cargo test"))
        .expect("Cargo test owns a test scope");
    assert_eq!(report.mode, "full");
    assert_eq!(report.reason, SCOPED_EXECUTION_NOT_CONFIRMED);
}

#[test]
fn a_real_escalation_keeps_its_own_reason_in_the_report() {
    let set = trustworthy(vec![modified("Cargo.lock"), modified("src/app.ts")]);
    let decisions = decide_with(Some(&set), &profile(true, true), Some(&app_and_core()));
    let report = decisions
        .report_for_check(&ran_without_evidence("Cargo test"))
        .expect("Cargo test owns a test scope");
    assert_eq!(report.mode, "full");
    assert_eq!(report.reason, reason::manifest("Cargo.lock"));
    assert_eq!(
        report.inputs,
        Some(2),
        "a full run that counted two changed paths says two; `null` is reserved \
         for having had nothing to count"
    );
    assert_eq!(report.selected, None);
    assert_eq!(report.selector, None);
}

/// `inputs: null` is a distinct statement from `inputs: 0`, and it belongs to
/// exactly one case: there was never a change set to count.
#[test]
fn only_a_missing_change_set_reports_an_unknown_input_count() {
    let unknown = decide_with(None, &profile(true, true), Some(&app_and_core()));
    assert_eq!(
        unknown
            .report_for_check(&ran_without_evidence("Cargo test"))
            .expect("scope")
            .inputs,
        None
    );

    let counted = ChangeSet::new(vec![modified("src/app.ts"), modified("src/b.ts")], false);
    let decisions = decide_with(Some(&counted), &profile(true, true), Some(&app_and_core()));
    assert_eq!(
        decisions
            .report_for_check(&ran_without_evidence("Vitest"))
            .expect("scope")
            .inputs,
        Some(2),
        "a set that was read and rejected was still counted"
    );
}

#[test]
fn the_scope_object_lands_only_on_the_checks_that_own_a_test_scope() {
    let set = trustworthy(vec![modified("src/app.ts")]);
    let decisions = decide_with(Some(&set), &profile(true, false), None);
    assert!(decisions.for_check("Cargo test").is_some());
    assert!(decisions.for_check("Vitest").is_some());
    assert!(
        decisions.for_check("vitest").is_some(),
        "name match is case-insensitive"
    );
    assert!(decisions.for_check("Clippy").is_none());
    assert!(decisions.for_check("Semgrep scan").is_none());
    assert!(owns_test_scope("Cargo test") && owns_test_scope("Vitest"));
    assert!(!owns_test_scope("Clippy"));
}

/// Caveats are derived from the very reports the artifacts publish, so a
/// reviewer cannot be told the suite ran in full while a row says otherwise —
/// and cannot be left uninformed when a row says it narrowed.
#[test]
fn review_caveats_follow_the_published_reports() {
    let set = trustworthy(vec![modified("crates/core/src/lib.rs")]);
    let decisions = decide_with(Some(&set), &profile(true, true), Some(&app_and_core()));

    assert!(
        decisions
            .review_caveats(&[ran_without_evidence("Cargo test")])
            .is_empty(),
        "a caveat about a narrowed run must not appear before any run is narrowed"
    );

    let narrowed_run = decisions.review_caveats(&[narrowed("Cargo test", 2, "-p app -p core")]);
    assert_eq!(narrowed_run.len(), 1);
    assert!(
        narrowed_run[0].starts_with("Cargo test ran a change-scoped test selection"),
        "got: {narrowed_run:?}"
    );
}

/// A skip is a different statement from a narrowing, and the caveat has to say
/// which one happened — a reviewer reading "ran a narrower selection" about a
/// suite that ran nothing at all has been told the wrong thing.
#[test]
fn an_empty_selection_raises_a_skip_caveat_not_a_narrowing_one() {
    let set = trustworthy(vec![modified("CHANGELOG.md")]);
    let decisions = decide_with(Some(&set), &profile(true, true), Some(&app_and_core()));
    let caveats =
        decisions.review_caveats(&[result_with("Cargo test", CheckStatus::Skipped, None)]);
    assert_eq!(
        caveats,
        vec![format!(
            "Cargo test skipped: {NO_TESTS_RELATED_TO_THE_CHANGE}"
        )]
    );
}

/// Contract §9: the operator asked for everything, so nothing is decided and no
/// `cargo metadata` is worth paying for. Both ecosystems say so in one voice.
#[tokio::test]
async fn full_tests_pins_both_ecosystems_to_a_full_run() {
    let mut config = crate::config::test_config();
    config.profile = profile(true, true);
    config.full_tests = true;
    config.changed_paths = Some(trustworthy(vec![modified("crates/core/src/lib.rs")]));

    // Returns before any subprocess: the flag is read ahead of the metadata
    // call, so a full-test run never pays for a decision it will not use.
    let decisions = resolve_run_scope(&config, &reviewed_snapshot()).await;
    for ecosystem in Ecosystem::ALL {
        assert_eq!(
            full_reason(decisions.get(ecosystem)),
            reason::FULL_TESTS_REQUESTED,
            "{ecosystem:?} must report the operator's request verbatim"
        );
    }
}
