//! Coverage delta — cross-reference changed source with test files.

use super::common::{contains_token_match, is_non_code_file};
use crate::git::{Diff, FileChange, Repository};
use crate::paths::normalize_path_display;
use crate::regression::tests::is_test_file;
use anyhow::Result;
use std::collections::{HashMap, HashSet};
use std::fmt::Write as FmtWrite;
use std::fs;
use std::path::Path;

// ── Coverage match tiers ─────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CoverageMatchTier {
    High,   // exact stem match, path-mirrored
    Medium, // sibling tests module, import recovery
    Low,    // keyword overlap only
}

impl std::fmt::Display for CoverageMatchTier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CoverageMatchTier::High => write!(f, "high"),
            CoverageMatchTier::Medium => write!(f, "medium"),
            CoverageMatchTier::Low => write!(f, "low"),
        }
    }
}

// ── Coverage data structures ─────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct CoverageDelta {
    pub total_source: usize,
    pub covered_count: usize,
    pub pct: u32,
    pub uncovered: Vec<CoverageFile>,
    pub covered: Vec<CoveragePair>,
    pub non_code_count: usize,
    pub ghost_tests: Vec<CoveragePair>,
}

impl CoverageDelta {
    /// Build CoverageDelta from the canonical CoverageSignal.
    pub fn from_signal(signal: &CoverageSignal) -> Self {
        CoverageDelta {
            total_source: signal.total_source_files,
            covered_count: signal.covered_count,
            pct: signal.coverage_pct,
            uncovered: signal
                .uncovered_files
                .iter()
                .map(|p| CoverageFile {
                    status: 'M',
                    path: p.clone(),
                })
                .collect(),
            covered: signal
                .covered_files
                .iter()
                .map(|(src, test, tier)| CoveragePair {
                    src_status: 'M',
                    src_path: src.clone(),
                    test_status: 'M',
                    test_path: test.clone(),
                    tier: *tier,
                })
                .collect(),
            ghost_tests: signal
                .ghost_tests
                .iter()
                .map(|(src, ghost)| CoveragePair {
                    src_status: 'D',
                    src_path: src.clone(),
                    test_status: 'M',
                    test_path: ghost.clone(),
                    tier: CoverageMatchTier::High, // ghost detection uses stem matching
                })
                .collect(),
            non_code_count: signal.non_code_count,
        }
    }
}

#[derive(Debug, Clone)]
pub struct CoverageFile {
    pub status: char,
    pub path: String,
}

#[derive(Debug, Clone)]
pub struct CoveragePair {
    pub src_status: char,
    pub src_path: String,
    pub test_status: char,
    pub test_path: String,
    // Never read after pairing: the match tier is computed in pair_sources_with_tests
    // but no runtime consumer weighs it yet. Kept because tier-weighted coverage
    // scoring in the PolicyEngine verdict (policy/engine.rs) is the planned
    // consumer; remove if that lands without tier weighting. Re-confirmed
    // still-dead after the wave-7 verdict wiring landed. Tracked in the vc-prune
    // forgotten-gems report 2026-07-02.
    #[allow(dead_code)]
    pub tier: CoverageMatchTier,
}

// ── Coverage signal ──────────────────────────────────────────────────

/// Canonical coverage signal — one source of truth for all consumers.
#[derive(Debug, Clone, serde::Serialize)]
pub struct CoverageSignal {
    pub covered_files: Vec<(String, String, CoverageMatchTier)>, // (source_path, test_path, tier)
    pub uncovered_files: Vec<String>,
    pub total_source_files: usize,
    pub covered_count: usize,
    pub coverage_pct: u32,
    pub non_code_count: usize,
    pub has_rust_inline_tests: bool,
    pub rust_uncovered_count: usize,
    /// Whether source file contents were available to check for inline
    /// `#[cfg(test)]` modules (Strategy 6). When true, files left in
    /// `uncovered_files` were verified to lack inline tests.
    pub inline_tests_checked: bool,
    /// Number of `.rs` files counted as covered via an inline `#[cfg(test)]`
    /// module that the filename heuristic could not see.
    pub inline_tested_count: usize,
    pub confidence: &'static str,
    pub limitations: Vec<&'static str>,
    pub ghost_tests: Vec<(String, String)>,
}

