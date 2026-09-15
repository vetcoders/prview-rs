//! End-to-end contract test for the `prview gate` process exit codes.
//!
//! The exit-code mapping (0 = PASS/advisory CONDITIONAL/strict warnings-only,
//! 1 = BLOCK, 2 = strict review-required or warning-clean rejection, 3 = gate
//! could not execute) is unit-tested at the
//! pure-function level in `src/gate.rs`. That does not prove the *binary*
//! actually exits with those codes — the composite GitHub Action decides
//! pass/fail solely from the process exit code, so the contract has to hold at
//! the process boundary, not just in a mapping function.
//!
//! These tests drive the real binary and assert the process exit code for each
//! contract branch. They are deterministic without depending on which quality
//! tools happen to be installed on the runner:
//!
//! * The gate profile disables tests/lint/heuristics, and the fixture PATH
//!   exposes git but not semgrep, so the `Semgrep scan` check is always skipped
//!   regardless of runner tooling.
//! * Each test owns its `PRVIEW_HOME` until the child exits. Parent storage and
//!   its locks cannot affect the expected exit code or receive fixture packs.
//! * Under the default policy that skip is advisory → CONDITIONAL (exit 0, or
//!   exit 2 with `--strict`).
//! * Under a `default_severity: block` policy the same skip becomes blocking →
//!   BLOCK (exit 1).
//! * Running the gate outside a git repository makes the review unable to
//!   execute → exit 3.
//! * An explicit `--base` that does not resolve is an execution error → exit 3,
//!   never an empty review that passes.
//! * An explicit `--base` is pinned to the commit it names before the run, so
//!   the run's own `git fetch --prune` cannot take it away mid-review, and pack
//!   headers still show the ref the caller wrote.
//! * `--base <before> --exact-base` reviews `before..HEAD` literally even when
//!   `before` is not an ancestor of `HEAD` — the force-push shape — while the
//!   same `--base` without the flag still normalizes to the merge-base.

use assert_cmd::prelude::*;
use prview::git::git_cmd;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::TempDir;

fn run_git(repo: &Path, args: &[&str]) {
    let status = git_cmd()
        .args(["-c", "commit.gpgsign=false", "-c", "tag.gpgsign=false"])
        .args(args)
        .current_dir(repo)
        .status()
        .expect("failed to run git command");
    assert!(status.success(), "git command failed: {args:?}");
}

/// A minimal repo with a `main` base and a checked-out feature branch that
/// changes one file, so `prview gate` has a diff to review.
fn create_gate_fixture() -> TempDir {
    let temp = tempfile::tempdir().expect("tempdir");
    let repo = temp.path();

    run_git(repo, &["init"]);
    run_git(repo, &["config", "user.name", "Test User"]);
    run_git(repo, &["config", "user.email", "test@example.com"]);

    fs::write(repo.join("README.md"), "hello\n").expect("write file");
    run_git(repo, &["add", "README.md"]);
    run_git(repo, &["commit", "-m", "initial"]);
    run_git(repo, &["branch", "-M", "main"]);

    run_git(repo, &["checkout", "-b", "feature/gate-exit-codes"]);
    fs::write(repo.join("README.md"), "hello\nworld\n").expect("update file");
    run_git(repo, &["add", "README.md"]);
    run_git(repo, &["commit", "-m", "change"]);

    temp
}

/// A repo whose feature branch removes a public Rust function — a genuine
/// breaking API change the gate must surface. Used to prove the breaking-change
/// escalation holds at the process boundary under `--strict`.
fn create_breaking_gate_fixture() -> TempDir {
    let temp = tempfile::tempdir().expect("tempdir");
    let repo = temp.path();

    run_git(repo, &["init"]);
    run_git(repo, &["config", "user.name", "Test User"]);
    run_git(repo, &["config", "user.email", "test@example.com"]);

    fs::write(repo.join("lib.rs"), "pub fn old_api() -> u32 {\n    1\n}\n").expect("write lib.rs");
    run_git(repo, &["add", "lib.rs"]);
    run_git(repo, &["commit", "-m", "initial"]);
    run_git(repo, &["branch", "-M", "main"]);

    run_git(repo, &["checkout", "-b", "feature/remove-public-api"]);
    // Remove the public function: a RemovedSymbol breaking finding.
    fs::write(repo.join("lib.rs"), "// old_api removed\n").expect("update lib.rs");
    run_git(repo, &["add", "lib.rs"]);
    run_git(repo, &["commit", "-m", "remove public api"]);

    temp
}

