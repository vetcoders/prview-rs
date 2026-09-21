//! Ephemeral git worktree support for remote check verification
//!
//! Creates a detached git worktree at a specific commit, with
//! local dependencies (node_modules, .venv) symlinked to preserve local caches.

use super::cmd::git_cmd;
#[cfg(unix)]
use anyhow::Context;
use anyhow::Result;
use std::collections::VecDeque;
use std::path::{Path, PathBuf};

#[cfg(unix)]
const BORROWED_LINKS_MANIFEST: &str = ".prview-borrowed-links";

const MAX_SYMLINK_RESOLUTIONS: usize = 40;

#[cfg(unix)]
const MAX_JS_SHIM_BYTES: u64 = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CommitPathResolution {
    Missing,
    Runnable,
    /// The target owns the requested entry or a symlink prefix, but Git alone
    /// cannot prove a runnable final file. The finished snapshot must resolve
    /// it and fail before spawn when it remains a directory/broken/non-file.
    Unresolved,
}

/// Classify `relative_path` in `commit`, following repository-relative
/// symlinks as far as the Git tree can prove.
///
/// `git2::Tree::get_path` deliberately does not traverse a blob stored with
/// mode `120000`, so a target-owned `.bin -> bin-owned` needs this small
/// resolver before eligibility can truthfully say whether the target contains
/// `node_modules/.bin/<tool>`. An absolute target-owned symlink is an existing
/// tool candidate too, but its final host path cannot be resolved from the Git
/// tree; admit it here and let the finished-snapshot resolver either reject a
/// missing/non-file target or classify the external bytes as borrowed.
pub(crate) fn commit_path_resolution(
    repo_root: &Path,
    commit: &str,
    relative_path: &Path,
) -> CommitPathResolution {
    let Ok(repo) = git2::Repository::discover(repo_root) else {
        return CommitPathResolution::Unresolved;
    };
    let Ok(commit) = repo
        .revparse_single(commit)
        .and_then(|object| object.peel_to_commit())
    else {
        return CommitPathResolution::Unresolved;
    };
    let Ok(tree) = commit.tree() else {
        return CommitPathResolution::Unresolved;
    };
    let Some(mut pending) = relative_components(relative_path) else {
        return CommitPathResolution::Unresolved;
    };
    let mut resolved = PathBuf::new();
    let mut followed_symlink = false;

    for _ in 0..MAX_SYMLINK_RESOLUTIONS {
        let Some(component) = pending.pop_front() else {
            return CommitPathResolution::Unresolved;
        };
        resolved.push(component);
        let Ok(entry) = tree.get_path(&resolved) else {
            return if followed_symlink {
                CommitPathResolution::Unresolved
            } else {
                CommitPathResolution::Missing
            };
        };

        if entry.filemode() == 0o120000 {
            followed_symlink = true;
            let Ok(object) = entry.to_object(&repo) else {
                return CommitPathResolution::Unresolved;
            };
            let Some(blob) = object.as_blob() else {
                return CommitPathResolution::Unresolved;
            };
            let Ok(target) = std::str::from_utf8(blob.content()) else {
                return CommitPathResolution::Unresolved;
            };
            if Path::new(target).is_absolute() {
                return CommitPathResolution::Runnable;
            }
            let Some(next) = resolved_symlink_path(&resolved, Path::new(target), &pending) else {
                return CommitPathResolution::Unresolved;
            };
            let Some(next_components) = relative_components(&next) else {
                return CommitPathResolution::Unresolved;
            };
            resolved.clear();
            pending = next_components;
            continue;
        }

        if pending.is_empty() {
            return if entry.kind() == Some(git2::ObjectType::Blob) {
                CommitPathResolution::Runnable
            } else {
                CommitPathResolution::Unresolved
            };
        }
        if entry.kind() != Some(git2::ObjectType::Tree) {
            return CommitPathResolution::Unresolved;
        }
    }

    CommitPathResolution::Unresolved
}

fn relative_components(path: &Path) -> Option<VecDeque<std::ffi::OsString>> {
    let mut normalized = Vec::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::Normal(component) => normalized.push(component.to_os_string()),
            std::path::Component::ParentDir => {
                normalized.pop()?;
            }
            std::path::Component::RootDir | std::path::Component::Prefix(_) => return None,
        }
    }
    Some(normalized.into())
}