/// Compute coverage signal from diffs — single canonical function.
///
/// All consumers (coverage-delta.txt, PR_REVIEW, MERGE_GATE, dashboard, report.json)
/// must use this same function to avoid inconsistent numbers.
pub fn compute_coverage_signal(
    diffs: &[Diff],
    repo_root: Option<&Path>,
    repo: Option<&Repository>,
) -> CoverageSignal {
    let mut seen_paths = HashSet::new();
    let all_files: Vec<&FileChange> = diffs
        .iter()
        .flat_map(|d| &d.files)
        .filter(|f| seen_paths.insert(f.path.as_str()))
        .collect();

    let mut source_files = Vec::new();
    let mut test_files = Vec::new();
    let mut deleted_source_files = Vec::new();
    let mut deleted_test_files = Vec::new();
    let mut non_code_count = 0usize;

    for fc in &all_files {
        if is_test_file(&fc.path) {
            if fc.status == crate::git::FileStatus::Deleted {
                deleted_test_files.push(*fc);
            } else {
                test_files.push(*fc);
            }
        } else if is_non_code_file(&fc.path) {
            non_code_count += 1;
        } else if fc.status == crate::git::FileStatus::Deleted {
            deleted_source_files.push(*fc);
        } else {
            source_files.push(*fc);
        }
    }

    let mut covered: Vec<(&FileChange, &FileChange, CoverageMatchTier)> = Vec::new();
    let mut uncovered: Vec<&FileChange> = Vec::new();

    // Strategies 1-4: filename heuristic, path-mirrored, sibling, keyword overlap
    for src in &source_files {
        if let Some((test, tier)) = find_matching_test(src, &test_files) {
            covered.push((*src, test, tier));
        } else {
            uncovered.push(*src);
        }
    }

    // Strategy 5 (import recovery): import-based matching for still-uncovered files
    let mut used_import_recovery = false;
    if repo_root.is_some() || repo.is_some() {
        let target_commit = diffs.first().map(|d| d.target_commit_id.as_str());
        let mut test_contents: HashMap<String, String> = HashMap::new();
        for test in &test_files {
            let content = if let (Some(r), Some(commit)) = (repo, target_commit) {
                r.file_at_commit(commit, &test.path).ok()
            } else if let Some(root) = repo_root {
                std::fs::read_to_string(root.join(&test.path)).ok()
            } else {
                None
            };
            if let Some(c) = content {
                test_contents.insert(test.path.clone(), c);
            }
        }

        let mut import_recovered: Vec<(&FileChange, &FileChange, CoverageMatchTier)> = Vec::new();
        for src in &uncovered {
            if let Some((test, tier)) = find_test_by_import(src, &test_files, &test_contents) {
                import_recovered.push((*src, test, tier));
            }
        }
        if !import_recovered.is_empty() {
            used_import_recovery = true;
            for (src, test, tier) in &import_recovered {
                covered.push((*src, *test, *tier));
            }
            let recovered_paths: HashSet<&str> = import_recovered
                .iter()
                .map(|(s, _, _)| s.path.as_str())
                .collect();
            uncovered.retain(|s| !recovered_paths.contains(s.path.as_str()));
        }
    }

    // Strategy 6 (inline-test recovery): a changed `.rs` source file that
    // carries an inline `#[cfg(test)]` module is self-tested. The filename
    // heuristic cannot see it, which otherwise deflates the ratio and emits a
    // misleading "may contain inline tests" caveat (TOOLING-11). Only runs when
    // source content is available.
    let mut inline_tested_paths: Vec<String> = Vec::new();
    let mut inline_tests_checked = false;
    if repo_root.is_some() || repo.is_some() {
        inline_tests_checked = true;
        let target_commit = diffs.first().map(|d| d.target_commit_id.as_str());
        let mut inline_set: HashSet<&str> = HashSet::new();
        for src in &uncovered {
            if !src.path.ends_with(".rs") {
                continue;
            }
            let content = if let (Some(r), Some(commit)) = (repo, target_commit) {
                r.file_at_commit(commit, &src.path).ok()
            } else if let Some(root) = repo_root {
                std::fs::read_to_string(root.join(&src.path)).ok()
            } else {
                None
            };
            // Require an actual test, not merely a `#[cfg(test)]` attribute: a
            // file with only a `#[cfg(test)] use ...;` import or a cfg(test)
            // helper (and no `#[test]`) is NOT self-covered. Counting it would
            // inflate coverage_pct with a false PASS.
            if content.as_deref().is_some_and(has_inline_rust_tests) {
                inline_set.insert(src.path.as_str());
                inline_tested_paths.push(src.path.clone());
            }
        }
        if !inline_set.is_empty() {
            uncovered.retain(|s| !inline_set.contains(s.path.as_str()));
        }
    }
    let inline_tested_count = inline_tested_paths.len();

    let mut all_tests_refs: Vec<&FileChange> = test_files.to_vec();
    all_tests_refs.extend(deleted_test_files.iter().copied());

    let mut ghost_tests = Vec::new();

    for src in &deleted_source_files {
        // Jeśli test połączony występuje w diffie (i jest np. usunięty / modyfikowany), to znaczy że nie jest duchem
        if find_matching_test(src, &all_tests_refs).is_none() {
            // Jeśli nie ma go w diffie, sprawdźmy fizycznie na dysku czy istnieje osierocony plik testowy
            if let Some(root) = repo_root
                && let Some(ghost) = find_ghost_test_on_disk(&src.path, root)
            {
                ghost_tests.push((src.path.clone(), ghost));
            }
        }
    }

    let total = source_files.len();
    let covered_count = covered.len() + inline_tested_count;
    let pct = if total > 0 {
        (covered_count as f64 / total as f64 * 100.0) as u32
    } else {
        100
    };

    let has_rust = source_files.iter().any(|f| f.path.ends_with(".rs"));
    let rust_uncovered = uncovered.iter().filter(|f| f.path.ends_with(".rs")).count();

    let has_low = covered.iter().any(|(_, _, t)| *t == CoverageMatchTier::Low);
    let has_medium = covered
        .iter()
        .any(|(_, _, t)| *t == CoverageMatchTier::Medium);

    let mut limitations = Vec::new();
    limitations.push("File-name heuristic, not actual code coverage");
    // Only warn about possibly-missed inline tests when we could NOT read source
    // content. When inline tests were checked, files left uncovered were verified
    // to lack a `#[cfg(test)]` module.
    if rust_uncovered > 0 && !inline_tests_checked {
        limitations.push("Inline #[cfg(test)] modules not detected by filename matching");
    }
    if used_import_recovery {
        limitations.push(
            "Import-based recovery uses word-boundary matching (may miss complex re-exports)",
        );
    }
    if has_low {
        limitations.push("Some matches use keyword overlap only (low confidence, verify manually)");
    }

    let confidence = if rust_uncovered > 0 || used_import_recovery || has_medium || has_low {
        "medium"
    } else {
        "high"
    };

    // Normalize all emitted paths to repo-relative form.
    let norm = |p: &str| -> String {
        match repo_root {
            Some(root) => normalize_path_display(p, root),
            None => p.to_string(),
        }
    };

    let mut covered_files: Vec<(String, String, CoverageMatchTier)> = covered
        .iter()
        .map(|(s, t, tier)| (norm(&s.path), norm(&t.path), *tier))
        .collect();
    for path in &inline_tested_paths {
        covered_files.push((
            path.clone(),
            "(inline #[cfg(test)])".to_string(),
            CoverageMatchTier::High,
        ));
    }
    covered_files.sort();
    let mut uncovered_files: Vec<String> = uncovered.iter().map(|s| norm(&s.path)).collect();
    uncovered_files.sort();

    let mut ghost_tests_sorted: Vec<(String, String)> = ghost_tests
        .iter()
        .map(|(src, ghost)| (norm(src), norm(ghost)))
        .collect();
    ghost_tests_sorted.sort();

    CoverageSignal {
        covered_files,
        uncovered_files,
        ghost_tests: ghost_tests_sorted,
        total_source_files: total,
        covered_count,
        coverage_pct: pct,
        non_code_count,
        has_rust_inline_tests: inline_tested_count > 0 || has_rust,
        rust_uncovered_count: rust_uncovered,
        inline_tests_checked,
        inline_tested_count,
        confidence,
        limitations,
    }
}

