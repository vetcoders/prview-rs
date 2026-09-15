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
    decide(
        change_set,
        &ScopeInputs {
            reviewed_tree,
            profile,
            cargo_workspace: workspace,
            is_generated: &never_generated,
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
    let repo = Path::new("/repo");
    let snapshot = PathBuf::from("/snap");
    for clean in [Some(true), Some(false), None] {
        assert_eq!(
            ReviewedTree::resolve(repo, Some(snapshot.clone()), clean),
            ReviewedTree::Snapshot(snapshot.clone()),
            "a materialised snapshot settles the substrate on its own"
        );
    }
    assert_eq!(
        ReviewedTree::resolve(repo, None, Some(true)),
        ReviewedTree::LocalClean(repo.to_path_buf())
    );
    assert_eq!(
        ReviewedTree::resolve(repo, None, Some(false)),
        ReviewedTree::LocalDirty(repo.to_path_buf())
    );
    assert_eq!(
        ReviewedTree::resolve(repo, None, None),
        ReviewedTree::Unknown(repo.to_path_buf()),
        "an unreadable checkout is unknown, never assumed clean"
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
        "docs/architecture.md",
        ".github/workflows/ci.yml",
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
        },
    );
    assert_eq!(selected(&decisions.vitest), ["src/app.ts".to_string()]);
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

#[test]
fn a_scopeable_decision_is_still_reported_as_full_while_commands_are_full() {
    // The lie this contract forbids is reporting `change-scoped` for a run that
    // executed everything. Until the invocations change, the mode stays `full`
    // and the reason says exactly why.
    let set = trustworthy(vec![modified("src/app.ts")]);
    let decisions = decide_with(Some(&set), &profile(true, false), None);
    let report = decisions.vitest.report();
    assert_eq!(report.mode, "full");
    assert_eq!(report.reason, SCOPED_EXECUTION_NOT_ENABLED);
    assert_eq!(report.inputs, Some(1));
    assert_eq!(
        report.selected, None,
        "a full run selected nothing, and must not claim otherwise"
    );
    assert_eq!(
        report.selector, None,
        "no selector ran, so no selector arguments may be published"
    );
}

#[test]
fn a_real_escalation_keeps_its_own_reason_in_the_report() {
    let set = trustworthy(vec![modified("Cargo.lock"), modified("src/app.ts")]);
    let decisions = decide_with(Some(&set), &profile(true, true), Some(&app_and_core()));
    let report = decisions.cargo.report();
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
    assert_eq!(unknown.cargo.report().inputs, None);

    let counted = ChangeSet::new(vec![modified("src/app.ts"), modified("src/b.ts")], false);
    let decisions = decide_with(Some(&counted), &profile(true, true), Some(&app_and_core()));
    assert_eq!(
        decisions.vitest.report().inputs,
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
}

#[test]
fn no_review_caveat_is_raised_while_no_check_actually_runs_narrower() {
    let set = trustworthy(vec![modified("src/app.ts")]);
    let decisions = decide_with(Some(&set), &profile(true, false), None);
    assert!(
        decisions.review_caveats().is_empty(),
        "a caveat about a narrowed run must not appear before any run is narrowed"
    );
}