fn resolved_symlink_path(
    symlink_path: &Path,
    target: &Path,
    tail: &VecDeque<std::ffi::OsString>,
) -> Option<PathBuf> {
    if target.is_absolute() {
        return None;
    }
    let mut combined = symlink_path
        .parent()
        .unwrap_or_else(|| Path::new(""))
        .join(target);
    combined.extend(tail.iter());
    let normalized = relative_components(&combined)?;
    Some(normalized.into_iter().collect())
}

/// True only when resolving `relative_path` in this snapshot consumes bytes
/// outside the snapshot tree.
///
/// The manifest lives beside the worktree, inside the same temporary directory,
/// so target-owned files cannot forge or collide with it and it disappears with
/// the snapshot. Its paths identify links created by prview, but comparison is
/// made through canonical filesystem identity rather than byte-exact spelling:
/// on a case-insensitive filesystem `ESLint` and `eslint` may name the same
/// created link. Canonical resolution also exposes target-owned absolute
/// symlinks that escape the snapshot.
///
/// A package-manager wrapper is not the final payload. Only strict, anchored
/// wrapper grammars contribute payload paths; comments, strings, and arbitrary
/// `require(` substrings are not evidence. A script whose closure cannot be
/// proved from one of those grammars is classified conservatively as borrowed.
#[cfg(unix)]
pub(crate) fn path_uses_prview_borrow(snapshot_root: &Path, relative_path: &Path) -> bool {
    let Some(parent) = snapshot_root.parent() else {
        return false;
    };
    let encoded = std::fs::read(parent.join(BORROWED_LINKS_MANIFEST)).unwrap_or_default();
    use std::os::unix::ffi::OsStringExt as _;
    let borrowed: Vec<PathBuf> = encoded
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .map(|path| PathBuf::from(std::ffi::OsString::from_vec(path.to_vec())))
        .collect();
    if borrowed
        .iter()
        .any(|created| relative_path.starts_with(created) || created.starts_with(relative_path))
    {
        return true;
    }
    let consumed = consumed_paths(snapshot_root, relative_path);
    if !consumed.closure_proven {
        return true;
    }
    if consumed.package_wrapper
        && borrowed.iter().any(|created| {
            created.starts_with("node_modules") || Path::new("node_modules").starts_with(created)
        })
    {
        return true;
    }
    consumed
        .paths
        .iter()
        .any(|path| path_is_external_or_borrowed(snapshot_root, path, borrowed.as_slice()))
}

#[cfg(unix)]
fn path_is_external_or_borrowed(snapshot_root: &Path, path: &Path, borrowed: &[PathBuf]) -> bool {
    let Ok(snapshot_identity) = std::fs::canonicalize(snapshot_root) else {
        return false;
    };
    let Ok(path_identity) = std::fs::canonicalize(path) else {
        return false;
    };

    if !path_identity.starts_with(&snapshot_identity) {
        return true;
    }

    borrowed.iter().any(|relative| {
        std::fs::canonicalize(snapshot_root.join(relative))
            .is_ok_and(|identity| path_identity.starts_with(identity))
    })
}

#[cfg(unix)]
struct ConsumedPaths {
    paths: Vec<PathBuf>,
    package_wrapper: bool,
    closure_proven: bool,
}

