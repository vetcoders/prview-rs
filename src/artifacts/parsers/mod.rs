pub mod cargo_test;
pub mod clippy;
pub mod eslint;
pub mod semgrep;
pub mod stylelint;

/// A single lint/test finding extracted from tool output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LintFinding {
    pub file: String,
    pub line: u32,
    pub column: Option<u32>,
    /// "error" | "warning" | "note"
    pub level: &'static str,
    pub message: String,
    pub rule_id: Option<String>,
    /// Tool identifier: "eslint" | "clippy" | "stylelint" | "cargo_test"
    pub source: &'static str,
}

/// Paths that should be excluded from findings (generated/vendored code).
///
/// Root-level segments (`target/`, `build/`, `dist/`, `coverage/`, `.next/`) are
/// only matched at the start of the path. `node_modules/` is matched anywhere
/// because it can appear nested in monorepos.
pub fn is_generated_path(path: &str) -> bool {
    let stripped = path.trim_start_matches('/');
    // Root-level generated dirs.
    const ROOT_PREFIXES: &[&str] = &["target/", "coverage/", "dist/", ".next/", "build/"];
    if ROOT_PREFIXES.iter().any(|p| stripped.starts_with(p)) {
        return true;
    }
    // node_modules can appear at any depth (monorepo).
    stripped.contains("node_modules/") || stripped.starts_with("node_modules/")
}

#[cfg(test)]
mod tests {
    use super::is_generated_path;

    #[test]
    fn generated_path_matches_known_segments() {
        assert!(is_generated_path("target/debug/build/foo.rs"));
        assert!(is_generated_path("coverage/lcov.info"));
        assert!(is_generated_path("node_modules/pkg/index.js"));
        assert!(is_generated_path("dist/app.js"));
        assert!(is_generated_path(".next/cache/chunk.js"));
        assert!(is_generated_path("build/output.css"));
    }

    #[test]
    fn generated_path_ignores_authored_files() {
        assert!(!is_generated_path("src/main.rs"));
        assert!(!is_generated_path("tests/integration.rs"));
        assert!(!is_generated_path("Cargo.toml"));
        assert!(!is_generated_path("docs/building.md"));
    }

    #[test]
    fn no_false_positives_on_substring_matches() {
        assert!(!is_generated_path("src/build/helper.rs"));
        assert!(!is_generated_path("src/target/selector.rs"));
        assert!(!is_generated_path("src/dist/chart.js"));
        assert!(!is_generated_path("rebuild/index.ts"));
    }
}