/// A deterministic warnings-only pack: missing semgrep is explicitly ignored,
/// while an added unsafe block produces the artifact-only `unsafe_audit`
/// warning in the canonical checks list.
fn create_warning_only_gate_fixture() -> TempDir {
    let temp = tempfile::tempdir().expect("tempdir");
    let repo = temp.path();

    run_git(repo, &["init"]);
    run_git(repo, &["config", "user.name", "Test User"]);
    run_git(repo, &["config", "user.email", "test@example.com"]);
    fs::create_dir_all(repo.join("src")).expect("create src");
    fs::write(
        repo.join("Cargo.toml"),
        "[package]\nname='operator-policy-fixture'\nversion='0.0.0'\nedition='2024'\n",
    )
    .expect("write Cargo.toml");
    fs::write(repo.join("src/lib.rs"), "pub fn stable() {}\n").expect("write lib.rs");
    fs::write(
        repo.join(".prview-policy.yml"),
        "version: 1\nmode: warn\ndefault_severity: ignore\nchecks:\n  semgrep_scan: ignore\n  cargo_audit: ignore\n",
    )
    .expect("write policy");
    run_git(
        repo,
        &["add", "Cargo.toml", "src/lib.rs", ".prview-policy.yml"],
    );
    run_git(repo, &["commit", "-m", "initial"]);
    run_git(repo, &["branch", "-M", "main"]);

    run_git(repo, &["checkout", "-b", "feature/warnings-only"]);
    fs::write(
        repo.join("src/lib.rs"),
        "pub fn stable() {}\n\npub unsafe fn raw(ptr: *const u8) -> u8 {\n    unsafe { *ptr }\n}\n",
    )
    .expect("add unsafe API");
    run_git(repo, &["add", "src/lib.rs"]);
    run_git(repo, &["commit", "-m", "add unsafe api"]);

    temp
}

fn path_without_semgrep(repo: &Path) -> OsString {
    let bin_dir = repo.join(".test-bin");
    fs::create_dir_all(&bin_dir).expect("create fixture bin dir");

    let git_path = which::which("git").expect("git must be available for gate fixtures");
    let git_file_name = git_path.file_name().expect("git path has file name");
    fs::copy(&git_path, bin_dir.join(git_file_name)).expect("copy git into fixture PATH");

    OsString::from(bin_dir)
}

fn prview_gate_command(repo: &Path, home: &Path) -> Command {
    let mut command = Command::new(assert_cmd::cargo::cargo_bin!("prview"));
    command
        .current_dir(repo)
        .env("PATH", path_without_semgrep(repo))
        .env("PRVIEW_HOME", home);
    command
}

#[test]
fn gate_exits_zero_for_non_strict_conditional() {
    let home = tempfile::tempdir().expect("prview home");
    let temp = create_gate_fixture();

    // Default policy: the skipped Semgrep check is advisory → CONDITIONAL,
    // which is accepted (exit 0) without --strict.
    prview_gate_command(temp.path(), home.path())
        .arg("gate")
        .assert()
        .code(0);
}

#[test]
fn gate_exits_two_for_strict_conditional() {
    let home = tempfile::tempdir().expect("prview home");
    let temp = create_gate_fixture();

    // Same CONDITIONAL verdict, but --strict rejects it with exit 2. This is the
    // exact code clap also uses for usage errors, which is why the action must
    // distinguish the two (see action.yml) — here we pin the contract value.
    prview_gate_command(temp.path(), home.path())
        .args(["gate", "--strict"])
        .assert()
        .code(2);
}

#[test]
fn gate_exits_two_for_strict_conditional_with_breaking_change() {
    let home = tempfile::tempdir().expect("prview home");
    let temp = create_breaking_gate_fixture();

    // A diff that removes a public Rust function is a breaking API change. With
    // the default `[gate] breaking_escalation` knob on, that escalates the
    // verdict to CONDITIONAL, which `--strict` rejects with the contract exit 2.
    // (The skipped Semgrep check is also advisory here; either way the process
    // must exit 2 with a real breaking finding present in the pack.)
    prview_gate_command(temp.path(), home.path())
        .args(["gate", "--strict"])
        .assert()
        .code(2);
}

