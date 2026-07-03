//! Scoped review pack generator.
//!
//! Generates a minimal diff/review pack filtered to only the files
//! and commits matching user-provided glob patterns.

mod filter;
mod output;

use crate::cli::ScopeArgs;
use crate::git::{self, CommitInfo, Repository, ResolvedRef};
use anyhow::{Context, Result, bail};
use colored::Colorize;
use std::path::Path;

/// Marker file written into every generated scope pack. Its presence — and a
/// valid `schema` field — is the *only* thing that authorises `prview` to wipe
/// an existing output directory. A bare `SCOPE.md` is deliberately not enough:
/// an operator's own directory can happen to contain a file by that name.
const SCOPE_PACK_MARKER: &str = ".prview-scope-pack.json";
/// Schema tag stored inside [`SCOPE_PACK_MARKER`]. Bump when the marker format
/// changes; the delete guard only trusts markers carrying this exact value.
const SCOPE_PACK_SCHEMA: &str = "prview-scope-pack/v1";

/// Run the scope command: generate a scoped review pack.
pub fn run(args: &ScopeArgs) -> Result<()> {
    // Validate: at least one of include/exclude required
    if args.include.is_empty() && args.exclude.is_empty() {
        bail!("At least one of --include or --exclude is required");
    }

    // Resolve the real repository root by walking up from the working dir, so
    // `prview scope` works from any subdirectory. Using `current_dir()` as the
    // repo root broke every invocation launched below the repo top level.
    let cwd = std::env::current_dir().context("Failed to get current directory")?;
    let repo_root = crate::config::find_repo_root_from(&cwd)
        .context("Not inside a git repository (no .git in the current or any parent directory)")?;
    let repo = Repository::open(&repo_root)?;

    // Build scope filter
    let scope_filter =
        filter::ScopeFilter::new(&args.include, &args.exclude).context("Invalid glob pattern")?;

    // Resolve target (HEAD) and base
    let target = resolve_target(&repo)?;
    let base = resolve_base(&repo, args.base.as_deref())?;

    // Diff from the merge-base of base and target, not the base tip. A tip-to-tip
    // diff surfaces changes that landed on the base branch after divergence (as
    // spurious reversed hunks) instead of only the target's own changes. Keep the
    // base *name* for display; only the commit we diff against changes. If the
    // histories are unrelated (no merge-base) we fall back to the base tip.
    let base = {
        let effective_commit = repo
            .merge_base(&base.commit_id, &target.commit_id)
            .unwrap_or_else(|_| base.commit_id.clone());
        ResolvedRef {
            name: base.name,
            commit_id: effective_commit,
            is_remote: base.is_remote,
        }
    };

    println!(
        "{} Scope: {} → {}",
        "→".cyan(),
        git::short_sha(&base.commit_id),
        git::short_sha(&target.commit_id),
    );

    // Generate diff to get file list and commits
    let diff = repo.diff_refs(&base, &target)?;
    let all_file_paths: Vec<String> = diff.files.iter().map(|f| f.path.clone()).collect();
    let scoped_files = scope_filter.filter_paths(&all_file_paths);

    // WIP files are derived independently of the committed diff: list every
    // file with working-tree changes, then run it through the same scope
    // filter. Deriving from the committed diff would miss WIP-only files and
    // would mishandle exclude-only scopes.
    let wip_scoped: Vec<String> = if args.wip {
        repo.wip_files_scoped(&[])?
            .into_iter()
            .filter(|f| scope_filter.matches(f))
            .collect()
    } else {
        Vec::new()
    };

    if scoped_files.is_empty() && wip_scoped.is_empty() {
        println!(
            "{} No files match the scope. Nothing to generate.",
            "ℹ".blue()
        );
        return Ok(());
    }

    // Filter commits: keep only those touching at least one scoped file.
    // When the committed scope is empty (we only got here via WIP-matched
    // files), skip commit filtering entirely: an empty pathspec would match
    // every file and wrongly pull in all commits.
    let scoped_commits = if scoped_files.is_empty() {
        Vec::new()
    } else {
        filter_commits_by_scope(&repo, &diff.commits, &scoped_files)?
    };

    println!(
        "{} Scope: {} files, {} commits (from {} files, {} commits total)",
        "✓".green(),
        scoped_files.len(),
        scoped_commits.len(),
        all_file_paths.len(),
        diff.commits.len(),
    );

    // Create output directory (guarded: never blow away a directory that
    // isn't a previously generated scope pack).
    let out_dir = &args.output;
    if out_dir.exists() {
        guard_output_dir(out_dir, &cwd, &repo_root)?;
        std::fs::remove_dir_all(out_dir)
            .with_context(|| format!("Failed to clean output dir {}", out_dir.display()))?;
    }
    std::fs::create_dir_all(out_dir)
        .with_context(|| format!("Failed to create output dir {}", out_dir.display()))?;
    // Stamp the pack marker immediately so a later re-run recognises this
    // directory as a prior scope pack and is allowed to clean it.
    write_scope_pack_marker(out_dir)?;

    // Generate full.patch (scoped)
    let full_patch = generate_scoped_full_patch(&repo, &base, &target, &scoped_files)?;
    output::write_full_patch(out_dir, &full_patch)?;

    // Generate per-commit patches (scoped)
    let per_commit = generate_per_commit_patches(&repo, &scoped_commits, &scoped_files)?;
    output::write_per_commit_patches(out_dir, &per_commit)?;

    // Generate per-file patches (optional)
    if args.per_file {
        let per_file = generate_per_file_patches(&repo, &base, &target, &scoped_files)?;
        output::write_per_file_patches(out_dir, &per_file)?;
    }

    // WIP mode — scoped to WIP-derived files, not the committed diff.
    if args.wip && !wip_scoped.is_empty() {
        let wip_refs: Vec<&str> = wip_scoped.iter().map(|s| s.as_str()).collect();
        let wip_patch = repo.wip_diff_scoped(&wip_refs)?;
        if !wip_patch.is_empty() {
            output::write_wip_patch(out_dir, &wip_patch)?;
            println!("  {} WIP changes included", "✓".green());
        }
    }

    // Write metadata files
    let wip_scoped_refs: Vec<&str> = wip_scoped.iter().map(|s| s.as_str()).collect();
    output::write_scope_md(&output::ScopeMdParams {
        dir: out_dir,
        include: &args.include,
        exclude: &args.exclude,
        wip: args.wip,
        base_ref: &base.name,
        target_ref: &target.name,
        scoped_files: &scoped_files,
        total_files: all_file_paths.len(),
        wip_scoped_files: &wip_scoped_refs,
        scoped_commits: &scoped_commits,
        total_commits: diff.commits.len(),
    })?;
    output::write_commits_log(out_dir, &scoped_commits)?;

    println!(
        "\n{} Review pack: {}",
        "✓".green().bold(),
        out_dir.display(),
    );
    print_pack_summary(out_dir);

    Ok(())
}

