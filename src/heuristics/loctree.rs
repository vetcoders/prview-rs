//! Loctree-suite integration (direct library usage)
//!
//! Uses loctree as a library for comprehensive code analysis:
//! - Dead code / unused exports detection
//! - Circular import detection
//! - Semantic duplicates (dead parrots, exact twins)
//! - Project statistics

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;

// Import loctree library directly
use loctree::analyzer::twins::detect_exact_twins;
use loctree::analyzer::{cycles, dead_parrots, twins};
use loctree::args::ParsedArgs;
use loctree::snapshot::{Snapshot, project_cache_dir};

/// Loctree-suite analysis results
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LoctreeAnalysis {
    pub stats: LoctreeStats,
    pub dead_exports: Vec<DeadExport>,
    pub cycles: Vec<CycleInfo>,
    pub twins: TwinsAnalysis,
    /// Whether loctree analysis completed successfully
    pub available: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LoctreeStats {
    pub total_files: usize,
    pub total_loc: usize,
    pub by_language: std::collections::HashMap<String, LanguageStats>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LanguageStats {
    pub files: usize,
    pub loc: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeadExport {
    pub file: String,
    pub symbol: String,
    pub line: Option<usize>,
    pub confidence: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CycleInfo {
    pub files: Vec<String>,
    pub length: usize,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TwinsAnalysis {
    pub dead_parrots: Vec<DeadParrot>,
    pub exact_twins: Vec<TwinPair>,
    pub total_symbols: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TwinPair {
    pub file_a: String,
    pub file_b: String,
    pub symbol: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeadParrot {
    pub file: String,
    pub symbol: String,
    pub kind: String,
    pub line: usize,
}

fn is_valid_dead_export_symbol(symbol: &str) -> bool {
    let normalized = symbol.strip_prefix("r#").unwrap_or(symbol);
    if normalized.is_empty() {
        return false;
    }

    // Loctree can occasionally surface parser noise from comments or string
    // literals as fake exports. Filter reserved keywords and non-identifiers
    // before they reach the review surface.
    const RUST_KEYWORDS: &[&str] = &[
        "as", "async", "await", "break", "const", "continue", "crate", "dyn", "else", "enum",
        "extern", "false", "fn", "for", "if", "impl", "in", "let", "loop", "match", "mod", "move",
        "mut", "pub", "ref", "return", "self", "Self", "static", "struct", "super", "trait",
        "true", "type", "unsafe", "use", "where", "while",
    ];

    if RUST_KEYWORDS.contains(&normalized) {
        return false;
    }

    let mut chars = normalized.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !(first.is_ascii_alphabetic() || first == '_') {
        return false;
    }

    chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
}

/// Run loctree analysis using the library directly (no subprocess!)
pub async fn run_loctree(root: &Path) -> Result<LoctreeAnalysis> {
    let mut analysis = LoctreeAnalysis::default();

    // Try to load existing snapshot or create new one.
    //
    // Fail loud, not fail-open: a blind loctree must surface as "not available",
    // never as a green "✓ 0 dead exports". Returning Err drives the degraded
    // branch in `run_all` and keeps the zeroed summary out of regression deltas
    // (a failed scan would otherwise read as a real +N/-N change against the
    // other side).
    let snapshot = match load_or_create_snapshot(root).await {
        Ok(s) => s,
        Err(e) => {
            return Err(e.context("loctree analysis unavailable (snapshot load/create failed)"));
        }
    };

    analysis.available = true;

    // Extract stats from snapshot metadata
    analysis.stats.total_files = snapshot.metadata.file_count;
    analysis.stats.total_loc = snapshot.metadata.total_loc;

    // Build language stats from file analyses
    for file in &snapshot.files {
        let lang = &file.language;
        let entry = analysis
            .stats
            .by_language
            .entry(lang.clone())
            .or_insert_with(LanguageStats::default);
        entry.files += 1;
        entry.loc += file.loc;
    }

    // Find dead exports using the correct API
    let dead_config = dead_parrots::DeadFilterConfig::default();
    let dead = dead_parrots::find_dead_exports(
        &snapshot.files,
        true, // high_confidence
        None, // open_base
        dead_config,
    );

    for d in dead {
        if !is_valid_dead_export_symbol(&d.symbol) {
            continue;
        }
        analysis.dead_exports.push(DeadExport {
            file: d.file.clone(),
            symbol: d.symbol.clone(),
            line: d.line,
            confidence: d.confidence.clone(),
        });
    }

    // Find circular imports (strict only — excludes lazy_import and type_import edges,
    // matching `loct cycles` behavior)
    let edges: Vec<(String, String, String)> = snapshot
        .edges
        .iter()
        .map(|e| (e.from.clone(), e.to.clone(), e.label.clone()))
        .collect();

    let (strict_cycles, _lazy_cycles) = cycles::find_cycles_with_lazy(&edges);
    for cycle in strict_cycles {
        let len = cycle.len();
        analysis.cycles.push(CycleInfo {
            files: cycle,
            length: len,
        });
    }

    // Find dead parrots (symbols with 0 imports)
    let twins_result = twins::find_dead_parrots(
        &snapshot.files,
        false, // dead_only
        false, // include_tests
    );

    analysis.twins.total_symbols = twins_result.total_symbols;
    for parrot in twins_result.dead_parrots {
        analysis.twins.dead_parrots.push(DeadParrot {
            file: parrot.file_path.clone(),
            symbol: parrot.name.clone(),
            kind: parrot.kind.clone(),
            line: parrot.line,
        });
    }

    // Find exact twins (symbols exported from multiple files)
    let exact_twins = detect_exact_twins(&snapshot.files, false);
    for twin in exact_twins {
        if let [a, b, ..] = &twin.locations[..] {
            analysis.twins.exact_twins.push(TwinPair {
                file_a: a.file_path.clone(),
                file_b: b.file_path.clone(),
                symbol: twin.name.clone(),
            });
        }
    }

    Ok(analysis)
}

/// Extract "major.minor" from a semver string (e.g. "0.8.14" -> "0.8")
fn schema_major_minor(version: &str) -> &str {
    match version
        .find('.')
        .and_then(|i| version[i + 1..].find('.').map(|j| i + 1 + j))
    {
        Some(end) => &version[..end],
        None => version,
    }
}

/// Load existing snapshot or create a new one.
/// Rejects snapshots with mismatched major.minor schema version to avoid
/// stale data from older loctree versions producing false positives.
async fn load_or_create_snapshot(root: &std::path::Path) -> Result<Snapshot> {
    let expected = loctree::snapshot::SNAPSHOT_SCHEMA_VERSION;

    // Try loading existing snapshot first
    if let Ok(snapshot) = Snapshot::load(root) {
        if schema_major_minor(&snapshot.metadata.schema_version) == schema_major_minor(expected) {
            // Freshness gate: a schema-compatible snapshot can still be stale —
            // git HEAD moved or the worktree is dirty since it was scanned. Serving
            // it would present days-old dead-export/LOC counts as the current run's
            // signal (the "numbers don't move despite my changes" complaint). Re-scan
            // on staleness. is_stale() returns false for non-git dirs (extracted
            // base/target snapshots), so remote-mode scans are unaffected.
            if !snapshot.is_stale(root) {
                return Ok(snapshot);
            }
            eprintln!("  [loctree] Snapshot stale (HEAD moved or worktree dirty), re-scanning");
        } else {
            eprintln!(
                "  [loctree] Stale snapshot ({}), re-scanning for {}",
                snapshot.metadata.schema_version, expected
            );
        }
    }

    // Create new snapshot by scanning
    let roots = vec![root.to_path_buf()];
    let parsed = ParsedArgs::default();

    let roots_clone = roots.clone();
    let parsed_clone = parsed.clone();
    tokio::task::spawn_blocking(move || loctree::snapshot::run_init(&roots_clone, &parsed_clone))
        .await?
        .context("Failed to run loctree scan")?;

    // Load from the exact cache path where run_init() saves (not Snapshot::load()
    // which may still pick up a legacy .loctree/ snapshot before the new cache entry).
    let snapshot_path = Snapshot::snapshot_path(root);
    let cache_root = project_cache_dir(root);
    let content = crate::paths::read_to_string_within(&cache_root, &snapshot_path)
        .with_context(|| format!("Failed to read snapshot from {}", snapshot_path.display()))?;
    let snapshot: Snapshot =
        serde_json::from_str(&content).context("Failed to parse freshly created snapshot")?;
    Ok(snapshot)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn load_or_create_snapshot_rescans_when_head_moves() {
        // A schema-compatible but stale cached snapshot (HEAD moved since the
        // scan) must not be served as the current signal — it would freeze the
        // dead-export/LOC counts across runs ("numbers don't move despite my
        // changes"). After a new commit the re-scan must reflect the new file.
        use crate::git::git_cmd;
        let repo = tempfile::tempdir().unwrap();
        let root = repo.path();

        let run_git = |args: &[&str]| {
            git_cmd()
                .args(args)
                .current_dir(root)
                .output()
                .expect("git command failed");
        };
        run_git(&["init"]);
        run_git(&["config", "user.email", "t@test.com"]);
        run_git(&["config", "user.name", "Test"]);
        run_git(&["config", "commit.gpgsign", "false"]);
        std::fs::write(root.join("a.rs"), "pub fn a() {}\n").unwrap();
        run_git(&["add", "."]);
        run_git(&["commit", "-m", "one"]);

        let snap1 = load_or_create_snapshot(root)
            .await
            .expect("first scan should succeed");
        let files1 = snap1.metadata.file_count;

        // HEAD moves: a second source file is committed.
        std::fs::write(root.join("b.rs"), "pub fn b() {}\n").unwrap();
        run_git(&["add", "."]);
        run_git(&["commit", "-m", "two"]);

        let snap2 = load_or_create_snapshot(root)
            .await
            .expect("second scan should succeed");
        assert!(
            snap2.metadata.file_count > files1,
            "stale snapshot served: file_count did not grow after HEAD moved ({} -> {})",
            files1,
            snap2.metadata.file_count
        );
    }

    #[test]
    fn test_loctree_analysis_default() {
        let analysis = LoctreeAnalysis::default();
        assert!(!analysis.available);
        assert_eq!(analysis.stats.total_files, 0);
        assert_eq!(analysis.stats.total_loc, 0);
        assert!(analysis.dead_exports.is_empty());
        assert!(analysis.cycles.is_empty());
    }

    #[test]
    fn test_loctree_stats_default() {
        let stats = LoctreeStats::default();
        assert_eq!(stats.total_files, 0);
        assert_eq!(stats.total_loc, 0);
        assert!(stats.by_language.is_empty());
    }

    #[test]
    fn test_language_stats_default() {
        let stats = LanguageStats::default();
        assert_eq!(stats.files, 0);
        assert_eq!(stats.loc, 0);
    }

    #[test]
    fn test_language_stats_creation() {
        let stats = LanguageStats {
            files: 50,
            loc: 2500,
        };
        assert_eq!(stats.files, 50);
        assert_eq!(stats.loc, 2500);
    }

    #[test]
    fn test_dead_export_creation() {
        let export = DeadExport {
            file: "src/utils.rs".to_string(),
            symbol: "unused_function".to_string(),
            line: Some(42),
            confidence: "high".to_string(),
        };
        assert_eq!(export.file, "src/utils.rs");
        assert_eq!(export.symbol, "unused_function");
        assert_eq!(export.line, Some(42));
        assert_eq!(export.confidence, "high");
    }

    #[test]
    fn test_dead_export_no_line() {
        let export = DeadExport {
            file: "src/lib.rs".to_string(),
            symbol: "dead_const".to_string(),
            line: None,
            confidence: "medium".to_string(),
        };
        assert!(export.line.is_none());
    }

    #[test]
    fn test_valid_dead_export_symbol_accepts_identifiers() {
        assert!(is_valid_dead_export_symbol("unused_function"));
        assert!(is_valid_dead_export_symbol("_internal"));
        assert!(is_valid_dead_export_symbol("r#type_alias"));
    }

    #[test]
    fn test_valid_dead_export_symbol_rejects_keywords_and_noise() {
        assert!(!is_valid_dead_export_symbol("for"));
        assert!(!is_valid_dead_export_symbol("match"));
        assert!(!is_valid_dead_export_symbol("123oops"));
        assert!(!is_valid_dead_export_symbol("foo-bar"));
        assert!(!is_valid_dead_export_symbol(""));
    }

    #[test]
    fn test_cycle_info_creation() {
        let cycle = CycleInfo {
            files: vec!["a.rs".to_string(), "b.rs".to_string(), "c.rs".to_string()],
            length: 3,
        };
        assert_eq!(cycle.files.len(), 3);
        assert_eq!(cycle.length, 3);
    }

    #[test]
    fn test_twins_analysis_default() {
        let twins = TwinsAnalysis::default();
        assert!(twins.dead_parrots.is_empty());
        assert!(twins.exact_twins.is_empty());
        assert_eq!(twins.total_symbols, 0);
    }

    #[test]
    fn test_twin_pair_creation() {
        let pair = TwinPair {
            file_a: "src/old.rs".to_string(),
            file_b: "src/new.rs".to_string(),
            symbol: "duplicate_fn".to_string(),
        };
        assert_eq!(pair.file_a, "src/old.rs");
        assert_eq!(pair.file_b, "src/new.rs");
        assert_eq!(pair.symbol, "duplicate_fn");
    }

    #[test]
    fn test_dead_parrot_creation() {
        let parrot = DeadParrot {
            file: "src/unused.rs".to_string(),
            symbol: "never_called".to_string(),
            kind: "function".to_string(),
            line: 100,
        };
        assert_eq!(parrot.file, "src/unused.rs");
        assert_eq!(parrot.symbol, "never_called");
        assert_eq!(parrot.kind, "function");
        assert_eq!(parrot.line, 100);
    }

    #[test]
    fn test_loctree_analysis_with_data() {
        let mut by_language = std::collections::HashMap::new();
        by_language.insert(
            "rust".to_string(),
            LanguageStats {
                files: 10,
                loc: 500,
            },
        );
        by_language.insert(
            "typescript".to_string(),
            LanguageStats { files: 5, loc: 300 },
        );

        let analysis = LoctreeAnalysis {
            stats: LoctreeStats {
                total_files: 15,
                total_loc: 800,
                by_language,
            },
            dead_exports: vec![DeadExport {
                file: "test.rs".to_string(),
                symbol: "unused".to_string(),
                line: Some(1),
                confidence: "high".to_string(),
            }],
            cycles: vec![CycleInfo {
                files: vec!["a.rs".to_string(), "b.rs".to_string()],
                length: 2,
            }],
            twins: TwinsAnalysis::default(),
            available: true,
        };

        assert!(analysis.available);
        assert_eq!(analysis.stats.total_files, 15);
        assert_eq!(analysis.dead_exports.len(), 1);
        assert_eq!(analysis.cycles.len(), 1);
    }

    #[test]
    fn test_loctree_stats_serialization() {
        let stats = LoctreeStats {
            total_files: 100,
            total_loc: 5000,
            by_language: std::collections::HashMap::new(),
        };
        let json = serde_json::to_string(&stats).unwrap();
        assert!(json.contains("\"total_files\":100"));
        assert!(json.contains("\"total_loc\":5000"));
    }

    #[test]
    fn test_dead_export_serialization() {
        let export = DeadExport {
            file: "test.rs".to_string(),
            symbol: "foo".to_string(),
            line: Some(10),
            confidence: "high".to_string(),
        };
        let json = serde_json::to_string(&export).unwrap();
        assert!(json.contains("\"file\":\"test.rs\""));
        assert!(json.contains("\"symbol\":\"foo\""));
    }

    #[test]
    fn test_cycle_info_serialization() {
        let cycle = CycleInfo {
            files: vec!["x.rs".to_string()],
            length: 1,
        };
        let json = serde_json::to_string(&cycle).unwrap();
        assert!(json.contains("\"length\":1"));
    }

    #[test]
    fn test_clone_implementations() {
        let stats = LoctreeStats::default();
        let cloned = stats.clone();
        assert_eq!(stats.total_files, cloned.total_files);

        let export = DeadExport {
            file: "f".to_string(),
            symbol: "s".to_string(),
            line: None,
            confidence: "low".to_string(),
        };
        let cloned = export.clone();
        assert_eq!(export.file, cloned.file);
    }
}