#[cfg(unix)]
fn consumed_paths(snapshot_root: &Path, relative_path: &Path) -> ConsumedPaths {
    let invocation = snapshot_root.join(relative_path);
    let mut consumed = vec![invocation.clone()];
    let Ok(metadata) = std::fs::metadata(&invocation) else {
        return ConsumedPaths {
            paths: consumed,
            package_wrapper: false,
            // Nothing can execute when final resolution fails. The check layer
            // reports that failure before spawn; provenance still describes the
            // target snapshot rather than inventing a borrowed execution.
            closure_proven: true,
        };
    };
    if !metadata.is_file() {
        return ConsumedPaths {
            paths: consumed,
            package_wrapper: false,
            closure_proven: true,
        };
    }
    use std::os::unix::fs::PermissionsExt as _;
    if metadata.permissions().mode() & 0o111 == 0 {
        return ConsumedPaths {
            paths: consumed,
            package_wrapper: false,
            closure_proven: true,
        };
    }
    if metadata.len() > MAX_JS_SHIM_BYTES {
        return ConsumedPaths {
            paths: consumed,
            package_wrapper: false,
            closure_proven: false,
        };
    }
    let Ok(bytes) = std::fs::read(&invocation) else {
        return ConsumedPaths {
            paths: consumed,
            package_wrapper: false,
            closure_proven: false,
        };
    };
    if !bytes.starts_with(b"#!") {
        return ConsumedPaths {
            paths: consumed,
            package_wrapper: false,
            closure_proven: true,
        };
    }
    let Ok(text) = std::str::from_utf8(&bytes) else {
        return ConsumedPaths {
            paths: consumed,
            package_wrapper: false,
            closure_proven: false,
        };
    };
    let Some(bin_dir) = invocation.parent() else {
        return ConsumedPaths {
            paths: consumed,
            package_wrapper: false,
            closure_proven: false,
        };
    };

    let active_lines: Vec<&str> = text
        .lines()
        .skip(1)
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .collect();
    let package_wrapper = is_known_pnpm_shell_wrapper(text, &active_lines);
    if !package_wrapper && !is_proved_direct_shell_script(text, &active_lines) {
        return ConsumedPaths {
            paths: consumed,
            package_wrapper: false,
            closure_proven: false,
        };
    }

    if package_wrapper {
        let basedir =
            regex::Regex::new(r#"\$basedir/([^\"'\s;|&)]+)"#).expect("static basedir regex");
        for line in &active_lines {
            for capture in basedir.captures_iter(line) {
                let Some(relative) = capture.get(1) else {
                    continue;
                };
                let candidate = bin_dir.join(relative.as_str());
                if candidate.exists() {
                    consumed.push(candidate);
                }
            }
        }
    }

    consumed.sort();
    consumed.dedup();
    ConsumedPaths {
        paths: consumed,
        package_wrapper,
        closure_proven: true,
    }
}

#[cfg(unix)]
fn is_known_pnpm_shell_wrapper(text: &str, active_lines: &[&str]) -> bool {
    let Some(shebang) = text.lines().next() else {
        return false;
    };
    if !matches!(shebang.trim(), "#!/bin/sh" | "#!/usr/bin/env sh") {
        return false;
    }
    if active_lines.len() != 2 || active_lines[0] != "basedir=$(dirname \"$0\")" {
        return false;
    }
    regex::Regex::new(r#"^exec node \"\$basedir/[^\"]+\" \"\$@\"$"#)
        .expect("static pnpm wrapper regex")
        .is_match(active_lines[1])
}

#[cfg(unix)]
fn is_proved_direct_shell_script(text: &str, active_lines: &[&str]) -> bool {
    let Some(shebang) = text.lines().next() else {
        return false;
    };
    if !matches!(
        shebang.trim(),
        "#!/bin/sh" | "#!/bin/bash" | "#!/usr/bin/env sh" | "#!/usr/bin/env bash"
    ) {
        return false;
    }
    let literal_output = regex::Regex::new(r#"^(?:printf|echo) '[^']*'$"#)
        .expect("static direct shell output regex");
    active_lines.iter().all(|line| {
        line == &":"
            || line == &"true"
            || line == &"false"
            || literal_output.is_match(line)
            || line == &"exit"
            || line
                .strip_prefix("exit ")
                .is_some_and(|code| !code.is_empty() && code.chars().all(|ch| ch.is_ascii_digit()))
    })
}

#[cfg(not(unix))]
pub(crate) fn path_uses_prview_borrow(_snapshot_root: &Path, _relative_path: &Path) -> bool {
    false
}

#[cfg(unix)]
fn create_borrowed_link(
    source: &Path,
    exposed: &Path,
    snapshot_root: &Path,
    borrowed_links: &mut Vec<PathBuf>,
) -> Result<()> {
    std::os::unix::fs::symlink(source, exposed).with_context(|| {
        format!(
            "failed to expose {} as borrowed dependency {}",
            source.display(),
            exposed.display()
        )
    })?;
    borrowed_links.push(
        exposed
            .strip_prefix(snapshot_root)
            .context("borrowed dependency escaped snapshot root")?
            .to_path_buf(),
    );
    Ok(())
}

#[cfg(unix)]
fn link_missing_entries(
    ambient: &Path,
    snapshot: &Path,
    snapshot_root: &Path,
    borrowed_links: &mut Vec<PathBuf>,
) -> Result<()> {
    for entry in std::fs::read_dir(ambient)
        .with_context(|| format!("failed to enumerate ambient {}", ambient.display()))?
    {
        let entry = entry.with_context(|| {
            format!("failed to read an entry from ambient {}", ambient.display())
        })?;
        let borrowed = entry.path();
        let exposed = snapshot.join(entry.file_name());
        match std::fs::symlink_metadata(&exposed) {
            Ok(_) => continue,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "failed to inspect target-owned dependency path {}",
                        exposed.display()
                    )
                });
            }
        }
        create_borrowed_link(&borrowed, &exposed, snapshot_root, borrowed_links)?;
    }
    Ok(())
}