/// Resolve HEAD as target ref.
fn resolve_target(repo: &Repository) -> Result<ResolvedRef> {
    let config = minimal_config();
    repo.resolve_target(&config)
}

/// Resolve base ref (explicit or auto-detect merge-base).
fn resolve_base(repo: &Repository, explicit_base: Option<&str>) -> Result<ResolvedRef> {
    let mut config = minimal_config();
    if let Some(base_name) = explicit_base {
        config.bases = vec![base_name.to_string()];
    } else {
        config.bases = vec![
            "develop".to_string(),
            "main".to_string(),
            "master".to_string(),
        ];
    }
    let bases = repo.resolve_bases(&config)?;
    bases
        .into_iter()
        .next()
        .context(if let Some(name) = explicit_base {
            format!("Could not resolve base ref '{name}'")
        } else {
            "Could not find a base branch (tried develop, main, master). Use --base to specify one."
                .to_string()
        })
}

/// Build a minimal Config for ref resolution only.
fn minimal_config() -> crate::config::Config {
    use crate::config::{DetectedProfile, ProfileKind};
    use crate::policy::PolicyConfig;

    let mut config = crate::config::Config::base(
        std::env::current_dir().unwrap_or_default(),
        DetectedProfile {
            kind: ProfileKind::Generic,
            has_package_json: false,
            has_tsconfig: false,
            has_cargo: false,
            has_pyproject: false,
            has_python_source: false,
            has_js_source: false,
            cargo_root: None,
            rust_dirs: vec![],
            is_workspace: false,
        },
        std::path::PathBuf::from(".prview-policy.yml"),
        PolicyConfig::default(),
        None,
    );
    config.quiet = true;
    config
}

