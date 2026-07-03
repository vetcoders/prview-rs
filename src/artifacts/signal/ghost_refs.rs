//! Ghost References Scan — detects dangling usages of removed files.

use crate::checks::{CheckResult, CheckStatus};
use crate::git::{Diff, FileStatus, Repository};
use crate::paths::normalize_path_display;
use std::collections::{HashMap, HashSet};
use std::fmt::Write as FmtWrite;
use std::fs;
use std::path::Path;
use walkdir::WalkDir;

const MAX_GHOST_REFS_PER_DELETED_FILE: usize = 50;

/// Category of a ghost reference based on the file where it was found.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GhostRefCategory {
    RuntimeCode,
    BuildArtifact,
    LogOrTemp,
    GeneratedAsset,
}

impl GhostRefCategory {
    /// Classify a relative file path into a category.
    fn classify(rel_path: &str) -> Self {
        let p = Path::new(rel_path);

        // Check directory components for build artifact dirs
        let build_dirs = [
            "target/",
            "dist/",
            "build/",
            "node_modules/",
            ".next/",
            "__pycache__/",
            "coverage/",
            "vendor/",
        ];
        for bd in &build_dirs {
            if rel_path.starts_with(bd) || rel_path.contains(&format!("/{bd}")) {
                return GhostRefCategory::BuildArtifact;
            }
        }

        // Check for log/temp dirs and extensions
        let log_temp_dirs = ["logs/", "tmp/"];
        for ltd in &log_temp_dirs {
            if rel_path.starts_with(ltd) || rel_path.contains(&format!("/{ltd}")) {
                return GhostRefCategory::LogOrTemp;
            }
        }

        if let Some(ext) = p.extension().and_then(|e| e.to_str())
            && matches!(ext, "log" | "tmp" | "bak")
        {
            return GhostRefCategory::LogOrTemp;
        }

        // Check for generated assets
        let generated_extensions: &[&str] =
            &["min.js", "min.css", "bundle.js", "css.map", "js.map"];
        let lossy = rel_path.to_string();
        for ge in generated_extensions {
            if lossy.ends_with(&format!(".{ge}")) {
                return GhostRefCategory::GeneratedAsset;
            }
        }

        if let Some(ext) = p.extension().and_then(|e| e.to_str()) {
            match ext {
                "html" => return GhostRefCategory::GeneratedAsset,
                "map" => return GhostRefCategory::GeneratedAsset, // catches .css.map, .js.map via extension
                _ => {}
            }

            // .d.ts files: check the stem for ".d"
            if ext == "ts"
                && let Some(stem) = p.file_stem().and_then(|s| s.to_str())
                && stem.ends_with(".d")
            {
                return GhostRefCategory::GeneratedAsset;
            }
        }

        // Check for runtime code extensions in appropriate dirs
        let runtime_exts = [
            "rs", "ts", "tsx", "js", "jsx", "py", "go", "java", "rb", "swift", "kt",
        ];
        let runtime_dirs = ["src/", "lib/", "app/"];

        if let Some(ext) = p.extension().and_then(|e| e.to_str())
            && runtime_exts.contains(&ext)
        {
            let in_runtime_dir = runtime_dirs
                .iter()
                .any(|rd| rel_path.starts_with(rd) || rel_path.contains(&format!("/{rd}")));
            if in_runtime_dir {
                return GhostRefCategory::RuntimeCode;
            }
        }

        // Default: treat as RuntimeCode (conservative — don't hide potentially real findings)
        GhostRefCategory::RuntimeCode
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct GhostRef {
    pub file: String,
    pub line: usize,
    pub content: String,
    pub category: GhostRefCategory,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct GhostRefsAudit {
    pub findings: HashMap<String, Vec<GhostRef>>,
    pub noise_filtered: usize,
    pub noise_categories: HashMap<String, usize>,
}

/// Binary-format extensions that should be skipped entirely.
const SKIP_EXTENSIONS: &[&str] = &[
    "png", "jpg", "zip", "lock", "sarif", "json", "woff", "woff2", "ttf", "eot", "ico", "gif",
    "mp3", "mp4", "webp", "avif", "pdf", "svg",
];

pub fn generate_ghost_refs(
    dir: &Path,
    diffs: &[Diff],
    repo: &Repository,
) -> anyhow::Result<Option<CheckResult>> {
    // 1. Collect deleted files and determine their Search Term.
    //
    // A term maps to a Vec of paths: two deleted files can share an extracted
    // stem (e.g. two `parser.rs` in different modules). A HashMap<term, path>
    // silently dropped the first on collision, erasing a deleted file from the
    // audit — the Vec keeps every colliding deletion.
    let mut deleted_terms_to_file: HashMap<String, Vec<String>> = HashMap::new();
    let mut modified_files = HashSet::new();

    for diff in diffs {
        for file in &diff.files {
            modified_files.insert(file.path.clone());
            if file.status == FileStatus::Deleted {
                let term = extract_search_term(&file.path);
                if term.len() > 3 {
                    // skip very short/common terms
                    deleted_terms_to_file
                        .entry(term)
                        .or_default()
                        .push(file.path.clone());
                }
            }
        }
    }

    if deleted_terms_to_file.is_empty() {
        return Ok(None);
    }

    let repo_root = repo.path();

    // Relocation guard, path-aware. The FULL path is the high-confidence
    // signal: if the exact deleted path still exists in the tree, the file was
    // NOT relocated, so a same-BASENAME file living elsewhere is a mere name
    // collision and must not suppress the audit. Suppressing on basename alone
    // was wrong — an unrelated `b/util.rs` surviving while `a/util.rs` is deleted
    // silenced a real deletion and dropped genuine ghost references from the
    // audit (PR #12 review #18). Basename is only a low-confidence FALLBACK,
    // used when the exact path is gone: a same-basename survivor may then be a
    // relocation (indistinguishable from a collision by path alone, e.g.
    // `src/x.rs` moved to `crates/foo/src/x.rs`), so suppress to avoid
    // relocation noise; otherwise the name is fully gone and it is a real ghost.
    let mut surviving_by_basename: HashMap<String, Vec<String>> = HashMap::new();
    for entry in WalkDir::new(repo_root)
        .max_depth(10)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
    {
        let Ok(rel) = entry.path().strip_prefix(repo_root) else {
            continue;
        };
        // Dot-dir filter on the RELATIVE path: an absolute-path check made every
        // file under a dotted ancestor (a repo living in ~/.config, ~/.vibecrafted,
        // ...) look hidden, skipping the whole tree — fail-open Ok(None).
        if has_hidden_component(rel) {
            continue;
        }
        if let Some(name) = entry.file_name().to_str() {
            surviving_by_basename
                .entry(name.to_string())
                .or_default()
                // Normalize separators: WalkDir yields backslashes on Windows,
                // but the deleted paths this is compared against come from git
                // diff (always forward slashes). No-op on Unix.
                .push(rel.to_string_lossy().replace('\\', "/"));
        }
    }
    deleted_terms_to_file.retain(|_term, deleted_paths| {
        deleted_paths.retain(|deleted_path| {
            let Some(basename) = Path::new(deleted_path).file_name().and_then(|n| n.to_str())
            else {
                return true;
            };
            match surviving_by_basename.get(basename) {
                // Exact path survives → not a relocation → keep despite any
                // basename collision. Path gone but basename survives elsewhere
                // → treat as relocation and suppress. Basename gone → real ghost.
                Some(paths) => paths.iter().any(|p| p == deleted_path),
                None => true,
            }
        });
        !deleted_paths.is_empty()
    });

    if deleted_terms_to_file.is_empty() {
        return Ok(None);
    }

    // 2. Scan the current workdir for these terms in files NOT in diff.
    let mut runtime_findings: HashMap<String, Vec<GhostRef>> = HashMap::new();
    let mut noise_categories: HashMap<String, usize> = HashMap::new();
    let mut noise_filtered: usize = 0;

    for entry in WalkDir::new(repo_root)
        .max_depth(10)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
    {
        let path = entry.path();

        let Ok(rel) = path.strip_prefix(repo_root) else {
            continue;
        };
        // Skip hidden paths (.git, etc) by RELATIVE component, so a repo under a
        // dotted ancestor is not entirely skipped (fail-open Ok(None)).
        if has_hidden_component(rel) {
            continue;
        }
        // Normalize separators so the forward-slash `modified_files` set (from
        // git diff) and should_skip_scan_path patterns match on Windows too.
        // No-op on Unix.
        let rel_path = rel.to_string_lossy().replace('\\', "/");
        let Ok(safe_rel_path) = crate::paths::validate_repo_relative_str(&rel_path) else {
            continue;
        };

        if should_skip_scan_path(&rel_path) {
            continue;
        }

        if modified_files.contains(&rel_path) {
            // Already modified in PR, we assume user knows what they are doing here
            // We want to find references in files UNTOUCHED by PR.
            continue;
        }

        if path
            .extension()
            .is_some_and(|ext| SKIP_EXTENSIONS.contains(&ext.to_str().unwrap_or_default()))
        {
            continue;
        }

        let content =
            crate::paths::read_to_string_within(repo_root, safe_rel_path).unwrap_or_default();
        if content.is_empty() {
            continue;
        }

        let normalized_rel = normalize_path_display(&rel_path, repo_root);
        let category = GhostRefCategory::classify(&normalized_rel);

        for (term, deleted_files) in &deleted_terms_to_file {
            if !content.contains(term) {
                continue;
            }
            for deleted_file in deleted_files {
                if runtime_findings
                    .get(deleted_file)
                    .is_some_and(|refs| refs.len() >= MAX_GHOST_REFS_PER_DELETED_FILE)
                {
                    continue;
                }

                for (ln, line) in content.lines().enumerate() {
                    if references_deleted_module(line, term) {
                        let ghost = GhostRef {
                            file: normalized_rel.clone(),
                            line: ln + 1,
                            content: line.trim().chars().take(120).collect(),
                            category,
                        };

                        match category {
                            GhostRefCategory::RuntimeCode => {
                                let refs =
                                    runtime_findings.entry(deleted_file.clone()).or_default();
                                if refs.len() >= MAX_GHOST_REFS_PER_DELETED_FILE {
                                    break;
                                }
                                refs.push(ghost);
                            }
                            _ => {
                                // Count as noise
                                noise_filtered += 1;
                                let cat_key = match category {
                                    GhostRefCategory::BuildArtifact => "build_artifact",
                                    GhostRefCategory::LogOrTemp => "log_or_temp",
                                    GhostRefCategory::GeneratedAsset => "generated_asset",
                                    GhostRefCategory::RuntimeCode => unreachable!(),
                                };
                                *noise_categories.entry(cat_key.to_string()).or_insert(0) += 1;
                            }
                        }
                    }
                }
            }
        }
    }

    if runtime_findings.is_empty() && noise_filtered == 0 {
        return Ok(None);
    }

    let audit = GhostRefsAudit {
        findings: runtime_findings,
        noise_filtered,
        noise_categories,
    };

    fs::create_dir_all(dir)?;
    fs::write(
        dir.join("GHOST_REFERENCES.json"),
        serde_json::to_string_pretty(&audit)?,
    )?;

    let md = format_ghost_refs(&audit);
    fs::write(dir.join("GHOST_REFERENCES.md"), md)?;

    if audit.findings.is_empty() {
        // All refs were noise — nothing actionable
        return Ok(None);
    }

    let msg = format!(
        "[⚠️ line heuristic] Found likely references to {} removed file name(s) in untouched code ({} noise filtered)",
        audit.findings.len(),
        audit.noise_filtered,
    );

    Ok(Some(CheckResult {
        name: "ghost_refs".to_string(),
        status: CheckStatus::Warnings,
        duration: std::time::Duration::ZERO,
        output: msg,
        cached: false,
        provenance: None,
    }))
}

fn extract_search_term(path: &str) -> String {
    let p = Path::new(path);
    let name = p.file_name().unwrap_or_default().to_string_lossy();
    if name == "index.ts" || name == "index.js" || name == "mod.rs" {
        // use parent dir name
        if let Some(parent) = p.parent() {
            return parent
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string();
        }
    }

    // strip extension
    if let Some(stem) = p.file_stem() {
        return stem.to_string_lossy().to_string();
    }
    name.to_string()
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// True when `line` references the deleted module `term` like a real code or
/// import reference, not an incidental substring.
///
/// The old `line.contains(term)` flooded the report with false positives: the
/// stem of a deleted file (e.g. `signal` from `signal.rs`) matched prose
/// ("high-signal"), plurals ("signals"), and unrelated identifiers
/// (`signal_reasons`, `mixed_signals`) — TOOLING/P1-06. A real dangling
/// reference appears as a path (`signal::Foo`, `crate::signal`), an import/`mod`
/// statement, or a file path (`signal.rs`). We require both:
///   1. the occurrence is a standalone identifier token (word boundaries), and
///   2. it sits in a module-path / import / file-reference context.
fn references_deleted_module(line: &str, term: &str) -> bool {
    let mut search_from = 0;
    while let Some(rel) = line[search_from..].find(term) {
        let idx = search_from + rel;
        let end = idx + term.len();
        search_from = idx + 1;

        let before = &line[..idx];
        let after = &line[end..];

        // Token boundaries: reject `signal_reasons`, `mixed_signals`, `signals`.
        let bounded = before.chars().next_back().is_none_or(|c| !is_word_char(c))
            && after.chars().next().is_none_or(|c| !is_word_char(c));
        if bounded && has_module_context(before, after, line) {
            return true;
        }
    }
    false
}

/// Whether the context around a bounded `term` occurrence looks like a module
/// path, import statement, or file reference (rather than prose).
fn has_module_context(before: &str, after: &str, full_line: &str) -> bool {
    // Rust path usage: `signal::Foo` or `crate::signal`.
    if after.starts_with("::") || before.ends_with("::") {
        return true;
    }

    // File reference: `signal.rs`, `signal.ts`, ...
    if let Some(rest) = after.strip_prefix('.') {
        const SRC_EXTS: &[&str] = &["rs", "ts", "tsx", "js", "jsx", "mjs", "cjs", "py", "go"];
        if SRC_EXTS.iter().any(|ext| rest.starts_with(ext)) {
            return true;
        }
    }

    // Path segment: `.../signal` or `signal/...`.
    if before.ends_with('/') || after.starts_with('/') {
        return true;
    }

    // Import / module statements.
    let trimmed = full_line.trim_start();
    const IMPORT_MARKERS: &[&str] = &["use ", "pub use ", "mod ", "pub mod ", "import ", "from "];
    if IMPORT_MARKERS.iter().any(|m| trimmed.starts_with(m)) {
        return true;
    }

    full_line.contains("require(") || full_line.contains("import(")
}

fn should_skip_scan_path(rel_path: &str) -> bool {
    const SKIPPED_PATH_MARKERS: &[&str] = &["logs/", "/logs/", "archive/", "/archive/"];

    SKIPPED_PATH_MARKERS
        .iter()
        .any(|marker| rel_path.contains(marker))
}

/// True when any component of a repo-relative path starts with a dot (`.git`,
/// `.venv`, ...). Must be evaluated on the RELATIVE path: a substring check on
/// the absolute path treated every file under a dotted ancestor (a repo under
/// ~/.config, ~/.vibecrafted, ...) as hidden and skipped the whole tree.
fn has_hidden_component(rel: &Path) -> bool {
    rel.components().any(|component| {
        matches!(component, std::path::Component::Normal(os)
            if os.to_str().is_some_and(|s| s.starts_with('.')))
    })
}

fn format_ghost_refs(audit: &GhostRefsAudit) -> String {
    let mut md = String::new();
    let _ = writeln!(md, "# Ghost References Scan\n");
    let _ = writeln!(
        md,
        "> ⚠️ **NEEDS VERIFICATION**: *Generated by a naive text heuristic. Manually confirm these are not false matches caused by an incidental name collision (e.g. in comments or imported framework modules).* \n"
    );
    let _ = writeln!(
        md,
        "Found dangling references to files removed in this PR inside completely untouched files. Needs manual cleanup!\n"
    );

    for (deleted_file, refs) in &audit.findings {
        let _ = writeln!(md, "### DELETED: `{}`", deleted_file);
        for r in refs {
            let _ = writeln!(
                md,
                "- GHOST: `{}:{}` -- `{}`",
                r.file,
                r.line,
                r.content.replace('`', "")
            );
        }
        let _ = writeln!(md);
    }

    if audit.noise_filtered > 0 {
        let _ = writeln!(md, "---\n");
        let _ = writeln!(
            md,
            "*{} reference(s) filtered as noise (build artifacts, logs, generated assets).*",
            audit.noise_filtered
        );
        for (cat, count) in &audit.noise_categories {
            let _ = writeln!(md, "- `{}`: {}", cat, count);
        }
    }

    md
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::artifacts::signal::test_helpers::{
        make_diff_with_ids, make_test_repo, mock_file_change,
    };
    use crate::git::{DiffStats, FileStatus, Repository};

    /// Create a git repo inside a non-hidden tempdir.
    ///
    /// `generate_ghost_refs` uses WalkDir and filters any path containing "/.".
    /// On macOS, `TempDir::new()` creates dirs like `/var/folders/.../T/.tmpXXX`
    /// whose dot-prefix causes every file to be skipped. We work around this by
    /// using a prefix without a leading dot.
    fn make_visible_repo(
        file_specs: &[(&str, &str)], // (relative_path, content)
    ) -> (tempfile::TempDir, Repository, String) {
        let outer = tempfile::Builder::new().prefix("ghtest").tempdir().unwrap();
        let repo_dir = outer.path().join("repo");
        fs::create_dir_all(&repo_dir).unwrap();

        let git_repo = git2::Repository::init(&repo_dir).unwrap();
        let sig = git2::Signature::now("Test", "test@test.com").unwrap();

        for &(path, content) in file_specs {
            if let Some(parent) = Path::new(path).parent()
                && !parent.as_os_str().is_empty()
            {
                fs::create_dir_all(repo_dir.join(parent)).unwrap();
            }
            fs::write(repo_dir.join(path), content).unwrap();
        }

        let mut index = git_repo.index().unwrap();
        for &(path, _) in file_specs {
            index.add_path(Path::new(path)).unwrap();
        }
        index.write().unwrap();
        let tree_id = index.write_tree().unwrap();
        let tree = git_repo.find_tree(tree_id).unwrap();
        let oid = git_repo
            .commit(Some("HEAD"), &sig, &sig, "init", &tree, &[])
            .unwrap();

        let repo = Repository::open(&repo_dir).unwrap();
        (outer, repo, oid.to_string())
    }

    #[test]
    fn extract_search_term_handles_index_files() {
        assert_eq!(
            extract_search_term("src/components/button/index.ts"),
            "button"
        );
        assert_eq!(extract_search_term("src/services/auth/mod.rs"), "auth");
        assert_eq!(extract_search_term("src/utils/helpers.ts"), "helpers");
    }

    #[test]
    fn should_skip_scan_path_filters_noisy_runtime_dirs() {
        assert!(should_skip_scan_path("logs/app.log"));
        assert!(should_skip_scan_path("nested/archive/dump.txt"));
        assert!(!should_skip_scan_path("target/debug/build.log"));
        assert!(!should_skip_scan_path("frontend/dist/bundle.js"));
        assert!(!should_skip_scan_path("src/app.rs"));
    }

    #[test]
    fn windows_separators_must_be_normalized_before_scan_path_checks() {
        // Regression (PR #13): WalkDir yields backslash separators on Windows,
        // but should_skip_scan_path's markers ("/logs/", ...) and the git-derived
        // modified_files set both use forward slashes. A raw backslash rel path
        // therefore evades the skip patterns; the scan now normalizes it first.
        let win = "nested\\logs\\app.log";
        assert!(
            !should_skip_scan_path(win),
            "raw backslash path slips past the forward-slash skip markers"
        );
        assert!(
            should_skip_scan_path(&win.replace('\\', "/")),
            "normalized path is correctly recognized and skipped"
        );
    }

    #[test]
    fn references_deleted_module_matches_real_references() {
        // Path usage, import/mod statements and file references are real ghosts.
        assert!(references_deleted_module(
            "    signal::compute();",
            "signal"
        ));
        assert!(references_deleted_module(
            "use crate::artifacts::signal;",
            "signal"
        ));
        assert!(references_deleted_module(
            "use crate::artifacts::signal::Foo;",
            "signal"
        ));
        assert!(references_deleted_module("mod signal;", "signal"));
        assert!(references_deleted_module(
            "// see signal.rs for the old impl",
            "signal"
        ));
        assert!(references_deleted_module(
            "import { x } from './signal'",
            "signal"
        ));
    }

    #[test]
    fn references_deleted_module_ignores_prose_and_identifier_substrings() {
        // These are exactly the 14 false positives the e2e run produced for the
        // deleted `signal.rs` (P1-06): prose, plurals and unrelated identifiers.
        assert!(!references_deleted_module(
            "gh repo edit --description \"High-signal PR review CLI\"",
            "signal"
        ));
        assert!(!references_deleted_module(
            "- Point a reviewer at the highest-signal files first",
            "signal"
        ));
        assert!(!references_deleted_module(
            "collapses three signals into one enum",
            "signal"
        ));
        assert!(!references_deleted_module(
            "fn test_compute_delta_mixed_signals() {",
            "signal"
        ));
        assert!(!references_deleted_module(
            "//! is not a production performance regression signal.",
            "signal"
        ));
        assert!(!references_deleted_module(
            "let signal_reasons = if reasons.prod_reasons.is_empty() {",
            "signal"
        ));
        assert!(!references_deleted_module(
            "// Suppress soft signals (low_coverage, hard_cap) when",
            "signal"
        ));
    }

    #[test]
    fn test_ghost_refs_finds_dangling_reference() {
        // calculator.rs is "deleted" in the diff; main.rs references "calculator"
        let (_tmp, repo, commit_id) = make_visible_repo(&[
            (
                "src/utils/calculator.rs",
                "pub fn add(a: i32, b: i32) -> i32 { a + b }",
            ),
            (
                "src/main.rs",
                "use utils::calculator;\nfn main() { calculator::add(1, 2); }",
            ),
        ]);

        let diff = Diff {
            base: "main".into(),
            target: "feature".into(),
            base_commit_id: commit_id.clone(),
            target_commit_id: commit_id,
            files: vec![crate::git::FileChange {
                path: "src/utils/calculator.rs".to_string(),
                status: FileStatus::Deleted,
                additions: 0,
                deletions: 5,
            }],
            stats: DiffStats::default(),
            commits: vec![],
        };

        let out_dir = repo.path().join("output");
        let result = generate_ghost_refs(&out_dir, &[diff], &repo).unwrap();
        assert!(result.is_some(), "should detect ghost reference");

        let cr = result.unwrap();
        assert_eq!(cr.name, "ghost_refs");
        assert!(cr.output.contains("1")); // 1 deleted file name found

        // Verify output files were created
        assert!(out_dir.join("GHOST_REFERENCES.json").exists());
        assert!(out_dir.join("GHOST_REFERENCES.md").exists());

        // Verify the JSON contains the category field
        let json_str = fs::read_to_string(out_dir.join("GHOST_REFERENCES.json")).unwrap();
        assert!(json_str.contains("\"category\""));
        assert!(json_str.contains("\"runtime_code\""));
    }

    #[test]
    fn ghost_refs_basename_collision_does_not_suppress_real_deletion() {
        // PR #12 review #18: deleting `a/calculator.rs` must still be audited
        // even though an UNRELATED `b/calculator.rs` survives with the same
        // basename. The old basename-only suppression dropped the deletion and
        // hid the dangling reference in main.rs.
        let (_tmp, repo, commit_id) = make_visible_repo(&[
            (
                "src/a/calculator.rs",
                "pub fn add(a: i32, b: i32) -> i32 { a + b }",
            ),
            (
                "src/b/calculator.rs",
                "pub fn mul(a: i32, b: i32) -> i32 { a * b }",
            ),
            (
                "src/main.rs",
                "use crate::a::calculator;\nfn main() { calculator::add(1, 2); }",
            ),
        ]);

        let diff = Diff {
            base: "main".into(),
            target: "feature".into(),
            base_commit_id: commit_id.clone(),
            target_commit_id: commit_id,
            files: vec![crate::git::FileChange {
                path: "src/a/calculator.rs".to_string(),
                status: FileStatus::Deleted,
                additions: 0,
                deletions: 5,
            }],
            stats: DiffStats::default(),
            commits: vec![],
        };

        let out_dir = repo.path().join("output");
        let result = generate_ghost_refs(&out_dir, &[diff], &repo).unwrap();
        assert!(
            result.is_some(),
            "deletion of a/calculator.rs must not be suppressed by the surviving b/calculator.rs basename collision"
        );
    }

    #[test]
    fn ghost_refs_scans_repo_under_dotted_ancestor() {
        // Regression for the dot-dir fail-open. A repo whose ABSOLUTE path carries
        // a dotted component (~/.config, ~/.vibecrafted, macOS /T/.tmpXXX) must
        // still be scanned. The old `contains("/.")` on the absolute path treated
        // every file as hidden, skipped the whole tree, and returned Ok(None) as
        // if the audit were clean. blind != healthy. This is the exact condition
        // make_visible_repo was invented to sidestep instead of fixing.
        let outer = tempfile::Builder::new().prefix("ghtest").tempdir().unwrap();
        let repo_dir = outer.path().join(".vibecrafted").join("repo");
        fs::create_dir_all(&repo_dir).unwrap();

        let git_repo = git2::Repository::init(&repo_dir).unwrap();
        let sig = git2::Signature::now("Test", "test@test.com").unwrap();
        let specs = &[
            (
                "src/utils/calculator.rs",
                "pub fn add(a: i32, b: i32) -> i32 { a + b }",
            ),
            (
                "src/main.rs",
                "use utils::calculator;\nfn main() { calculator::add(1, 2); }",
            ),
        ];
        for &(path, content) in specs {
            if let Some(parent) = Path::new(path).parent()
                && !parent.as_os_str().is_empty()
            {
                fs::create_dir_all(repo_dir.join(parent)).unwrap();
            }
            fs::write(repo_dir.join(path), content).unwrap();
        }
        let mut index = git_repo.index().unwrap();
        for &(path, _) in specs {
            index.add_path(Path::new(path)).unwrap();
        }
        index.write().unwrap();
        let tree_id = index.write_tree().unwrap();
        let tree = git_repo.find_tree(tree_id).unwrap();
        let oid = git_repo
            .commit(Some("HEAD"), &sig, &sig, "init", &tree, &[])
            .unwrap();
        let repo = Repository::open(&repo_dir).unwrap();
        let commit_id = oid.to_string();

        // calculator.rs is deleted; the untouched main.rs still references it.
        let diff = Diff {
            base: "main".into(),
            target: "feature".into(),
            base_commit_id: commit_id.clone(),
            target_commit_id: commit_id,
            files: vec![crate::git::FileChange {
                path: "src/utils/calculator.rs".to_string(),
                status: FileStatus::Deleted,
                additions: 0,
                deletions: 5,
            }],
            stats: DiffStats::default(),
            commits: vec![],
        };

        let out_dir = outer.path().join("output");
        let result = generate_ghost_refs(&out_dir, &[diff], &repo).unwrap();
        assert!(
            result.is_some(),
            "ghost refs under a dotted ancestor must be detected, not skipped as a clean Ok(None)"
        );
    }

    #[test]
    fn test_ghost_refs_returns_none_when_no_findings() {
        // Repo with only modified files, no deletions
        let files = &[("src/lib.rs", "fn old() {}", "fn new_version() {}")];
        let (tmp, repo, base_id, target_id) = make_test_repo(files);

        let diff = make_diff_with_ids(
            base_id,
            target_id,
            vec![mock_file_change("src/lib.rs", FileStatus::Modified, 1, 1)],
        );

        let out_dir = tmp.path().join("output");
        let result = generate_ghost_refs(&out_dir, &[diff], &repo).unwrap();
        assert!(result.is_none(), "no deleted files means no ghost refs");
    }

    #[test]
    fn test_ghost_refs_ignores_short_terms() {
        // Deleted file with a very short stem (<=3 chars) should be skipped
        let files = &[
            ("src/ab.rs", "short", "short"),
            (
                "src/main.rs",
                "use ab;\nfn main() { ab::thing(); }",
                "use ab;\nfn main() { ab::thing(); }",
            ),
        ];
        let (tmp, repo, base_id, target_id) = make_test_repo(files);

        let diff = make_diff_with_ids(
            base_id,
            target_id,
            vec![mock_file_change("src/ab.rs", FileStatus::Deleted, 0, 1)],
        );

        let out_dir = tmp.path().join("output");
        let result = generate_ghost_refs(&out_dir, &[diff], &repo).unwrap();
        assert!(
            result.is_none(),
            "short term 'ab' (len=2, <=3) should be skipped"
        );
    }

    #[test]
    fn test_ghost_refs_skips_logs_and_caps_findings_per_deleted_file() {
        let mut main_content = String::new();
        for idx in 0..80 {
            let _ = writeln!(&mut main_content, "calculator::call_{}();", idx);
        }

        let (_tmp, repo, commit_id) = make_visible_repo(&[
            (
                "src/utils/calculator.rs",
                "pub fn add(a: i32, b: i32) -> i32 { a + b }",
            ),
            ("src/main.rs", &main_content),
            (
                "logs/archive/runtime.log",
                "calculator calculator calculator calculator",
            ),
        ]);

        let diff = Diff {
            base: "main".into(),
            target: "feature".into(),
            base_commit_id: commit_id.clone(),
            target_commit_id: commit_id,
            files: vec![crate::git::FileChange {
                path: "src/utils/calculator.rs".to_string(),
                status: FileStatus::Deleted,
                additions: 0,
                deletions: 5,
            }],
            stats: DiffStats::default(),
            commits: vec![],
        };

        let out_dir = repo.path().join("output");
        let result = generate_ghost_refs(&out_dir, &[diff], &repo).unwrap();
        let audit: serde_json::Value = serde_json::from_str(
            &fs::read_to_string(out_dir.join("GHOST_REFERENCES.json")).unwrap(),
        )
        .unwrap();
        let refs = audit["findings"]["src/utils/calculator.rs"]
            .as_array()
            .expect("expected findings for deleted calculator file");

        assert!(result.is_some(), "should still detect real code references");
        assert_eq!(refs.len(), MAX_GHOST_REFS_PER_DELETED_FILE);
        assert!(refs.iter().all(|r| {
            !r["file"]
                .as_str()
                .expect("ghost ref entry should include file")
                .starts_with("logs/")
        }));
    }

    #[test]
    fn test_ghost_refs_skips_relocated_file() {
        // `src/sanitize.rs` was MOVED to `crates/aicx-parser/src/sanitize.rs`.
        // A `pub mod sanitize;` reference still resolves to the relocated file,
        // so it must NOT be reported as a ghost (regression: BUG-2).
        let (_tmp, repo, commit_id) = make_visible_repo(&[
            ("crates/aicx-parser/src/sanitize.rs", "pub fn clean() {}"),
            ("crates/aicx-parser/src/lib.rs", "pub mod sanitize;\n"),
        ]);

        let diff = Diff {
            base: "main".into(),
            target: "feature".into(),
            base_commit_id: commit_id.clone(),
            target_commit_id: commit_id,
            files: vec![crate::git::FileChange {
                path: "src/sanitize.rs".to_string(),
                status: FileStatus::Deleted,
                additions: 0,
                deletions: 3,
            }],
            stats: DiffStats::default(),
            commits: vec![],
        };

        let out_dir = repo.path().join("output");
        let result = generate_ghost_refs(&out_dir, &[diff], &repo).unwrap();
        assert!(
            result.is_none(),
            "relocated file (same basename surviving elsewhere) must not be a ghost"
        );
    }

    #[test]
    fn test_ghost_refs_filters_build_artifacts() {
        // calculator.rs is "deleted"; references exist in both src/ (runtime)
        // and target/ (build artifact). Only src/ should appear in findings.
        let (_tmp, repo, commit_id) = make_visible_repo(&[
            (
                "src/utils/calculator.rs",
                "pub fn add(a: i32, b: i32) -> i32 { a + b }",
            ),
            (
                "src/main.rs",
                "use utils::calculator;\nfn main() { calculator::add(1, 2); }",
            ),
            (
                "target/debug/build/calculator_info.txt",
                "built from calculator module",
            ),
            (
                "dist/calculator.bundle.js",
                "var calculator = require('calculator');",
            ),
        ]);

        let diff = Diff {
            base: "main".into(),
            target: "feature".into(),
            base_commit_id: commit_id.clone(),
            target_commit_id: commit_id,
            files: vec![crate::git::FileChange {
                path: "src/utils/calculator.rs".to_string(),
                status: FileStatus::Deleted,
                additions: 0,
                deletions: 5,
            }],
            stats: DiffStats::default(),
            commits: vec![],
        };

        let out_dir = repo.path().join("output");
        let result = generate_ghost_refs(&out_dir, &[diff], &repo).unwrap();
        assert!(result.is_some(), "should detect ghost reference in src/");

        let cr = result.unwrap();
        // Only src/main.rs should be in findings (RuntimeCode),
        // target/ and dist/ refs should be noise
        assert!(cr.output.contains("noise filtered"));

        // Parse the JSON to verify structure
        let json_str = fs::read_to_string(out_dir.join("GHOST_REFERENCES.json")).unwrap();
        let audit: GhostRefsAudit = serde_json::from_str(&json_str).unwrap();

        // Runtime findings should only contain src/main.rs refs
        for refs in audit.findings.values() {
            for r in refs {
                assert_eq!(r.category, GhostRefCategory::RuntimeCode);
                assert!(
                    !r.file.starts_with("target/"),
                    "target/ files should be filtered as noise"
                );
                assert!(
                    !r.file.starts_with("dist/"),
                    "dist/ files should be filtered as noise"
                );
            }
        }

        // Noise should have been counted
        assert!(audit.noise_filtered > 0, "should have filtered noise");
        assert!(
            audit.noise_categories.contains_key("build_artifact"),
            "should have build_artifact noise category"
        );
    }

    #[test]
    fn test_ghost_ref_category_classify() {
        // RuntimeCode
        assert_eq!(
            GhostRefCategory::classify("src/main.rs"),
            GhostRefCategory::RuntimeCode
        );
        assert_eq!(
            GhostRefCategory::classify("lib/utils.py"),
            GhostRefCategory::RuntimeCode
        );
        assert_eq!(
            GhostRefCategory::classify("app/components/Button.tsx"),
            GhostRefCategory::RuntimeCode
        );

        // BuildArtifact
        assert_eq!(
            GhostRefCategory::classify("target/debug/build/info.txt"),
            GhostRefCategory::BuildArtifact
        );
        assert_eq!(
            GhostRefCategory::classify("dist/bundle.js"),
            GhostRefCategory::BuildArtifact
        );
        assert_eq!(
            GhostRefCategory::classify("node_modules/lodash/index.js"),
            GhostRefCategory::BuildArtifact
        );
        assert_eq!(
            GhostRefCategory::classify("__pycache__/mod.pyc"),
            GhostRefCategory::BuildArtifact
        );
        assert_eq!(
            GhostRefCategory::classify("coverage/lcov.info"),
            GhostRefCategory::BuildArtifact
        );

        // LogOrTemp
        assert_eq!(
            GhostRefCategory::classify("output.log"),
            GhostRefCategory::LogOrTemp
        );
        assert_eq!(
            GhostRefCategory::classify("data.tmp"),
            GhostRefCategory::LogOrTemp
        );
        assert_eq!(
            GhostRefCategory::classify("logs/app.txt"),
            GhostRefCategory::LogOrTemp
        );
        assert_eq!(
            GhostRefCategory::classify("tmp/scratch.rs"),
            GhostRefCategory::LogOrTemp
        );
        assert_eq!(
            GhostRefCategory::classify("backup.bak"),
            GhostRefCategory::LogOrTemp
        );

        // GeneratedAsset
        assert_eq!(
            GhostRefCategory::classify("index.html"),
            GhostRefCategory::GeneratedAsset
        );
        assert_eq!(
            GhostRefCategory::classify("styles.css.map"),
            GhostRefCategory::GeneratedAsset
        );
        assert_eq!(
            GhostRefCategory::classify("types.d.ts"),
            GhostRefCategory::GeneratedAsset
        );
        assert_eq!(
            GhostRefCategory::classify("app.min.js"),
            GhostRefCategory::GeneratedAsset
        );
        assert_eq!(
            GhostRefCategory::classify("styles.min.css"),
            GhostRefCategory::GeneratedAsset
        );
    }

    #[test]
    fn test_ghost_refs_log_files_filtered_as_noise() {
        let (_tmp, repo, commit_id) = make_visible_repo(&[
            (
                "src/utils/calculator.rs",
                "pub fn add(a: i32, b: i32) -> i32 { a + b }",
            ),
            ("logs/build.txt", "compiled calculator module successfully"),
        ]);

        let diff = Diff {
            base: "main".into(),
            target: "feature".into(),
            base_commit_id: commit_id.clone(),
            target_commit_id: commit_id,
            files: vec![crate::git::FileChange {
                path: "src/utils/calculator.rs".to_string(),
                status: FileStatus::Deleted,
                additions: 0,
                deletions: 5,
            }],
            stats: DiffStats::default(),
            commits: vec![],
        };

        let out_dir = repo.path().join("output");
        let result = generate_ghost_refs(&out_dir, &[diff], &repo).unwrap();
        // Only noise found, no runtime refs, so result should be None
        assert!(
            result.is_none(),
            "log-only references should be filtered — no actionable findings"
        );
    }
}