#[test]
fn operator_policy_real_gate_warning_lane() {
    let temp = create_warning_only_gate_fixture();
    let home = tempfile::tempdir().expect("prview home");
    let path = path_without_semgrep(temp.path());

    let strict = Command::new(assert_cmd::cargo::cargo_bin!("prview"))
        .current_dir(temp.path())
        .env("PATH", &path)
        .env("PRVIEW_HOME", home.path())
        .args(["gate", "--strict", "--json"])
        .output()
        .expect("run strict warning gate");
    assert_eq!(
        strict.status.code(),
        Some(0),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&strict.stdout),
        String::from_utf8_lossy(&strict.stderr)
    );
    let strict_json: serde_json::Value = serde_json::from_slice(&strict.stdout).unwrap();
    assert_eq!(strict_json["enforcement_disposition"], "warnings_only");
    assert_eq!(strict_json["exit_code"], 0);

    let hardened = Command::new(assert_cmd::cargo::cargo_bin!("prview"))
        .current_dir(temp.path())
        .env("PATH", &path)
        .env("PRVIEW_HOME", home.path())
        .args(["gate", "--strict", "--fail-on-warnings", "--json"])
        .output()
        .expect("run warnings-clean gate");
    assert_eq!(hardened.status.code(), Some(2));
    let hardened_json: serde_json::Value = serde_json::from_slice(&hardened.stdout).unwrap();
    assert_eq!(hardened_json["enforcement_disposition"], "warnings_only");
    assert_eq!(hardened_json["exit_code"], 2);
    assert_eq!(hardened_json["fail_on_warnings"], true);
}

#[test]
fn gate_exits_one_for_block_verdict() {
    let home = tempfile::tempdir().expect("prview home");
    let temp = create_gate_fixture();
    let repo = temp.path();

    // Escalate the skipped required check to blocking so the verdict is BLOCK.
    fs::write(
        repo.join(".prview-policy.yml"),
        "version: 1\nmode: block\ndefault_severity: block\n",
    )
    .expect("write policy");
    run_git(repo, &["add", ".prview-policy.yml"]);
    run_git(repo, &["commit", "-m", "block policy"]);

    prview_gate_command(repo, home.path())
        .arg("gate")
        .assert()
        .code(1);
}

/// A pack whose `MERGE_GATE.json` is gone carries no verdict. `prview --ci` used
/// to paper over that by re-deriving the decision from the in-memory policy
/// engine — the one path where `allow_merge: true` could sit beside a
/// `CONDITIONAL` verdict. The reader now fails loud with the same execution-error
/// exit code the gate uses, and the contract has to hold at the process boundary.
#[test]
fn ci_run_exits_three_when_pack_has_no_merge_gate() {
    let temp = create_gate_fixture();
    let repo = temp.path();
    let home = tempfile::tempdir().expect("prview home");
    // Built once: the helper copies git into the fixture bin dir, which is not
    // writable a second time.
    let path = path_without_semgrep(repo);

    // 1. A real run, so the pack on disk is a genuine one (metadata included).
    let assert = Command::new(assert_cmd::cargo::cargo_bin!("prview"))
        .current_dir(repo)
        .env("PATH", &path)
        .env("PRVIEW_HOME", home.path())
        .args(["--ci", "--quiet", "--no-zip", "--no-heuristics"])
        .assert();
    let first_code = assert.get_output().status.code();
    assert!(
        matches!(first_code, Some(0) | Some(1)),
        "seeding run must produce a verdict, got exit {first_code:?}"
    );

    // 2. Amputate the verdict artifact, leaving an otherwise complete pack.
    let mut removed = 0usize;
    for gate in walk_merge_gate_json(home.path()) {
        fs::remove_file(&gate).expect("remove MERGE_GATE.json");
        removed += 1;
    }
    assert_eq!(
        removed, 1,
        "seeding run must write exactly one MERGE_GATE.json"
    );

    // 3. `--update` re-reads that pack (HEAD is unchanged). No verdict is
    //    readable, so the process must report an execution error, not a guess.
    Command::new(assert_cmd::cargo::cargo_bin!("prview"))
        .current_dir(repo)
        .env("PATH", &path)
        .env("PRVIEW_HOME", home.path())
        .args(["--ci", "--update", "--quiet", "--no-zip", "--no-heuristics"])
        .assert()
        .code(3);
}

/// Every `00_summary/MERGE_GATE.json` under a prview home.
fn walk_merge_gate_json(root: &Path) -> Vec<std::path::PathBuf> {
    let mut found = Vec::new();
    let Ok(entries) = fs::read_dir(root) else {
        return found;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        // `latest` is a symlink to the newest run; following it would visit the
        // same pack twice.
        if path.is_symlink() {
            continue;
        }
        if path.is_dir() {
            found.extend(walk_merge_gate_json(&path));
        } else if path.file_name().is_some_and(|n| n == "MERGE_GATE.json") {
            found.push(path);
        }
    }
    found
}