/// Filter commits to only those that touch at least one scoped file.
fn filter_commits_by_scope<'a>(
    repo: &Repository,
    commits: &'a [CommitInfo],
    scoped_files: &[&str],
) -> Result<Vec<&'a CommitInfo>> {
    let mut result = Vec::new();

    for commit in commits {
        // Skip sentinel commits from truncation
        if commit.id.starts_with("0000000") {
            continue;
        }

        // Skip merge commits (per scope spec): their first-parent diff
        // duplicates content already carried by the individual commits.
        if repo.is_merge_commit(&commit.id)? {
            continue;
        }

        if repo.commit_touches_paths(&commit.id, scoped_files)? {
            result.push(commit);
        }
    }

    Ok(result)
}

/// Generate a unified patch combining all scoped file diffs.
///
/// Uses a single rename-aware diff rather than per-path diffs so that a rename
/// into the scope carries its old-path deletion (see
/// [`Repository::scoped_full_diff`]); per-path `file_diff` would show only the
/// added new path and silently drop the delete of the old one.
fn generate_scoped_full_patch(
    repo: &Repository,
    base: &ResolvedRef,
    target: &ResolvedRef,
    scoped_files: &[&str],
) -> Result<String> {
    repo.scoped_full_diff(&base.commit_id, &target.commit_id, scoped_files)
}

/// Generate per-commit patches, each scoped to matching files.
fn generate_per_commit_patches<'a>(
    repo: &Repository,
    commits: &[&'a CommitInfo],
    scoped_files: &[&str],
) -> Result<Vec<(usize, &'a CommitInfo, String)>> {
    let mut patches = Vec::new();

    for (idx, commit) in commits.iter().enumerate() {
        let patch = repo.commit_patch_scoped(&commit.id, scoped_files)?;
        patches.push((idx + 1, *commit, patch));
    }

    Ok(patches)
}

/// Generate per-file patches.
fn generate_per_file_patches<'a>(
    repo: &Repository,
    base: &ResolvedRef,
    target: &ResolvedRef,
    scoped_files: &[&'a str],
) -> Result<Vec<(&'a str, String)>> {
    let mut patches = Vec::new();

    for file_path in scoped_files {
        let patch = repo.file_diff(&base.commit_id, &target.commit_id, file_path)?;
        patches.push((*file_path, patch));
    }

    Ok(patches)
}