#[cfg(unix)]
fn write_borrowed_links_manifest(temp_root: &Path, borrowed_links: &mut [PathBuf]) -> Result<()> {
    if borrowed_links.is_empty() {
        return Ok(());
    }
    borrowed_links.sort();
    use std::os::unix::ffi::OsStrExt as _;
    let mut encoded = Vec::new();
    for path in borrowed_links {
        encoded.extend_from_slice(path.as_os_str().as_bytes());
        encoded.push(0);
    }
    std::fs::write(temp_root.join(BORROWED_LINKS_MANIFEST), encoded)
        .context("failed to record snapshot borrowed-link provenance")
}

/// Roll back one exact worktree registration without spawning another child.
///
/// This is the cancellation backstop for the interval after `git worktree add`
/// has written its common-dir metadata but before it returns to prview. It is
/// deliberately path-scoped: a review must never prune another worktree merely
/// because both registrations live in the same repository.
fn prune_registered_worktree(repo_root: &Path, worktree_path: &Path) -> Result<bool> {
    let repo = git2::Repository::open(repo_root)?;
    let expected = comparable_worktree_path(worktree_path);
    for name in repo.worktrees()?.iter().flatten() {
        let Ok(worktree) = repo.find_worktree(name) else {
            continue;
        };
        if comparable_worktree_path(worktree.path()) != expected {
            continue;
        }
        let mut options = git2::WorktreePruneOptions::new();
        options.valid(true).locked(true).working_tree(false);
        worktree.prune(Some(&mut options))?;
        return Ok(true);
    }
    Ok(false)
}

fn comparable_worktree_path(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| {
        path.parent()
            .and_then(|parent| std::fs::canonicalize(parent).ok())
            .and_then(|parent| path.file_name().map(|name| parent.join(name)))
            .unwrap_or_else(|| path.to_path_buf())
    })
}

struct WorktreeRegistrationRollback {
    repo_root: PathBuf,
    worktree_path: PathBuf,
    armed: bool,
}

