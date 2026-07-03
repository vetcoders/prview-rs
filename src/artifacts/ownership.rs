//! Ownership map: CODEOWNERS parsing plus path-based fallback.

use super::*;

// ── Ownership map (CODEOWNERS + path-based fallback) ─────────────

/// A single CODEOWNERS entry mapping a pattern to one or more owners.
pub(crate) struct OwnershipEntry {
    pub pattern: String,
    pub owners: Vec<String>,
}

/// Parse a CODEOWNERS file if it exists.
///
/// Searches (in order): `.github/CODEOWNERS`, `CODEOWNERS`, `docs/CODEOWNERS`.
/// Returns an empty vec on any error — completely defensive.
pub(crate) fn load_codeowners(repo_root: &Path) -> Vec<OwnershipEntry> {
    let candidates = [
        repo_root.join(".github/CODEOWNERS"),
        repo_root.join("CODEOWNERS"),
        repo_root.join("docs/CODEOWNERS"),
    ];

    let content = candidates.iter().find_map(|p| fs::read_to_string(p).ok());

    let Some(content) = content else {
        return Vec::new();
    };

    parse_codeowners(&content)
}

/// Parse CODEOWNERS content into ownership entries.
pub(crate) fn parse_codeowners(content: &str) -> Vec<OwnershipEntry> {
    let mut entries = Vec::new();
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut parts = line.split_whitespace();
        let Some(pattern) = parts.next() else {
            continue;
        };
        let owners: Vec<String> = parts.map(|s| s.to_string()).collect();
        if owners.is_empty() {
            continue;
        }
        entries.push(OwnershipEntry {
            pattern: pattern.to_string(),
            owners,
        });
    }
    entries
}

/// Find the owner for a given file path.
///
/// Strategy:
/// 1. Match against CODEOWNERS patterns (last match wins, per GitHub convention)
/// 2. If no CODEOWNERS match: use the second path component as module name
///    (e.g. `src/checks/foo.rs` -> `checks`, `tests/unit.rs` -> `tests`)
/// 3. If path has only one component: `root`
/// 4. Absolute fallback: `unassigned`
pub(crate) fn find_owner(path: &str, codeowners: &[OwnershipEntry]) -> String {
    // CODEOWNERS: last matching rule wins (GitHub convention)
    let mut best_match: Option<&[String]> = None;
    for entry in codeowners {
        if codeowners_pattern_matches(&entry.pattern, path) {
            best_match = Some(&entry.owners);
        }
    }
    if let Some(owners) = best_match {
        return owners.join(", ");
    }

    // Path-based fallback: use second component as module name
    path_based_module(path)
}

/// Derive a module name from a file path.
///
/// `src/checks/foo.rs` -> `checks`
/// `tests/unit.rs` -> `tests`
/// `Cargo.toml` -> `root`
pub(crate) fn path_based_module(path: &str) -> String {
    let parts: Vec<&str> = path.split('/').collect();
    if parts.len() >= 3 {
        // src/checks/foo.rs -> "checks"
        parts[1].to_string()
    } else if parts.len() == 2 {
        // tests/foo.rs -> "tests"
        parts[0].to_string()
    } else {
        "root".to_string()
    }
}

/// Simple CODEOWNERS pattern matching.
///
/// Supports:
/// - `*` as glob (matches any sequence of non-`/` characters)
/// - `*.ext` matches any file with that extension
/// - `dir/` matches anything under that directory
/// - Literal path prefix matching
pub(crate) fn codeowners_pattern_matches(pattern: &str, path: &str) -> bool {
    let pattern = pattern.trim_start_matches('/');

    // `**/*.ext` — double-star extension glob: match any file ending with that
    // extension regardless of directory depth (e.g. `**/*.rs` matches `src/deep/file.rs`)
    if let Some(rest) = pattern.strip_prefix("**/") {
        // `**/dirname/` — match any path containing that directory
        if rest.ends_with('/') {
            let dir_segment = rest.trim_end_matches('/');
            // Match if path contains /dirname/ or starts with dirname/
            return path.starts_with(&format!("{dir_segment}/"))
                || path.contains(&format!("/{dir_segment}/"));
        }
        // `**/*.ext` — match by suffix (e.g. rest = "*.rs")
        if let Some(suffix) = rest.strip_prefix('*') {
            return path.ends_with(suffix);
        }
        // `**/filename` — match literal filename at any depth
        return path == rest || path.ends_with(&format!("/{rest}"));
    }

    // `*.ext` — extension glob at any depth
    if pattern.starts_with('*') && !pattern[1..].contains('*') {
        let suffix = &pattern[1..]; // e.g. ".rs"
        return path.ends_with(suffix);
    }

    // `dir/` — directory prefix
    if pattern.ends_with('/') {
        let prefix = pattern.trim_end_matches('/');
        return path.starts_with(prefix) && path.as_bytes().get(prefix.len()) == Some(&b'/');
    }

    // `some/path/*` — everything under a directory
    if let Some(prefix) = pattern.strip_suffix("/*") {
        return path.starts_with(prefix) && path.as_bytes().get(prefix.len()) == Some(&b'/');
    }

    // Exact match
    if path == pattern {
        return true;
    }

    // Prefix match (pattern is a directory without trailing slash)
    if path.starts_with(pattern) && path.as_bytes().get(pattern.len()) == Some(&b'/') {
        return true;
    }

    false
}

/// Build an ownership map for a set of file paths.
pub(crate) fn build_ownership_map(
    repo_root: &Path,
    file_paths: &[String],
) -> Vec<(String, String)> {
    let codeowners = load_codeowners(repo_root);
    file_paths
        .iter()
        .map(|path| {
            let owner = find_owner(path, &codeowners);
            (path.clone(), owner)
        })
        .collect()
}