#[test]
fn gate_exits_three_when_it_cannot_execute() {
    let home = tempfile::tempdir().expect("prview home");
    // Outside a git repository the review cannot run, so the gate reports an
    // execution error (exit 3) rather than a verdict.
    let temp = tempfile::tempdir().expect("tempdir");

    Command::new(assert_cmd::cargo::cargo_bin!("prview"))
        .current_dir(temp.path())
        .env("GIT_CEILING_DIRECTORIES", temp.path())
        .env("PRVIEW_HOME", home.path())
        .arg("gate")
        .assert()
        .code(3);
}

/// A repo that sits on `main` with two commits — the shape of a CI checkout
/// after a push to the default branch. Base auto-detection resolves `main` to
/// the target itself, so only an explicit base yields a change to review.
/// Returns the fixture and the SHA of the first (pre-push) commit.
fn create_pushed_main_fixture() -> (TempDir, String) {
    let temp = tempfile::tempdir().expect("tempdir");
    let repo = temp.path();

    run_git(repo, &["init"]);
    run_git(repo, &["config", "user.name", "Test User"]);
    run_git(repo, &["config", "user.email", "test@example.com"]);

    fs::write(repo.join("README.md"), "hello\n").expect("write file");
    run_git(repo, &["add", "README.md"]);
    run_git(repo, &["commit", "-m", "initial"]);
    run_git(repo, &["branch", "-M", "main"]);

    let before = git_cmd()
        .args(["rev-parse", "HEAD"])
        .current_dir(repo)
        .output()
        .expect("rev-parse HEAD");
    assert!(before.status.success(), "rev-parse HEAD failed");
    let before = String::from_utf8(before.stdout)
        .expect("utf8 sha")
        .trim()
        .to_string();

    fs::write(repo.join("README.md"), "hello\nworld\n").expect("update file");
    run_git(repo, &["add", "README.md"]);
    run_git(repo, &["commit", "-m", "pushed change"]);

    (temp, before)
}