/// Refuse to delete an output directory that is not a previously generated
/// scope pack. `prview` is a review-safety tool; it must never wipe a path the
/// operator points at with `-o` (a source dir, repo root, home, etc.).
///
/// A directory is safe to clean only when it is *inside the repo* and *looks
/// like a prior scope pack* (contains `SCOPE.md`) or is empty.
fn guard_output_dir(out_dir: &Path, cwd: &Path, repo_root: &Path) -> Result<()> {
    // Relative `-o` paths resolve against the process working directory (where
    // create/remove actually operate), NOT the repo root — otherwise the guard
    // would inspect a different path than the one about to be wiped.
    let abs = if out_dir.is_absolute() {
        out_dir.to_path_buf()
    } else {
        cwd.join(out_dir)
    };
    // Canonicalize the existing target so `..` / symlinks can't dodge the guards.
    let abs = abs.canonicalize().unwrap_or(abs);
    let repo_root = repo_root
        .canonicalize()
        .unwrap_or_else(|_| repo_root.to_path_buf());

    // 1. Filesystem root / no parent.
    if abs.parent().is_none() {
        bail!(
            "Refusing to clean filesystem root as output dir: {}",
            abs.display()
        );
    }
    // 2. Home directory. Canonicalize `$HOME` first so a symlinked home (or a
    //    `$HOME` with a trailing slash / `.` component) still matches the
    //    already-canonicalized `abs`.
    if let Some(home) = std::env::var_os("HOME").map(std::path::PathBuf::from) {
        let home = home.canonicalize().unwrap_or(home);
        if abs == home {
            bail!(
                "Refusing to clean home directory as output dir: {}",
                abs.display()
            );
        }
    }
    // 3. The repo root itself, or any ancestor of it.
    if abs == repo_root || repo_root.starts_with(&abs) {
        bail!(
            "Refusing to clean {} — it is the repo root or an ancestor of it",
            abs.display()
        );
    }
    // 4. Must live inside the repo. An absolute path outside the repo is rejected.
    if !abs.starts_with(&repo_root) {
        bail!(
            "Refusing to clean {} — output dir must be inside the repository ({})",
            abs.display(),
            repo_root.display()
        );
    }
    // 5. Must carry our pack marker (or be empty). A bare `SCOPE.md` is NOT
    //    accepted: only a directory we created (schema-tagged marker) is safe
    //    to wipe. This closes the collision where an operator's own directory
    //    holding an unrelated `SCOPE.md` would be destroyed.
    let is_empty = std::fs::read_dir(&abs)
        .map(|mut rd| rd.next().is_none())
        .unwrap_or(false);
    if !is_empty && !dir_has_scope_pack_marker(&abs) {
        bail!(
            "Refusing to clean {} — it is not empty and does not carry a prview scope-pack \
             marker ({SCOPE_PACK_MARKER}). Packs created before this marker existed, or any \
             unrelated directory, must be removed manually or point -o elsewhere.",
            abs.display()
        );
    }
    Ok(())
}

/// Write the schema-tagged pack marker into a freshly created scope pack.
fn write_scope_pack_marker(dir: &Path) -> Result<()> {
    let body = format!("{{\"schema\":\"{SCOPE_PACK_SCHEMA}\"}}\n");
    std::fs::write(dir.join(SCOPE_PACK_MARKER), body)
        .with_context(|| format!("Failed to write scope-pack marker in {}", dir.display()))?;
    Ok(())
}

/// True only when `dir` contains a parseable pack marker whose `schema` field
/// matches [`SCOPE_PACK_SCHEMA`]. Any read/parse failure or schema mismatch is
/// treated as "not our pack" — the guard must fail closed.
fn dir_has_scope_pack_marker(dir: &Path) -> bool {
    let Ok(content) = std::fs::read_to_string(dir.join(SCOPE_PACK_MARKER)) else {
        return false;
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&content) else {
        return false;
    };
    value.get("schema").and_then(|s| s.as_str()) == Some(SCOPE_PACK_SCHEMA)
}

/// Print a summary of generated files.
fn print_pack_summary(dir: &Path) {
    let entries = [
        ("SCOPE.md", "scope metadata"),
        ("commits.log", "commit log"),
        ("full.patch", "unified diff"),
    ];

    for (file, desc) in entries {
        if dir.join(file).exists() {
            println!("  {} {file} ({desc})", "·".dimmed());
        }
    }

    let per_commit = dir.join("per-commit");
    if per_commit.exists()
        && let Ok(rd) = std::fs::read_dir(&per_commit)
    {
        let count = rd.filter_map(|e| e.ok()).count();
        if count > 0 {
            println!("  {} per-commit/ ({count} patches)", "·".dimmed());
        }
    }

    let per_file = dir.join("per-file");
    if per_file.exists()
        && let Ok(rd) = std::fs::read_dir(&per_file)
    {
        let count = rd.filter_map(|e| e.ok()).count();
        if count > 0 {
            println!("  {} per-file/ ({count} patches)", "·".dimmed());
        }
    }

    if dir.join("wip.patch").exists() {
        println!("  {} wip.patch (uncommitted changes)", "·".dimmed());
    }
}
