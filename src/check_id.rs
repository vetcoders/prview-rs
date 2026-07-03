//! Canonical check-name → check-id normalization.
//!
//! A single source of truth for turning a human check name (e.g. `"cargo
//! check"`, `"TypeScript"`) into the stable id used across policy severity
//! lookup, terminal/JSON output, and the artifact contract (FINDINGS / SARIF /
//! MERGE_GATE). Three independent copies previously drifted (naive vs.
//! separator-collapsing vs. alias-table), which produced different ids for the
//! same check in policy vs. output vs. findings — the exact drift class that
//! bit merge-gate evidence matching (commit ce99682). This module is the
//! canon: the alias table.
//!
//! The alias table is *additive* — new display names may map to an existing id
//! — so extending it never silently repartitions an existing name. The
//! snapshot test below pins the current mapping so any change is deliberate.

/// Normalize a check display name into its stable check id.
///
/// Known tool names map through the alias table (e.g. `typescript → tsc`,
/// `cargo check → cargo`, `vitest → tests`); anything else falls back to a
/// lowercase, separator-underscored slug.
pub fn check_id_from_name(name: &str) -> String {
    match name.to_ascii_lowercase().as_str() {
        "typescript" => "tsc".to_string(),
        "cargo check" => "cargo".to_string(),
        "clippy" => "clippy".to_string(),
        "rustfmt" => "rustfmt".to_string(),
        "cargo test" => "cargo_test".to_string(),
        "cargo audit" => "cargo_audit".to_string(),
        "cargo geiger" => "cargo_geiger".to_string(),
        "eslint" => "eslint".to_string(),
        "stylelint" => "stylelint".to_string(),
        "vitest" => "tests".to_string(),
        "ruff" => "ruff".to_string(),
        "mypy" => "mypy".to_string(),
        "pytest" => "pytest".to_string(),
        other => other.replace([' ', '-', '/'], "_"),
    }
}

#[cfg(test)]
mod tests {
    use super::check_id_from_name;

    /// Pins the canonical name → id mapping. If a change to `check_id_from_name`
    /// alters any existing name's id, this snapshot fails — forcing the change
    /// to be deliberate (aliases are additive by contract).
    #[test]
    fn check_id_mapping_snapshot() {
        let cases = [
            // Aliased tool names.
            ("typescript", "tsc"),
            ("TypeScript", "tsc"),
            ("cargo check", "cargo"),
            ("clippy", "clippy"),
            ("rustfmt", "rustfmt"),
            ("cargo test", "cargo_test"),
            ("cargo audit", "cargo_audit"),
            ("cargo geiger", "cargo_geiger"),
            ("eslint", "eslint"),
            ("stylelint", "stylelint"),
            ("vitest", "tests"),
            ("ruff", "ruff"),
            ("mypy", "mypy"),
            ("Mypy", "mypy"),
            ("pytest", "pytest"),
            // Fallback slugging (no alias).
            ("semgrep scan", "semgrep_scan"),
            ("heuristics_loctree", "heuristics_loctree"),
            ("Some-Weird/Name", "some_weird_name"),
        ];
        for (name, expected) in cases {
            assert_eq!(
                check_id_from_name(name),
                expected,
                "check_id drift for {name:?}"
            );
        }
    }
}