impl WorktreeRegistrationRollback {
    fn new(repo_root: &Path, worktree_path: &Path) -> Self {
        Self {
            repo_root: repo_root.to_path_buf(),
            worktree_path: worktree_path.to_path_buf(),
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for WorktreeRegistrationRollback {
    fn drop(&mut self) {
        if self.armed {
            let _ = prune_registered_worktree(&self.repo_root, &self.worktree_path);
        }
    }
}

/// An ephemeral detached `git worktree` checked out at a specific commit. Kept
/// alive for the duration of a scan; the worktree is deregistered and its files
/// removed on drop, on every path (scan success or error).
pub struct WorktreeSnapshot {
    pub repo_root: PathBuf,
    pub worktree_path: PathBuf,
    pub(crate) original_target_sha: String,
    registered: bool,
    // Owns the enclosing temp dir; dropped after the worktree is deregistered so
    // the directory removal is the backstop for the `git worktree remove` call.
    _tmp: tempfile::TempDir,
}

impl Drop for WorktreeSnapshot {
    fn drop(&mut self) {
        // Drop can run while unwinding an async stage. Never start or wait for a
        // child here: the explicit success path owns governed `git worktree
        // remove`, while this backstop only prunes this exact registration in
        // process. TempDir removes the checkout files after this method returns.
        if self.registered
            && matches!(
                prune_registered_worktree(&self.repo_root, &self.worktree_path),
                Ok(true)
            )
        {
            self.registered = false;
        }
    }
}

impl WorktreeSnapshot {
    /// Deregister this snapshot with an owned Git child or its path-exact,
    /// in-process cancellation fallback.
    pub fn cleanup(&mut self) -> Result<()> {
        if !self.registered {
            return Ok(());
        }
        let mut remove = git_cmd();
        remove
            .args(["worktree", "remove", "--force"])
            .arg(&self.worktree_path)
            .current_dir(&self.repo_root);
        let output = match crate::proc::output_governed_with_timeout(
            remove,
            "git worktree remove",
            std::time::Duration::from_secs(60),
        ) {
            Ok(output) => output,
            Err(error) => {
                let rollback = prune_registered_worktree(&self.repo_root, &self.worktree_path);
                if matches!(&rollback, Ok(true)) {
                    self.registered = false;
                }
                if crate::governor::is_cancellation(&error) {
                    return Err(error);
                }
                return match rollback {
                    Ok(true) => Ok(()),
                    Ok(false) => Err(error.context(
                        "git worktree remove failed and the exact registration was not found for rollback",
                    )),
                    Err(rollback) => Err(error.context(format!(
                        "git worktree remove failed and exact registration rollback failed: {rollback}"
                    ))),
                };
            }
        };
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return match prune_registered_worktree(&self.repo_root, &self.worktree_path) {
                Ok(true) => {
                    self.registered = false;
                    Ok(())
                }
                Ok(false) => anyhow::bail!(
                    "git worktree remove failed: {}; exact registration was not found for rollback",
                    stderr.trim()
                ),
                Err(rollback) => anyhow::bail!(
                    "git worktree remove failed: {}; exact registration rollback also failed: {rollback}",
                    stderr.trim()
                ),
            };
        }
        self.registered = false;
        Ok(())
    }
}

/// Create an ephemeral detached worktree of `commit` under a fresh temp dir.
pub fn create_worktree_snapshot(repo_root: &Path, commit: &str) -> Result<WorktreeSnapshot> {
    // Resolve symbolic inputs once, before creating the checkout. All later
    // integrity comparisons use this immutable source identity.
    let original_target_sha = git2::Repository::discover(repo_root)?
        .revparse_single(commit)?
        .peel_to_commit()?
        .id()
        .to_string();
    let tmp = tempfile::tempdir()?;
    // `git worktree add` wants a path it can create, so point it at a fresh
    // subdirectory of the temp dir rather than the (already-created) temp root.
    let worktree_path = tmp.path().join("snapshot");
    // A reviewed commit is input data, not an operator checkout. In particular,
    // `worktree add` must not execute an inherited/global post-checkout hook:
    // that hook can require ambient tools, mutate the snapshot, or inspect an
    // unrelated checkout. Point Git at an empty, snapshot-owned hook directory
    // without changing the repository's persistent configuration.
    let hooks_path = tmp.path().join("hooks");
    std::fs::create_dir(&hooks_path)?;
    // Armed before the child starts: if cancellation/timeout wins after Git has
    // registered the path but before the command returns, Drop can still undo
    // that exact administrative entry in-process.
    let mut registration_rollback = WorktreeRegistrationRollback::new(repo_root, &worktree_path);

    let mut hooks_config = std::ffi::OsString::from("core.hooksPath=");
    hooks_config.push(&hooks_path);
    let mut command = git_cmd();
    command
        .arg("-c")
        .arg(hooks_config)
        .args(["worktree", "add", "--detach", "--force"])
        .arg(&worktree_path)
        .arg(&original_target_sha)
        .current_dir(repo_root);
    let output = crate::proc::output_governed_with_timeout(
        command,
        "git worktree add",
        std::time::Duration::from_secs(60),
    )?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("git worktree add failed: {}", stderr.trim());
    }

    // Symlink untracked dependencies (node_modules and .venv) to bypass reinstall overhead.
    // A failed borrow is terminal instead of silently leaving a snapshot whose
    // JS eligibility was decided from the operator checkout but whose toolchain
    // is absent at execution time. A target is allowed to commit files under
    // node_modules; preserve that directory and borrow only missing top-level
    // dependency entries in that case. Linking `.bin` alone is insufficient for
    // npm/pnpm shims because they resolve sibling package paths such as
    // `../eslint` from the snapshot.
    #[cfg(unix)]
    {
        let mut borrowed_links = Vec::new();
        let nm = repo_root.join("node_modules");
        let snapshot_nm = worktree_path.join("node_modules");
        if nm.exists() {
            if !snapshot_nm.exists() {
                create_borrowed_link(&nm, &snapshot_nm, &worktree_path, &mut borrowed_links)?;
            } else {
                let ambient_bin = nm.join(".bin");
                let snapshot_bin = snapshot_nm.join(".bin");
                if ambient_bin.exists() {
                    link_missing_entries(&nm, &snapshot_nm, &worktree_path, &mut borrowed_links)?;
                    if std::fs::symlink_metadata(&snapshot_bin)
                        .is_ok_and(|metadata| metadata.file_type().is_dir())
                    {
                        link_missing_entries(
                            &ambient_bin,
                            &snapshot_bin,
                            &worktree_path,
                            &mut borrowed_links,
                        )?;
                    }
                }
            }
        }
        let venv = repo_root.join(".venv");
        let snapshot_venv = worktree_path.join(".venv");
        if venv.exists() && !snapshot_venv.exists() {
            create_borrowed_link(&venv, &snapshot_venv, &worktree_path, &mut borrowed_links)?;
        }
        write_borrowed_links_manifest(tmp.path(), &mut borrowed_links)?;
    }

    registration_rollback.disarm();
    Ok(WorktreeSnapshot {
        repo_root: repo_root.to_path_buf(),
        worktree_path,
        original_target_sha,
        registered: true,
        _tmp: tmp,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo_with_commit() -> (tempfile::TempDir, git2::Repository) {
        let tmp = tempfile::tempdir().expect("repo tempdir");
        let repo = git2::Repository::init(tmp.path()).expect("init repo");
        let mut config = repo.config().expect("repo config");
        config.set_str("user.name", "prview test").expect("name");
        config
            .set_str("user.email", "prview@example.test")
            .expect("email");
        drop(config);
        let tree_id = repo.index().expect("index").write_tree().expect("tree id");
        {
            let tree = repo.find_tree(tree_id).expect("tree");
            let signature = repo.signature().expect("signature");
            repo.commit(Some("HEAD"), &signature, &signature, "initial", &tree, &[])
                .expect("initial commit");
        }
        (tmp, repo)
    }

    fn registered_paths(repo: &git2::Repository) -> Vec<PathBuf> {
        repo.worktrees()
            .expect("worktree names")
            .iter()
            .flatten()
            .map(|name| {
                repo.find_worktree(name)
                    .expect("registered worktree")
                    .path()
                    .to_path_buf()
            })
            .collect()
    }

    #[test]
    fn registration_rollback_is_path_exact_and_handles_locked_worktrees() {
        let (repo_tmp, repo) = repo_with_commit();
        let worktrees_tmp = tempfile::tempdir().expect("worktree tempdir");
        let candidate_path = worktrees_tmp.path().join("candidate");
        let control_path = worktrees_tmp.path().join("control");
        let candidate = repo
            .worktree("candidate", &candidate_path, None)
            .expect("candidate worktree");
        candidate
            .lock(Some("partial registration"))
            .expect("lock candidate");
        let _control = repo
            .worktree("control", &control_path, None)
            .expect("control worktree");

        drop(WorktreeRegistrationRollback::new(
            repo_tmp.path(),
            &candidate_path,
        ));

        let paths = registered_paths(&repo);
        assert!(
            !paths
                .iter()
                .any(|path| comparable_worktree_path(path)
                    == comparable_worktree_path(&candidate_path)),
            "the exact partial registration must be removed"
        );
        assert!(
            paths
                .iter()
                .any(|path| comparable_worktree_path(path)
                    == comparable_worktree_path(&control_path)),
            "rollback must not prune a sibling worktree"
        );
    }

    #[test]
    fn registration_rollback_skips_an_unreadable_sibling_before_the_exact_target() {
        let (repo_tmp, repo) = repo_with_commit();
        let worktrees_tmp = tempfile::tempdir().expect("worktree tempdir");
        let stale_path = worktrees_tmp.path().join("stale");
        let healthy_path = worktrees_tmp.path().join("healthy");
        let target_path = worktrees_tmp.path().join("target");
        let _stale = repo
            .worktree("a-stale", &stale_path, None)
            .expect("stale sibling registration");
        let _healthy = repo
            .worktree("m-healthy", &healthy_path, None)
            .expect("healthy sibling registration");
        let target = repo
            .worktree("z-target", &target_path, None)
            .expect("target registration");
        target
            .lock(Some("exact rollback target"))
            .expect("lock target");

        std::fs::remove_file(repo.path().join("worktrees/a-stale/gitdir"))
            .expect("make the sibling registration unreadable");
        assert!(repo.find_worktree("a-stale").is_err());
        assert!(
            prune_registered_worktree(repo_tmp.path(), &target_path)
                .expect("a stale sibling must not abort the exact lookup")
        );

        let names = repo.worktrees().expect("remaining worktree names");
        assert!(names.iter().flatten().any(|name| name == "m-healthy"));
        assert!(!names.iter().flatten().any(|name| name == "z-target"));
    }

    #[cfg(unix)]
    #[test]
    fn snapshot_creation_does_not_execute_checkout_hooks() {
        use std::os::unix::fs::PermissionsExt;

        let (repo_tmp, repo) = repo_with_commit();
        let hooks = repo_tmp.path().join("operator-hooks");
        std::fs::create_dir(&hooks).expect("hooks dir");
        let marker = repo_tmp.path().join("post-checkout-ran");
        let hook = hooks.join("post-checkout");
        std::fs::write(
            &hook,
            format!(
                "#!/bin/sh\nprintf called > '{}'\nexit 1\n",
                marker.display()
            ),
        )
        .expect("write hook");
        let mut permissions = std::fs::metadata(&hook)
            .expect("hook metadata")
            .permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&hook, permissions).expect("make hook executable");
        repo.config()
            .expect("repo config")
            .set_str("core.hooksPath", hooks.to_str().expect("utf8 temp path"))
            .expect("configure hooks");

        let head = repo.head().unwrap().target().unwrap().to_string();
        let snapshot = create_worktree_snapshot(repo_tmp.path(), &head)
            .expect("operator hooks must not participate in snapshot creation");
        assert!(snapshot.worktree_path.is_dir());
        assert!(!marker.exists(), "post-checkout hook must stay isolated");
    }

    #[cfg(unix)]
    #[test]
    fn snapshot_borrows_untracked_node_modules_with_a_real_symlink() {
        let (repo_tmp, repo) = repo_with_commit();
        let node_modules = repo_tmp.path().join("node_modules");
        std::fs::create_dir(&node_modules).expect("node_modules");
        std::fs::write(node_modules.join("marker"), "operator dependency\n")
            .expect("dependency marker");
        let head = repo.head().unwrap().target().unwrap().to_string();

        let snapshot = create_worktree_snapshot(repo_tmp.path(), &head).expect("snapshot");
        let borrowed = snapshot.worktree_path.join("node_modules");
        assert!(
            borrowed.is_symlink(),
            "borrow must be visible to provenance"
        );
        assert_eq!(
            std::fs::canonicalize(&borrowed).expect("borrow target"),
            std::fs::canonicalize(&node_modules).expect("operator dependencies"),
        );
        assert_eq!(
            std::fs::read_to_string(borrowed.join("marker")).expect("borrowed marker"),
            "operator dependency\n",
        );
    }

    #[cfg(unix)]
    #[test]
    fn ordinary_drop_deregisters_in_process_without_spawning_git() {
        use std::os::unix::fs::PermissionsExt;

        let (repo_tmp, _repo) = repo_with_commit();
        let head = git2::Repository::open(repo_tmp.path())
            .expect("open repo")
            .head()
            .expect("head")
            .target()
            .expect("head oid")
            .to_string();
        let snapshot = create_worktree_snapshot(repo_tmp.path(), &head).expect("snapshot");
        let snapshot_path = snapshot.worktree_path.clone();
        let marker = repo_tmp.path().join("drop-spawned-git");
        let shim = repo_tmp.path().join("git-shim");
        std::fs::write(
            &shim,
            format!(
                "#!/bin/sh\nprintf called > '{}'\nexit 1\n",
                marker.display()
            ),
        )
        .expect("write git shim");
        let mut permissions = std::fs::metadata(&shim)
            .expect("shim metadata")
            .permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&shim, permissions).expect("make shim executable");

        let _override = crate::git::override_test_git_program(shim);
        drop(snapshot);

        assert!(!marker.exists(), "Drop must not start a git child");
        let repo = git2::Repository::open(repo_tmp.path()).expect("reopen repo");
        assert!(
            registered_paths(&repo)
                .iter()
                .all(|path| comparable_worktree_path(path)
                    != comparable_worktree_path(&snapshot_path)),
            "Drop must prune the exact registration"
        );
        assert!(
            !snapshot_path.exists(),
            "TempDir still owns checkout cleanup"
        );
    }

    #[tokio::test]
    async fn cancelled_drop_deregisters_an_existing_snapshot_in_process() {
        let (repo_tmp, _repo) = repo_with_commit();
        let head = git2::Repository::open(repo_tmp.path())
            .expect("open repo")
            .head()
            .expect("head")
            .target()
            .expect("head oid")
            .to_string();
        let snapshot = create_worktree_snapshot(repo_tmp.path(), &head).expect("snapshot");
        let snapshot_path = snapshot.worktree_path.clone();
        let governor = std::sync::Arc::new(crate::governor::ResourceGovernor::new());
        governor.cancel();

        crate::governor::with_run_scope(governor, async move {
            drop(snapshot);
        })
        .await;

        let repo = git2::Repository::open(repo_tmp.path()).expect("reopen repo");
        assert!(
            registered_paths(&repo)
                .iter()
                .all(|path| comparable_worktree_path(path)
                    != comparable_worktree_path(&snapshot_path)),
            "cancelled Drop must not leave a worktree registration"
        );
        assert!(
            !snapshot_path.exists(),
            "the snapshot tempdir still owns filesystem cleanup"
        );
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn cancelled_worktree_add_rolls_back_a_completed_registration() {
        use std::os::unix::fs::PermissionsExt;

        let (repo_tmp, repo) = repo_with_commit();
        let repo_root = repo_tmp.path().to_path_buf();
        let baseline = registered_paths(&repo).len();
        let ready = repo_tmp.path().join("worktree-add-ready");
        let shim = repo_tmp.path().join("worktree-add-shim");
        std::fs::write(
            &shim,
            format!(
                "#!/bin/sh\ngit \"$@\"\nstatus=$?\nif [ \"$3\" = worktree ] && [ \"$4\" = add ] && [ \"$status\" -eq 0 ]; then\n  printf '%s\\n' \"$7\" > '{}'\n  sleep 30\nfi\nexit \"$status\"\n",
                ready.display()
            ),
        )
        .expect("write git shim");
        let mut permissions = std::fs::metadata(&shim)
            .expect("shim metadata")
            .permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&shim, permissions).expect("make shim executable");

        let governor = std::sync::Arc::new(crate::governor::ResourceGovernor::new());
        let canceller = {
            let governor = std::sync::Arc::clone(&governor);
            let ready = ready.clone();
            std::thread::spawn(move || {
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
                while !ready.exists() {
                    if std::time::Instant::now() >= deadline {
                        governor.cancel();
                        panic!("git shim never completed worktree registration");
                    }
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                governor.cancel();
            })
        };
        let result =
            crate::governor::with_run_scope(std::sync::Arc::clone(&governor), async move {
                crate::governor::blocking_stage(|| {
                    let _override = crate::git::override_test_git_program(shim);
                    create_worktree_snapshot(&repo_root, "HEAD")
                })
            })
            .await;
        canceller.join().expect("canceller");

        let error = match result {
            Ok(_) => panic!("cancellation must interrupt the worktree-add shim"),
            Err(error) => error,
        };
        assert!(crate::governor::is_cancellation(&error), "{error:#}");
        assert_eq!(governor.inflight_count(), 0);
        let registered_path = PathBuf::from(
            std::fs::read_to_string(&ready)
                .expect("registered path receipt")
                .trim(),
        );
        let repo = git2::Repository::open(repo_tmp.path()).expect("reopen repo");
        assert_eq!(
            registered_paths(&repo).len(),
            baseline,
            "cancelled add must restore the registration count"
        );
        assert!(
            !registered_path.exists(),
            "cancelled add must also release its TempDir-owned checkout"
        );
    }
}
