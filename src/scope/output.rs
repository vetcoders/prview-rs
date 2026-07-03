//! Output generation for scoped review packs.

use crate::git::{CommitInfo, short_sha};
use anyhow::{Context, Result};
use std::fs;
use std::path::Path;

/// Parameters for SCOPE.md generation.
pub struct ScopeMdParams<'a> {
    pub dir: &'a Path,
    pub include: &'a [String],
    pub exclude: &'a [String],
    pub wip: bool,
    pub base_ref: &'a str,
    pub target_ref: &'a str,
    pub scoped_files: &'a [&'a str],
    pub total_files: usize,
    /// WIP-derived files in scope. Listed in SCOPE.md (marked `(WIP)`) so a
    /// reviewer sees WIP-only files even though they never appear in the
    /// committed diff. Committed stats above are deliberately left untouched.
    pub wip_scoped_files: &'a [&'a str],
    pub scoped_commits: &'a [&'a CommitInfo],
    pub total_commits: usize,
}

/// Write SCOPE.md summarizing the scope configuration and results.
pub fn write_scope_md(params: &ScopeMdParams<'_>) -> Result<()> {
    let mut content = String::from("# Scoped Review Pack\n\n");

    content.push_str(&format!("**Base:** {}\n", params.base_ref));
    content.push_str(&format!("**Target:** {}\n", params.target_ref));
    if params.wip {
        content.push_str("**Mode:** committed + WIP (staged/unstaged)\n");
    }
    content.push('\n');

    if !params.include.is_empty() {
        content.push_str("## Include patterns\n\n");
        for p in params.include {
            content.push_str(&format!("- `{p}`\n"));
        }
        content.push('\n');
    }

    if !params.exclude.is_empty() {
        content.push_str("## Exclude patterns\n\n");
        for p in params.exclude {
            content.push_str(&format!("- `{p}`\n"));
        }
        content.push('\n');
    }

    content.push_str(&format!(
        "## Stats\n\n- Files in scope: **{}** / {}\n- Commits in scope: **{}** / {}\n\n",
        params.scoped_files.len(),
        params.total_files,
        params.scoped_commits.len(),
        params.total_commits,
    ));

    content.push_str("## Files\n\n");
    for file in params.scoped_files {
        content.push_str(&format!("- `{file}`\n"));
    }
    // WIP-only files (not already listed among committed files) get an explicit
    // marker so they are visible without being conflated with committed changes.
    for file in params.wip_scoped_files {
        if !params.scoped_files.contains(file) {
            content.push_str(&format!("- `{file}` (WIP)\n"));
        }
    }

    fs::write(params.dir.join("SCOPE.md"), content).context("Failed to write SCOPE.md")?;
    Ok(())
}

/// Write commits.log with the scoped commit list.
pub fn write_commits_log(dir: &Path, commits: &[&CommitInfo]) -> Result<()> {
    let mut content = String::new();
    for commit in commits {
        content.push_str(&format!(
            "{} {} <{}> {}\n  {}\n\n",
            short_sha(&commit.id),
            commit.date,
            commit.author,
            commit.email,
            commit.message,
        ));
    }

    fs::write(dir.join("commits.log"), content).context("Failed to write commits.log")?;
    Ok(())
}

/// Write full.patch (scoped unified diff).
pub fn write_full_patch(dir: &Path, patch: &str) -> Result<()> {
    fs::write(dir.join("full.patch"), patch).context("Failed to write full.patch")?;
    Ok(())
}

/// Write per-commit patches to per-commit/ directory.
pub fn write_per_commit_patches(
    dir: &Path,
    patches: &[(usize, &CommitInfo, String)],
) -> Result<()> {
    let commit_dir = dir.join("per-commit");
    fs::create_dir_all(&commit_dir).context("Failed to create per-commit directory")?;

    for (idx, commit, patch) in patches {
        if patch.is_empty() {
            continue;
        }
        let slug = slugify(&commit.message);
        let filename = format!("{:02}_{}_{}.patch", idx, short_sha(&commit.id), slug);
        fs::write(commit_dir.join(&filename), patch)
            .with_context(|| format!("Failed to write {filename}"))?;
    }

    Ok(())
}

/// Write per-file patches to per-file/ directory.
pub fn write_per_file_patches(dir: &Path, patches: &[(&str, String)]) -> Result<()> {
    let file_dir = dir.join("per-file");
    fs::create_dir_all(&file_dir).context("Failed to create per-file directory")?;

    for (path, patch) in patches {
        if patch.is_empty() {
            continue;
        }
        // Reversible, collision-free encoding: `a/b.rs` -> `a%2Fb.rs`. The old
        // `/`->`--` substitution collided (`a/b.rs` and `a--b.rs` both mapped to
        // `a--b.rs`), so one patch could silently overwrite another.
        let filename = format!("{}.patch", crate::config::branch_storage_key(path));
        fs::write(file_dir.join(&filename), patch)
            .with_context(|| format!("Failed to write per-file patch for {path}"))?;
    }

    Ok(())
}

/// Write WIP patch.
pub fn write_wip_patch(dir: &Path, patch: &str) -> Result<()> {
    if patch.is_empty() {
        return Ok(());
    }
    fs::write(dir.join("wip.patch"), patch).context("Failed to write wip.patch")?;
    Ok(())
}

/// Convert a commit message to a filename-safe slug.
fn slugify(msg: &str) -> String {
    let slug: String = msg
        .chars()
        .take(40)
        .map(|c| {
            if c.is_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect();
    slug.trim_matches('-').to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugify_basic() {
        assert_eq!(
            slugify("fix: payment CTA button"),
            "fix--payment-cta-button"
        );
    }

    #[test]
    fn slugify_long_message() {
        let msg = "a".repeat(100);
        assert_eq!(slugify(&msg).len(), 40);
    }

    #[test]
    fn slugify_special_chars() {
        assert_eq!(
            slugify("feat(scope): add --include flag"),
            "feat-scope---add---include-flag"
        );
    }

    /// Regression (#9): paths that previously collided under the `/`->`--`
    /// substitution (`a/b.rs` vs `a-b.rs`) must map to distinct per-file patch
    /// filenames, preserving each patch.
    #[test]
    fn per_file_filenames_do_not_collide() {
        let dir = tempfile::tempdir().unwrap();
        let patches: Vec<(&str, String)> = vec![
            ("a/b.rs", "SLASH_PATCH\n".to_string()),
            ("a-b.rs", "DASH_PATCH\n".to_string()),
        ];
        write_per_file_patches(dir.path(), &patches).unwrap();

        let pf = dir.path().join("per-file");
        let count = std::fs::read_dir(&pf)
            .unwrap()
            .filter_map(|e| e.ok())
            .count();
        assert_eq!(
            count, 2,
            "colliding-looking paths must yield distinct files"
        );

        let slash = std::fs::read_to_string(pf.join(format!(
            "{}.patch",
            crate::config::branch_storage_key("a/b.rs")
        )))
        .unwrap();
        assert_eq!(slash, "SLASH_PATCH\n");
        let dash = std::fs::read_to_string(pf.join(format!(
            "{}.patch",
            crate::config::branch_storage_key("a-b.rs")
        )))
        .unwrap();
        assert_eq!(dash, "DASH_PATCH\n");
    }
}