/// Run `prview gate --json` with extra args and return the parsed gate JSON.
/// The pack lives under `home`, which must outlive the caller's assertions.
fn run_gate_json(repo: &Path, path: &OsString, home: &Path, extra: &[&str]) -> serde_json::Value {
    let output = Command::new(assert_cmd::cargo::cargo_bin!("prview"))
        .current_dir(repo)
        .env("PATH", path)
        .env("PRVIEW_HOME", home)
        .arg("gate")
        .args(extra)
        .arg("--json")
        .output()
        .expect("run gate");
    assert_eq!(
        output.status.code(),
        Some(0),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("gate json")
}

fn per_file_diff_count(gate_json: &serde_json::Value) -> usize {
    let output_dir = gate_json["output_dir"]
        .as_str()
        .expect("gate json names its output_dir");
    fs::read_dir(Path::new(output_dir).join("10_diff").join("per-file-diffs"))
        .map(|entries| entries.flatten().count())
        .unwrap_or(0)
}

#[test]
fn gate_explicit_base_commit_reviews_the_pushed_change() {
    let (temp, before) = create_pushed_main_fixture();
    // Built once: the helper copies git into the fixture bin dir, which is not
    // writable a second time.
    let path = path_without_semgrep(temp.path());

    // Control: auto-detection resolves `main` == target, so the review is empty.
    let auto_home = tempfile::tempdir().expect("prview home");
    let auto = run_gate_json(temp.path(), &path, auto_home.path(), &[]);
    assert_eq!(
        per_file_diff_count(&auto),
        0,
        "auto-detected base on main must review an empty change: {auto}"
    );

    // A raw 40-hex commit SHA as --base reviews the change since that commit.
    assert_eq!(before.len(), 40, "fixture passes a full commit SHA");
    let explicit_home = tempfile::tempdir().expect("prview home");
    let explicit = run_gate_json(
        temp.path(),
        &path,
        explicit_home.path(),
        &["--base", &before],
    );
    assert!(
        per_file_diff_count(&explicit) > 0,
        "--base <sha> must review a non-empty change: {explicit}"
    );
}

fn rev_parse(repo: &Path, rev: &str) -> String {
    let output = git_cmd()
        .args(["rev-parse", rev])
        .current_dir(repo)
        .output()
        .expect("rev-parse");
    assert!(output.status.success(), "rev-parse {rev} failed");
    String::from_utf8(output.stdout)
        .expect("utf8 sha")
        .trim()
        .to_string()
}

/// `git diff --name-only <from> <to>` — the two-dot, tree-to-tree file set,
/// which is exactly what a push delivered between those two commits.
fn git_diff_name_only(repo: &Path, from: &str, to: &str) -> Vec<String> {
    let output = git_cmd()
        .args(["diff", "--name-only", from, to])
        .current_dir(repo)
        .output()
        .expect("git diff --name-only");
    assert!(output.status.success(), "git diff --name-only failed");
    let mut paths: Vec<String> = String::from_utf8(output.stdout)
        .expect("utf8 diff")
        .lines()
        .map(str::to_string)
        .collect();
    paths.sort();
    paths
}

/// The file set the pack actually reviewed, read from the pack's own
/// `report.json` rather than from what the gate was asked to do.
fn reviewed_paths(gate_json: &serde_json::Value) -> Vec<String> {
    let output_dir = gate_json["output_dir"]
        .as_str()
        .expect("gate json names its output_dir");
    let report: serde_json::Value = serde_json::from_slice(
        &fs::read(Path::new(output_dir).join("report.json")).expect("pack report.json"),
    )
    .expect("report.json is valid JSON");
    let mut paths: Vec<String> = report["diff"]["files"]
        .as_array()
        .expect("report.json names the reviewed files")
        .iter()
        .map(|file| {
            file["path"]
                .as_str()
                .expect("each reviewed file has a path")
                .to_string()
        })
        .collect();
    paths.sort();
    paths
}

/// A repo whose checked-out tip is NOT a descendant of the commit a push
/// reported as `before` — the shape of a force-push. The rewritten history also
/// drops a file the pre-push tip carried, so the literal range and the
/// merge-base range differ by more than line counts. Returns the fixture, the
/// pre-push commit, and the merge-base that normalization would fall back to.
fn create_force_pushed_main_fixture() -> (TempDir, String, String) {
    let temp = tempfile::tempdir().expect("tempdir");
    let repo = temp.path();

    run_git(repo, &["init"]);
    run_git(repo, &["config", "user.name", "Test User"]);
    run_git(repo, &["config", "user.email", "test@example.com"]);

    fs::write(repo.join("README.md"), "hello\n").expect("write file");
    run_git(repo, &["add", "README.md"]);
    run_git(repo, &["commit", "-m", "initial"]);
    run_git(repo, &["branch", "-M", "main"]);
    let common_ancestor = rev_parse(repo, "HEAD");

    // The tip the push reports as `before`. It carries a file the rewrite drops.
    fs::write(repo.join("README.md"), "hello\nworld\n").expect("update file");
    fs::write(repo.join("dropped.md"), "dropped by the force push\n").expect("write file");
    run_git(repo, &["add", "README.md", "dropped.md"]);
    run_git(repo, &["commit", "-m", "pre-push tip"]);
    let before = rev_parse(repo, "HEAD");

    // The force push: history is rewritten off the common ancestor, so `before`
    // stops being an ancestor of the new tip while staying a reachable object.
    run_git(repo, &["reset", "--hard", common_ancestor.as_str()]);
    fs::write(repo.join("README.md"), "hello\nrewritten\n").expect("update file");
    run_git(repo, &["add", "README.md"]);
    run_git(repo, &["commit", "-m", "force-pushed change"]);

    (temp, before, common_ancestor)
}

/// The contract this whole path exists for: Gate Shadow reviews the range the
/// push delivered. Diff bases are normalized to their merge-base with the
/// target, which on an ordinary fast-forward is `before` itself and changes
/// nothing — but on a force-push `before` is no longer an ancestor, and
/// normalization silently widens the review to
/// `merge-base(before, after)..after`. `--exact-base` opts that one base out, so
/// the reviewed file set is `git diff --name-only <before> <after>` exactly,
/// including the file the force push removed.
#[test]
fn gate_exact_base_reviews_the_literal_force_pushed_range() {
    let (temp, before, common_ancestor) = create_force_pushed_main_fixture();
    let path = path_without_semgrep(temp.path());
    let after = rev_parse(temp.path(), "HEAD");

    let is_ancestor = git_cmd()
        .args([
            "merge-base",
            "--is-ancestor",
            before.as_str(),
            after.as_str(),
        ])
        .current_dir(temp.path())
        .status()
        .expect("merge-base --is-ancestor");
    assert!(
        !is_ancestor.success(),
        "fixture: the pre-push commit must not be an ancestor of the new tip"
    );

    let delivered = git_diff_name_only(temp.path(), &before, &after);
    let normalized_range = git_diff_name_only(temp.path(), &common_ancestor, &after);
    assert_ne!(
        delivered, normalized_range,
        "fixture: the two range models must disagree for this test to mean anything"
    );
    assert!(
        delivered.contains(&"dropped.md".to_string()),
        "fixture: the delivered range includes the file the force push removed: {delivered:?}"
    );

    let exact_home = tempfile::tempdir().expect("prview home");
    let exact = run_gate_json(
        temp.path(),
        &path,
        exact_home.path(),
        &["--base", &before, "--exact-base"],
    );
    assert_eq!(
        reviewed_paths(&exact),
        delivered,
        "--exact-base must review exactly what the push delivered: {exact}"
    );

    // Control: the same `--base` without the flag still normalizes to the
    // merge-base, which is the unchanged contract for every other caller.
    let normalized_home = tempfile::tempdir().expect("prview home");
    let normalized = run_gate_json(
        temp.path(),
        &path,
        normalized_home.path(),
        &["--base", &before],
    );
    assert_eq!(
        reviewed_paths(&normalized),
        normalized_range,
        "without --exact-base the base is still normalized to the merge-base: {normalized}"
    );
    assert_ne!(
        reviewed_paths(&exact),
        reviewed_paths(&normalized),
        "the two modes must produce different reviews on a force-push"
    );
}

/// Pinned verbatim. This sentence is a statement about range semantics, not a
/// warning and not a review caveat — if a later change reworks it into one, or
/// routes it through the caveat machinery, these tests fail.
const REWRITTEN_RANGE_NOTE: &str = "Force-push detected: the file set reflects the pre-push \u{2192} current tree difference, so it may contain changes not attributable to any commit in the displayed commit list.";

/// A reader holding the pack sees a file list and a commit list side by side.
/// On a rewritten range the two legitimately disagree: the file set is the
/// literal pinned-base-to-target tree difference, so it carries what the
/// force-push removed, while no commit in the list removed it. The pack says so
/// once, beside the base, on both human surfaces.
#[test]
fn gate_exact_base_states_the_rewritten_range_in_the_pack() {
    let (temp, before, _common_ancestor) = create_force_pushed_main_fixture();
    let path = path_without_semgrep(temp.path());
    let home = tempfile::tempdir().expect("prview home");

    let gate = run_gate_json(
        temp.path(),
        &path,
        home.path(),
        &["--base", &before, "--exact-base"],
    );

    for surface in ["PR_REVIEW.md", "REVIEW_SUMMARY.md"] {
        let rendered = read_pack_file(&gate, surface);
        assert!(
            rendered.contains(REWRITTEN_RANGE_NOTE),
            "{surface} must state the rewritten range verbatim: {}",
            rendered.lines().take(12).collect::<Vec<_>>().join("\n")
        );
    }

    // It explains correct behaviour, so it must not be a review caveat and must
    // not have moved any verdict.
    let caveats = gate["caveats"]
        .as_array()
        .expect("gate json lists its caveats");
    assert!(
        !caveats
            .iter()
            .any(|caveat| caveat.as_str().is_some_and(|c| c.contains("Force-push"))),
        "the range note must not be routed through the caveat machinery: {caveats:?}"
    );
}

/// The note is a claim about this run's range, not about the flag. An ordinary
/// push is a fast-forward, the pinned base IS the merge-base, the file list and
/// the commit list agree — and the pack stays silent even though `--exact-base`
/// was passed.
#[test]
fn gate_exact_base_stays_silent_on_a_fast_forward() {
    let (temp, before) = create_pushed_main_fixture();
    let path = path_without_semgrep(temp.path());
    let after = rev_parse(temp.path(), "HEAD");
    let is_ancestor = git_cmd()
        .args([
            "merge-base",
            "--is-ancestor",
            before.as_str(),
            after.as_str(),
        ])
        .current_dir(temp.path())
        .status()
        .expect("merge-base --is-ancestor");
    assert!(
        is_ancestor.success(),
        "fixture: the pre-push commit must be an ancestor of the tip"
    );

    let home = tempfile::tempdir().expect("prview home");
    let gate = run_gate_json(
        temp.path(),
        &path,
        home.path(),
        &["--base", &before, "--exact-base"],
    );

    for surface in ["PR_REVIEW.md", "REVIEW_SUMMARY.md"] {
        let rendered = read_pack_file(&gate, surface);
        assert!(
            !rendered.contains("Force-push detected"),
            "{surface} must stay silent on a fast-forward range"
        );
    }
}

#[test]
fn gate_explicit_base_annotated_tag_reviews_the_change_since_the_tag() {
    let (temp, _before) = create_pushed_main_fixture();
    // Tag the first commit with an annotated tag: the ref names a tag object,
    // which must be peeled to the tagged commit before the diff.
    run_git(
        temp.path(),
        &["tag", "-a", "v0.1.0", "-m", "release", "HEAD~1"],
    );
    let path = path_without_semgrep(temp.path());

    let home = tempfile::tempdir().expect("prview home");
    let tagged = run_gate_json(temp.path(), &path, home.path(), &["--base", "v0.1.0"]);
    assert!(
        per_file_diff_count(&tagged) > 0,
        "--base <annotated tag> must review a non-empty change: {tagged}"
    );
}

/// A working repo whose `origin` no longer carries the branch the caller names
/// as the base. This is the shape `git fetch --prune` deletes: the
/// remote-tracking ref resolves when the gate accepts `--base`, and is gone by
/// the time the review resolves it. Returns the fixture, the work tree, and the
/// commit that branch pointed at.
fn create_pruned_remote_base_fixture() -> (TempDir, PathBuf, String) {
    let temp = tempfile::tempdir().expect("tempdir");
    let upstream = temp.path().join("upstream.git");
    let work = temp.path().join("work");
    fs::create_dir_all(&work).expect("create work dir");

    run_git(temp.path(), &["init", "--bare", "--quiet", "upstream.git"]);

    run_git(&work, &["init"]);
    run_git(&work, &["config", "user.name", "Test User"]);
    run_git(&work, &["config", "user.email", "test@example.com"]);
    fs::write(work.join("README.md"), "hello\n").expect("write file");
    run_git(&work, &["add", "README.md"]);
    run_git(&work, &["commit", "-m", "initial"]);
    run_git(&work, &["branch", "-M", "main"]);

    let before = git_cmd()
        .args(["rev-parse", "HEAD"])
        .current_dir(&work)
        .output()
        .expect("rev-parse HEAD");
    assert!(before.status.success(), "rev-parse HEAD failed");
    let before = String::from_utf8(before.stdout)
        .expect("utf8 sha")
        .trim()
        .to_string();

    run_git(
        &work,
        &[
            "remote",
            "add",
            "origin",
            upstream.to_str().expect("utf8 path"),
        ],
    );
    // Both remote branches start at the pre-change commit. No local branch is
    // created: `origin/release-base` must be reachable only as a
    // remote-tracking ref, which is the thing a prune can delete.
    run_git(
        &work,
        &[
            "push",
            "--quiet",
            "origin",
            "main:main",
            "main:release-base",
        ],
    );
    run_git(&work, &["fetch", "--quiet", "origin"]);

    // The change the review must actually see.
    fs::write(work.join("README.md"), "hello\nworld\n").expect("update file");
    run_git(&work, &["add", "README.md"]);
    run_git(&work, &["commit", "-m", "pushed change"]);

    // Upstream drops the branch. The ref still exists locally until the review's
    // own `git fetch --prune` runs.
    run_git(&upstream, &["update-ref", "-d", "refs/heads/release-base"]);

    (temp, work, before)
}

fn read_pack_file(gate_json: &serde_json::Value, name: &str) -> String {
    let output_dir = gate_json["output_dir"]
        .as_str()
        .expect("gate json names its output_dir");
    fs::read_to_string(Path::new(output_dir).join(name))
        .unwrap_or_else(|err| panic!("read {name} from the pack: {err}"))
}

/// The regression. `prview gate` accepts `--base` and only then starts the
/// review, which opens with `git fetch --quiet --prune origin`. A
/// `--base origin/<branch>` whose upstream branch has been deleted is pruned out
/// from under the run; base resolution drops what it cannot resolve, so the
/// review lost its only base, saw an empty change, and passed. Prune removes
/// refs and never objects, so the run is handed the commit id: the base survives
/// its own ref disappearing, and the pushed change is actually reviewed.
#[test]
fn gate_explicit_base_survives_a_prune_of_the_ref_it_named() {
    let (_temp, work, before) = create_pruned_remote_base_fixture();
    let path = path_without_semgrep(&work);
    let home = tempfile::tempdir().expect("prview home");

    let gate = run_gate_json(
        &work,
        &path,
        home.path(),
        &["--base", "origin/release-base"],
    );

    // The ref the caller named is gone by now: proof the prune really happened.
    let pruned = git_cmd()
        .args(["rev-parse", "--verify", "--quiet", "origin/release-base"])
        .current_dir(&work)
        .output()
        .expect("rev-parse the pruned ref");
    assert!(
        !pruned.status.success(),
        "the fixture must actually lose the ref to the review's prune"
    );

    assert!(
        per_file_diff_count(&gate) > 0,
        "a pruned --base must still review the change, not an empty diff: {gate}"
    );

    let report: serde_json::Value =
        serde_json::from_str(&read_pack_file(&gate, "report.json")).expect("parse report.json");
    assert_eq!(
        report["meta"]["range"]["merge_base"],
        serde_json::Value::String(before.clone()),
        "the review must run from the commit the pruned ref named: {}",
        report["meta"]["range"]
    );
}

/// The pin is an identity for resolving a range, not a label for a person. Pack
/// headers keep the ref the caller wrote and put the reviewed commit beside it,
/// so a reviewer reads `origin/release-base (abc123456789)` rather than forty
/// characters of hex.
#[test]
fn gate_explicit_base_renders_the_callers_ref_name_with_the_reviewed_commit() {
    let (temp, before) = create_pushed_main_fixture();
    run_git(temp.path(), &["branch", "release-base", &before]);
    let path = path_without_semgrep(temp.path());
    let home = tempfile::tempdir().expect("prview home");

    let gate = run_gate_json(temp.path(), &path, home.path(), &["--base", "release-base"]);
    let expected = format!("`release-base` (`{}`)", &before[..12]);

    let pr_review = read_pack_file(&gate, "PR_REVIEW.md");
    assert!(
        pr_review.contains(&expected),
        "PR_REVIEW.md must name the caller's ref and the reviewed commit ({expected}): {}",
        pr_review.lines().take(12).collect::<Vec<_>>().join("\n")
    );
    let ai_index = read_pack_file(&gate, "AI_INDEX.md");
    assert!(
        ai_index.contains(&expected),
        "AI_INDEX.md must name the caller's ref and the reviewed commit ({expected}): {}",
        ai_index.lines().take(12).collect::<Vec<_>>().join("\n")
    );

    let report: serde_json::Value =
        serde_json::from_str(&read_pack_file(&gate, "report.json")).expect("parse report.json");
    assert_eq!(
        report["meta"]["range"]["base"],
        serde_json::Value::String("release-base".to_string()),
        "report.json keeps the caller's spelling too; the commit is its merge_base"
    );
}

#[test]
fn gate_exits_three_for_unresolvable_explicit_base() {
    let temp = create_gate_fixture();
    let path = path_without_semgrep(temp.path());

    // The second ref embeds keywords `display_error` maps to hints; the
    // hint must come from the failure, never from the user's ref name.
    for base in ["does-not-exist", "remote-fetch-git"] {
        let home = tempfile::tempdir().expect("prview home");
        let output = Command::new(assert_cmd::cargo::cargo_bin!("prview"))
            .current_dir(temp.path())
            .env("PATH", &path)
            .env("PRVIEW_HOME", home.path())
            .args(["gate", "--base", base, "--json"])
            .output()
            .expect("run gate");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

        assert_eq!(
            output.status.code(),
            Some(3),
            "{base}: stdout={stdout} stderr={stderr}"
        );
        assert!(
            stderr.contains(&format!("gate base '{base}' does not resolve to a commit")),
            "the error must name the unresolvable ref: {stderr}"
        );
        assert!(
            !stderr.contains("hint:"),
            "no generic repository/network hint applies to an unresolvable base: {stderr}"
        );
        assert!(
            !stdout.contains("PASS"),
            "no verdict may be claimed for an unresolvable base: {stdout}"
        );
    }
}

/// `--pr` swaps the review base for the pull request's own base, so it must
/// never silently discard an explicit `--base`. Top-level flags cannot precede
/// the `gate` subcommand, and the gate does not accept `--pr`. Both spellings
/// therefore fail at parse time: offline, before any GitHub call, with no
/// verdict. The gate's own guard for the combination is unit-tested in
/// `src/main.rs`.
#[test]
fn gate_base_cannot_be_combined_with_pr() {
    let home = tempfile::tempdir().expect("prview home");
    let temp = create_gate_fixture();
    // No `gh` on PATH: anything past argument parsing would fail differently.
    // Built once, because the helper cannot copy git into the fixture bin dir twice.
    let path = path_without_semgrep(temp.path());

    for args in [
        &["--pr", "42", "gate", "--base", "main", "--json"][..],
        &["gate", "--base", "main", "--pr", "42", "--json"][..],
    ] {
        let output = Command::new(assert_cmd::cargo::cargo_bin!("prview"))
            .current_dir(temp.path())
            .env("PRVIEW_HOME", home.path())
            .env("PATH", &path)
            .args(args)
            .output()
            .expect("run prview");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

        assert_eq!(
            output.status.code(),
            Some(2),
            "{args:?} must be a usage error: stdout={stdout} stderr={stderr}"
        );
        assert!(
            stderr.contains("unexpected argument"),
            "{args:?} must be rejected by the parser: {stderr}"
        );
        assert!(
            stdout.is_empty(),
            "{args:?} must not emit a verdict: {stdout}"
        );
    }
}