fn has_inline_rust_tests(content: &str) -> bool {
    // Require BOTH a `mod tests` declaration and an actual `#[test]` attribute.
    // A bare `#[cfg(test)]` import/helper or an empty `mod tests {}` is NOT local
    // test coverage, and counting it would inflate the headline percentage with a
    // false PASS.
    let mut has_test_mod = false;
    let mut has_test_attr = false;
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("mod tests") || trimmed.starts_with("pub mod tests") {
            has_test_mod = true;
        }
        if trimmed.starts_with("#[test]") || trimmed.starts_with("#[tokio::test]") {
            has_test_attr = true;
        }
        if has_test_mod && has_test_attr {
            return true;
        }
    }
    false
}

/// Render coverage-delta.txt from a pre-computed CoverageSignal.
///
/// Callers must pass the same signal used for structured outputs so that
/// the text artifact and MERGE_GATE/dashboard/report.json never diverge.
pub fn generate_coverage_delta(dir: &Path, signal: &CoverageSignal) -> Result<()> {
    let pct = signal.coverage_pct;

    let mut output = String::new();
    output.push_str("# Coverage Delta (heuristic)\n");
    for lim in &signal.limitations {
        let _ = writeln!(output, "# {}", lim);
    }
    let _ = writeln!(output, "# Confidence: {}\n", signal.confidence);

    if !signal.uncovered_files.is_empty() {
        output.push_str(
            "MODIFIED_NO_TEST_CHANGE (actionable - source code altered without tests):\n",
        );
        for path in &signal.uncovered_files {
            let _ = writeln!(output, "  {}", path);
        }
        if signal.rust_uncovered_count > 0 && !signal.inline_tests_checked {
            let _ = writeln!(
                output,
                "Note: {} .rs file(s) may contain inline #[cfg(test)] modules not detected by this heuristic.",
                signal.rust_uncovered_count
            );
        }
        output.push('\n');
    }

    if !signal.ghost_tests.is_empty() {
        output.push_str(
            "ORPHANED_TEST_DETECTED (diff hygiene - source dropped but test file left behind):\n",
        );
        for (src, ghost) in &signal.ghost_tests {
            let _ = writeln!(output, "  source: {} -> likely ghost test: {}", src, ghost);
        }
        output.push('\n');
    }

    let strong_matches: Vec<_> = signal
        .covered_files
        .iter()
        .filter(|(_, _, tier)| *tier != CoverageMatchTier::Low)
        .collect();
    let weak_matches: Vec<_> = signal
        .covered_files
        .iter()
        .filter(|(_, _, tier)| *tier == CoverageMatchTier::Low)
        .collect();

    if !strong_matches.is_empty() {
        output.push_str("HAS_TEST_CHANGE (matching test file found in diff):\n");
        for (src, test, _tier) in &strong_matches {
            let _ = writeln!(output, "  {}  <->  {}", src, test);
        }
        output.push('\n');
    }

    if !weak_matches.is_empty() {
        output.push_str(
            "WEAK_TEST_MATCH (low confidence - keyword overlap only, verify manually):\n",
        );
        for (src, test, _tier) in &weak_matches {
            let _ = writeln!(output, "  {}  <->  {}", src, test);
        }
        output.push('\n');
    }

    let _ = writeln!(
        output,
        "Summary: {}/{} changed code files have matching test changes ({}%)",
        signal.covered_count, signal.total_source_files, pct
    );
    if signal.inline_tested_count > 0 {
        let _ = writeln!(
            output,
            "Recovered {} .rs file(s) as covered via inline #[cfg(test)] modules.",
            signal.inline_tested_count
        );
    }
    if signal.non_code_count > 0 {
        let _ = writeln!(
            output,
            "Non-code changes (assets/i18n/config/docs): {} files excluded from coverage",
            signal.non_code_count
        );
    }

    fs::write(dir.join("coverage-delta.txt"), output)?;

    Ok(())
}

