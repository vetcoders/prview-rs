//! Isolated git command builder.
//!
//! Every `Command::new("git")` in this codebase should use [`git_cmd`] instead.
//! It strips inherited `GIT_*` environment variables that leak repo context
//! from parent processes (hooks, worktrees, editors). Without this, a `git init`
//! in a temp directory can silently commit to the real repository.

use std::process::Command;

/// Environment variables that leak parent repo context into child git processes.
const GIT_ENV_VARS: &[&str] = &[
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_COMMON_DIR",
];

/// Create a `Command` for git with inherited repo context stripped.
///
/// Use this instead of `Command::new("git")` everywhere — production and test code alike.
/// The caller still needs to set `.current_dir()` and `.args()` as usual.
///
/// ```ignore
/// use crate::git::cmd::git_cmd;
///
/// let output = git_cmd()
///     .args(["diff", "HEAD", "--stat"])
///     .current_dir(&repo_root)
///     .output()?;
/// ```
pub fn git_cmd() -> Command {
    let mut cmd = Command::new("git");
    for var in GIT_ENV_VARS {
        cmd.env_remove(var);
    }
    // prview is a non-interactive tool: git must fail fast instead of
    // prompting for credentials (fetch/ls-remote over https on a repo the
    // operator is not authenticated for would otherwise hang the run on an
    // invisible prompt). Stdin is detached for the same reason; nothing in
    // this codebase feeds stdin to git.
    cmd.env("GIT_TERMINAL_PROMPT", "0");
    cmd.stdin(std::process::Stdio::null());
    cmd
}