/// Find a test file that corresponds to a given source file.
///
/// Matching strategies (in order):
/// 1. Exact stem match: `foo.rs` <-> `foo_test.rs` / `test_foo.rs` / `foo.test.ts`
/// 2. Path-mirrored: `src/foo/bar.rs` <-> `tests/foo/bar.rs`
/// 3. Sibling tests module: `src/foo/bar.rs` <-> `src/foo/tests.rs` / `src/foo/tests/*.rs`
/// 4. Keyword overlap: `core/audio/chunker.rs` <-> `tests/e2e_vad_flow.rs` (shared path segments)
fn find_matching_test<'a>(
    source: &FileChange,
    test_files: &[&'a FileChange],
) -> Option<(&'a FileChange, CoverageMatchTier)> {
    let src_stem = Path::new(&source.path)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("");

    let src_parent = Path::new(&source.path)
        .parent()
        .and_then(|p| p.to_str())
        .unwrap_or("");

    // Strategy 1: Exact stem match (strip test prefix/suffix)
    for test in test_files {
        let test_stem = Path::new(&test.path)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("");

        let test_base = test_stem
            .strip_suffix("_test")
            .or_else(|| test_stem.strip_suffix(".test"))
            .or_else(|| test_stem.strip_suffix(".spec"))
            .or_else(|| test_stem.strip_prefix("test_"))
            .unwrap_or(test_stem);

        if test_base == src_stem {
            let tier = if same_coverage_module(&source.path, &test.path) {
                CoverageMatchTier::High
            } else {
                CoverageMatchTier::Low
            };
            return Some((test, tier));
        }
    }

    // Strategy 2: Path-mirrored (tests/foo/bar.rs <-> src/foo/bar.rs)
    for test in test_files {
        if test.path.contains("tests/") || test.path.contains("__tests__/") {
            let test_filename = Path::new(&test.path)
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("");
            if test_filename == src_stem {
                let tier = if same_coverage_module(&source.path, &test.path) {
                    CoverageMatchTier::High
                } else {
                    CoverageMatchTier::Low
                };
                return Some((test, tier));
            }
        }
    }

    // Strategy 3: Sibling tests module (src/foo/bar.rs <-> src/foo/tests.rs or src/foo/tests/*.rs) → Medium
    for test in test_files {
        let test_parent = Path::new(&test.path)
            .parent()
            .and_then(|p| p.to_str())
            .unwrap_or("");
        let test_stem = Path::new(&test.path)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("");

        // src/foo/bar.rs <-> src/foo/tests.rs
        if test_parent == src_parent && test_stem == "tests" {
            return Some((test, CoverageMatchTier::Medium));
        }

        // src/foo/bar.rs <-> src/foo/tests/anything.rs
        if let Some(stripped) = test_parent.strip_suffix("/tests")
            && stripped == src_parent
        {
            return Some((test, CoverageMatchTier::Medium));
        }
        // Also handle tests/ at start: src/foo/bar.rs <-> tests/foo/anything.rs
        if let Some(test_sub) = test_parent.strip_prefix("tests/")
            && !src_parent.is_empty()
            && (src_parent.ends_with(test_sub) || test_sub.ends_with(src_parent))
        {
            return Some((test, CoverageMatchTier::Medium));
        }
    }

    // Strategy 4: Keyword overlap — source path segments appear in test filename → Low
    // e.g. core/audio/chunker.rs <-> tests/e2e_audio_chunker.rs
    let src_segments: Vec<&str> = source
        .path
        .split('/')
        .filter(|s| !matches!(*s, "src" | "core" | "lib" | "mod.rs" | "app"))
        .collect();

    if src_segments.len() >= 2 {
        let mut best_match: Option<(&'a FileChange, usize)> = None;

        for test in test_files {
            let test_lower = test.path.to_lowercase();
            let overlap = src_segments
                .iter()
                .filter(|seg| {
                    let seg_lower = seg.to_lowercase();
                    let seg_clean = seg_lower
                        .strip_suffix(".rs")
                        .or_else(|| seg_lower.strip_suffix(".py"))
                        .or_else(|| seg_lower.strip_suffix(".ts"))
                        .or_else(|| seg_lower.strip_suffix(".tsx"))
                        .or_else(|| seg_lower.strip_suffix(".js"))
                        .or_else(|| seg_lower.strip_suffix(".jsx"))
                        .unwrap_or(&seg_lower);
                    seg_clean.len() >= 3 && test_lower.contains(seg_clean)
                })
                .count();

            if overlap >= 2 && best_match.is_none_or(|(_, best)| overlap > best) {
                best_match = Some((test, overlap));
            }
        }

        if let Some((test, _)) = best_match {
            return Some((test, CoverageMatchTier::Low));
        }
    }

    None
}

fn same_coverage_module(source_path: &str, test_path: &str) -> bool {
    coverage_module_key(source_path) == coverage_module_key(test_path)
}

fn coverage_module_key(path: &str) -> Vec<String> {
    let mut components = Path::new(path)
        .components()
        .filter_map(|component| component.as_os_str().to_str())
        .filter(|component| !component.is_empty())
        .map(|component| component.to_string())
        .collect::<Vec<_>>();

    if components
        .first()
        .is_some_and(|component| matches!(component.as_str(), "src" | "tests" | "__tests__"))
    {
        components.remove(0);
    }

    components.pop();

    while components
        .last()
        .is_some_and(|component| matches!(component.as_str(), "tests" | "__tests__"))
    {
        components.pop();
    }

    components
}

/// Strategy 5 (import recovery): find a test file that imports the source module.
///
/// Only called for source files that had no filename-heuristic match.
/// Reads test file content from disk and greps for import patterns.
fn find_test_by_import<'a>(
    source: &FileChange,
    test_files: &[&'a FileChange],
    test_contents: &HashMap<String, String>,
) -> Option<(&'a FileChange, CoverageMatchTier)> {
    let src_stem = Path::new(&source.path)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("");

    if src_stem.is_empty() {
        return None;
    }

    // Build module path variants to search for in imports
    // e.g., "screenscribe/cli.py" -> "screenscribe.cli", "screenscribe/cli", "cli"
    let p = source.path.as_str();
    let module_path = p
        .strip_suffix(".py")
        .or_else(|| p.strip_suffix(".ts"))
        .or_else(|| p.strip_suffix(".tsx"))
        .or_else(|| p.strip_suffix(".js"))
        .or_else(|| p.strip_suffix(".jsx"))
        .or_else(|| p.strip_suffix(".rs"))
        .unwrap_or(p);
    let module_dot = module_path.replace('/', ".");
    let module_slash = module_path;

    let needles: Vec<&str> = vec![&module_dot, module_slash, src_stem]
        .into_iter()
        .filter(|s| s.len() >= 3) // avoid matching very short stems
        .collect();

    if needles.is_empty() {
        return None;
    }

    for test in test_files {
        let content = match test_contents.get(&test.path) {
            Some(c) => c,
            None => continue,
        };

        // Only scan import-related lines for efficiency
        for line in content.lines() {
            let trimmed = line.trim();
            let is_import = trimmed.starts_with("import ")
                || trimmed.starts_with("from ")
                || trimmed.starts_with("use ")
                || trimmed.starts_with("require(")
                || trimmed.contains("import(")
                || trimmed.contains("require(");

            if !is_import {
                continue;
            }

            if needles
                .iter()
                .any(|needle| contains_token_match(trimmed, needle))
            {
                return Some((test, CoverageMatchTier::Medium));
            }
        }
    }

    None
}

/// Szybka fizyczna sondażówka dyskowa poszukująca sierocych testów dla porzuconych plików źródłowych
fn find_ghost_test_on_disk(source_path: &str, repo_root: &Path) -> Option<String> {
    let path = Path::new(source_path);
    let stem = path.file_stem()?.to_str()?;
    let ext = path.extension().and_then(|s| s.to_str()).unwrap_or("");
    let parent = path.parent().and_then(|p| p.to_str()).unwrap_or("");

    let mut candidates = Vec::new();

    if ext == "rs" {
        if !parent.is_empty() {
            candidates.push(format!("{}/{}_test.rs", parent, stem));
            candidates.push(format!("{}/test_{}.rs", parent, stem));
            candidates.push(format!("{}/tests/{}.rs", parent, stem));
            let tests_dir = parent.replace("src", "tests");
            if tests_dir != parent {
                candidates.push(format!("{}/{}.rs", tests_dir, stem));
            }
        }
        candidates.push(format!("tests/{}.rs", stem));
    } else if matches!(ext, "ts" | "tsx" | "js" | "jsx" | "py") {
        let e = ext;
        if !parent.is_empty() {
            candidates.push(format!("{}/{}.test.{}", parent, stem, e));
            candidates.push(format!("{}/{}.spec.{}", parent, stem, e));
            candidates.push(format!("{}/__tests__/{}.{}", parent, stem, e));
            candidates.push(format!("{}/test_{}.{}", parent, stem, e));
            candidates.push(format!("{}/{}_test.{}", parent, stem, e));
        }
        candidates.push(format!("tests/test_{}.{}", stem, e));
        candidates.push(format!("tests/{}_test.{}", stem, e));
    }

    candidates
        .into_iter()
        .find(|cand| repo_root.join(cand).exists())
}

#[cfg(test)]
mod tests {
    use super::super::test_helpers::{
        make_diff_with_ids, make_test_repo, mock_diff, mock_file_change,
    };
    use super::*;
    use crate::git::FileStatus;
    use tempfile::TempDir;

    #[test]
    fn coverage_delta_identifies_uncovered() {
        let tmp = TempDir::new().unwrap();
        let diff = mock_diff(vec![
            mock_file_change("src/lib.rs", FileStatus::Modified, 10, 5),
            mock_file_change("src/utils.rs", FileStatus::Added, 20, 0),
            mock_file_change("tests/lib_test.rs", FileStatus::Modified, 5, 2),
        ]);

        let signal = compute_coverage_signal(&[diff], None, None);
        generate_coverage_delta(tmp.path(), &signal).unwrap();

        let content = fs::read_to_string(tmp.path().join("coverage-delta.txt")).unwrap();

        assert!(content.contains("src/utils.rs"));
        assert!(content.contains("NO_TEST_CHANGE"));
        assert!(content.contains("lib_test.rs"));
        assert!(content.contains("HAS_TEST_CHANGE"));
        assert!(content.contains("1/2"));
        assert!(content.contains("50%"));
        assert!(content.contains("heuristic"));
        assert!(content.contains("#[cfg(test)]") || content.contains("inline"));
        assert!(content.contains("# Confidence: medium"));
    }

    #[test]
    fn coverage_confidence_medium_when_rust_uncovered() {
        // When Rust source files are uncovered, confidence should be "medium"
        // because inline #[cfg(test)] modules are a known blind spot.
        let diff = mock_diff(vec![
            mock_file_change("src/lib.rs", FileStatus::Modified, 10, 5),
            mock_file_change("src/utils.rs", FileStatus::Added, 20, 0),
        ]);

        let signal = compute_coverage_signal(&[diff], None, None);
        assert_eq!(signal.confidence, "medium");
        assert!(signal.rust_uncovered_count > 0);
    }

    #[test]
    fn coverage_counts_inline_rust_test_modules_as_test_signal() {
        let old_content = "pub fn value() -> u32 { 1 }\n";
        let new_content = "pub fn value() -> u32 { 2 }\n\n\
                           #[cfg(test)]\n\
                           mod tests {\n\
                               #[test]\n\
                               fn value_works() { assert_eq!(super::value(), 2); }\n\
                           }\n";
        let files = &[("src/lib.rs", old_content, new_content)];
        let (_tmp, repo, base_id, target_id) = make_test_repo(files);
        let diff = make_diff_with_ids(
            base_id,
            target_id,
            vec![mock_file_change("src/lib.rs", FileStatus::Modified, 10, 1)],
        );

        let signal = compute_coverage_signal(&[diff], None, Some(&repo));
        assert_eq!(signal.covered_count, 1);
        assert_eq!(signal.coverage_pct, 100);
        assert!(signal.uncovered_files.is_empty());
        assert_eq!(
            signal.covered_files[0],
            (
                "src/lib.rs".to_string(),
                "(inline #[cfg(test)])".to_string(),
                CoverageMatchTier::High,
            )
        );
    }

    #[test]
    fn coverage_inline_cfg_test_without_test_fn_not_counted() {
        // A `#[cfg(test)]` import/helper with no `mod tests` + `#[test]` is not
        // local test coverage and must not be counted (false PASS otherwise).
        let old_content = "pub fn value() -> u32 { 1 }\n";
        let new_content = "pub fn value() -> u32 { 2 }\n\n\
                           #[cfg(test)]\n\
                           use std::fmt;\n";
        let files = &[("src/lib.rs", old_content, new_content)];
        let (_tmp, repo, base_id, target_id) = make_test_repo(files);
        let diff = make_diff_with_ids(
            base_id,
            target_id,
            vec![mock_file_change("src/lib.rs", FileStatus::Modified, 2, 1)],
        );

        let signal = compute_coverage_signal(&[diff], None, Some(&repo));
        assert_eq!(signal.uncovered_files, vec!["src/lib.rs"]);
        assert!(signal.coverage_pct < 100);
    }

    #[test]
    fn coverage_confidence_high_when_all_covered() {
        let diff = mock_diff(vec![
            mock_file_change("src/lib.rs", FileStatus::Modified, 10, 5),
            mock_file_change("tests/lib_test.rs", FileStatus::Modified, 5, 2),
        ]);

        let signal = compute_coverage_signal(&[diff], None, None);
        assert_eq!(signal.confidence, "high");
    }

    #[test]
    fn artifact_consistency_stem_match_across_different_modules_is_low_confidence() {
        let diff = mock_diff(vec![
            mock_file_change("src/a/util.rs", FileStatus::Modified, 10, 5),
            mock_file_change("tests/b/util_test.rs", FileStatus::Modified, 5, 2),
        ]);

        let signal = compute_coverage_signal(&[diff], None, None);

        assert_eq!(signal.covered_count, 1);
        assert_eq!(
            signal.covered_files[0],
            (
                "src/a/util.rs".to_string(),
                "tests/b/util_test.rs".to_string(),
                CoverageMatchTier::Low,
            )
        );
        assert_eq!(signal.confidence, "medium");
    }

    #[test]
    fn coverage_delta_excludes_non_code_files_from_source_count() {
        let diff = mock_diff(vec![
            mock_file_change("src/lib.rs", FileStatus::Modified, 10, 5),
            mock_file_change("tests/lib_test.rs", FileStatus::Modified, 5, 2),
            mock_file_change(".gitignore", FileStatus::Modified, 1, 0),
            mock_file_change("Makefile", FileStatus::Added, 12, 0),
            mock_file_change("tools/setup_hooks.sh", FileStatus::Added, 20, 0),
            mock_file_change("README.md", FileStatus::Modified, 8, 1),
        ]);

        let signal = compute_coverage_signal(&[diff], None, None);
        assert_eq!(signal.total_source_files, 1);
        assert_eq!(signal.covered_count, 1);
        assert_eq!(signal.non_code_count, 4);
        assert_eq!(signal.coverage_pct, 100);
    }

    #[test]
    fn coverage_signal_only_warns_about_inline_rust_tests_when_rust_files_are_uncovered() {
        let uncovered = compute_coverage_signal(
            &[mock_diff(vec![mock_file_change(
                "src/lib.rs",
                FileStatus::Modified,
                10,
                2,
            )])],
            None,
            None,
        );
        assert_eq!(uncovered.confidence, "medium");
        assert!(
            uncovered
                .limitations
                .iter()
                .any(|item| item.contains("Inline #[cfg(test)] modules"))
        );

        let covered = compute_coverage_signal(
            &[mock_diff(vec![
                mock_file_change("src/lib.rs", FileStatus::Modified, 10, 2),
                mock_file_change("tests/lib_test.rs", FileStatus::Modified, 5, 1),
            ])],
            None,
            None,
        );
        assert_eq!(covered.confidence, "high");
        assert!(
            !covered
                .limitations
                .iter()
                .any(|item| item.contains("Inline #[cfg(test)] modules"))
        );
    }

    #[test]
    fn coverage_signal_import_recovery_respects_underscore_identifier_boundaries() {
        let (_tmp, repo, base_id, target_id) = make_test_repo(&[
            (
                "src/api.rs",
                "pub fn api() {}\n",
                "pub fn api() -> bool { true }\n",
            ),
            (
                "tests/client.rs",
                "use api_client::Client;\n\n#[test]\nfn works() {}\n",
                "use api_client::Client;\n\n#[test]\nfn works() {\n    let _ = Client::new();\n}\n",
            ),
        ]);
        let diff = make_diff_with_ids(
            base_id,
            target_id,
            vec![
                mock_file_change("src/api.rs", FileStatus::Modified, 1, 1),
                mock_file_change("tests/client.rs", FileStatus::Modified, 2, 1),
            ],
        );

        let signal = compute_coverage_signal(&[diff], None, Some(&repo));

        assert_eq!(signal.covered_count, 0);
        assert_eq!(signal.uncovered_files, vec!["src/api.rs"]);
    }

    #[test]
    fn coverage_inline_cfg_test_counts_as_covered() {
        // A changed source file carrying an inline #[cfg(test)] module is
        // self-tested; the filename heuristic alone misses it (TOOLING-11).
        let new = "pub fn add(a: i32, b: i32) -> i32 { a + b }\n\
                   #[cfg(test)]\n\
                   mod tests {\n    #[test]\n    fn t() { assert_eq!(super::add(1, 1), 2); }\n}\n";
        let (_tmp, repo, base_id, target_id) = make_test_repo(&[(
            "src/calc.rs",
            "pub fn add(a: i32, b: i32) -> i32 { a + b }\n",
            new,
        )]);
        let diff = make_diff_with_ids(
            base_id,
            target_id,
            vec![mock_file_change("src/calc.rs", FileStatus::Modified, 5, 0)],
        );

        let signal = compute_coverage_signal(&[diff], None, Some(&repo));
        assert!(signal.inline_tests_checked);
        assert_eq!(signal.inline_tested_count, 1);
        assert!(
            signal.uncovered_files.is_empty(),
            "inline-tested file must not be counted as uncovered"
        );
        assert_eq!(signal.coverage_pct, 100);
        assert!(
            signal
                .covered_files
                .iter()
                .any(|(s, t, _)| s == "src/calc.rs" && t.contains("inline")),
            "inline-tested file should appear as covered with an inline label"
        );
    }

    #[test]
    fn coverage_inline_cfg_test_helper_only_not_counted() {
        // A `#[cfg(test)]` attribute on a mere import/helper, with NO `#[test]`,
        // must NOT count the file as self-covered (would be a false PASS).
        let new = "pub fn add(a: i32, b: i32) -> i32 { a + b }\n\
                   #[cfg(test)]\n\
                   use std::fmt;\n";
        let (_tmp, repo, base_id, target_id) = make_test_repo(&[(
            "src/calc.rs",
            "pub fn add(a: i32, b: i32) -> i32 { a + b }\n",
            new,
        )]);
        let diff = make_diff_with_ids(
            base_id,
            target_id,
            vec![mock_file_change("src/calc.rs", FileStatus::Modified, 2, 0)],
        );

        let signal = compute_coverage_signal(&[diff], None, Some(&repo));
        assert!(signal.inline_tests_checked);
        assert_eq!(
            signal.inline_tested_count, 0,
            "a cfg(test) import without #[test] must not count as inline-tested"
        );
        assert_eq!(signal.uncovered_files, vec!["src/calc.rs"]);
        assert!(signal.coverage_pct < 100);
    }

    #[test]
    fn coverage_delta_empty() {
        let tmp = TempDir::new().unwrap();
        let diff = mock_diff(vec![]);

        let signal = compute_coverage_signal(&[diff], None, None);
        generate_coverage_delta(tmp.path(), &signal).unwrap();

        let content = fs::read_to_string(tmp.path().join("coverage-delta.txt")).unwrap();
        assert!(content.contains("0/0"));
        assert!(content.contains("100%"));
    }

    #[test]
    fn coverage_text_and_struct_are_consistent() {
        // Guard test: the text artifact and the structured CoverageDelta
        // must report the same numbers when derived from the same signal.
        let diff = mock_diff(vec![
            mock_file_change("src/lib.rs", FileStatus::Modified, 10, 5),
            mock_file_change("src/utils.rs", FileStatus::Added, 20, 0),
            mock_file_change("tests/lib_test.rs", FileStatus::Modified, 5, 2),
        ]);

        let signal = compute_coverage_signal(&[diff], None, None);
        let delta = CoverageDelta::from_signal(&signal);

        let tmp = TempDir::new().unwrap();
        generate_coverage_delta(tmp.path(), &signal).unwrap();
        let text = fs::read_to_string(tmp.path().join("coverage-delta.txt")).unwrap();

        // Text must contain the same numbers as structured
        assert!(
            text.contains(&format!("{}/{}", delta.covered_count, delta.total_source)),
            "Text artifact must show {}/{} but got:\n{}",
            delta.covered_count,
            delta.total_source,
            text
        );
        assert!(
            text.contains(&format!("{}%", delta.pct)),
            "Text artifact must show {}% but got:\n{}",
            delta.pct,
            text
        );
        assert!(
            text.contains(&format!("Confidence: {}", signal.confidence)),
            "Text artifact must show Confidence: {} but got:\n{}",
            signal.confidence,
            text
        );

        // Structured delta must match signal
        assert_eq!(delta.covered_count, signal.covered_count);
        assert_eq!(delta.total_source, signal.total_source_files);
        assert_eq!(delta.pct, signal.coverage_pct);
    }

    #[test]
    fn coverage_ghost_test_deleted_source_test_remains() {
        let tmp = TempDir::new().unwrap();
        // Create a physical test file so the scanner finds it
        fs::create_dir_all(tmp.path().join("src/foo")).unwrap();
        fs::write(tmp.path().join("src/foo/bar_test.rs"), "test code").unwrap();

        let diff = mock_diff(vec![mock_file_change(
            "src/foo/bar.rs",
            FileStatus::Deleted,
            10,
            5,
        )]);

        let signal = compute_coverage_signal(&[diff], Some(tmp.path()), None);
        assert_eq!(signal.ghost_tests.len(), 1);
        assert_eq!(signal.ghost_tests[0].0, "src/foo/bar.rs");
        assert_eq!(signal.ghost_tests[0].1, "src/foo/bar_test.rs");

        generate_coverage_delta(tmp.path(), &signal).unwrap();
        let content = fs::read_to_string(tmp.path().join("coverage-delta.txt")).unwrap();
        assert!(content.contains("ORPHANED_TEST_DETECTED"));
        assert!(content.contains("src/foo/bar.rs -> likely ghost test: src/foo/bar_test.rs"));
    }

    #[test]
    fn coverage_ghost_test_deleted_source_and_test_together() {
        let tmp = TempDir::new().unwrap();
        // Even if physical file existed, git diff includes the deletion of the test file.
        // But since we simulate tempfs, let's not create it (meaning it was deleted).
        let diff = mock_diff(vec![
            mock_file_change("src/api.rs", FileStatus::Deleted, 10, 5),
            mock_file_change("src/api_test.rs", FileStatus::Deleted, 10, 5),
        ]);

        let signal = compute_coverage_signal(&[diff], Some(tmp.path()), None);
        // Both were deleted, diff hygiene is clean.
        assert_eq!(signal.ghost_tests.len(), 0);
    }

    #[test]
    fn coverage_ghost_test_deleted_source_no_test_ever() {
        let tmp = TempDir::new().unwrap();
        let diff = mock_diff(vec![mock_file_change(
            "src/main.rs",
            FileStatus::Deleted,
            10,
            5,
        )]);

        let signal = compute_coverage_signal(&[diff], Some(tmp.path()), None);
        // Source deleted, and there happens to be NO test for it on disk anyway.
        assert_eq!(signal.ghost_tests.len(), 0);
        let delta = CoverageDelta::from_signal(&signal);
        assert_eq!(delta.ghost_tests.len(), 0);
    }

    #[test]
    fn coverage_ghost_test_multiple_deleted() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir_all(tmp.path().join("src")).unwrap();
        fs::write(tmp.path().join("src/a_test.rs"), "test code").unwrap();
        fs::write(tmp.path().join("src/b_test.rs"), "test code").unwrap();

        let diff = mock_diff(vec![
            mock_file_change("src/a.rs", FileStatus::Deleted, 10, 5),
            mock_file_change("src/b.rs", FileStatus::Deleted, 10, 5),
            mock_file_change("src/c.rs", FileStatus::Deleted, 10, 5),
        ]);

        let signal = compute_coverage_signal(&[diff], Some(tmp.path()), None);
        // 'a' i 'b' osierocone, 'c' w ogóle nie miał testów.
        assert_eq!(signal.ghost_tests.len(), 2);
    }
}
