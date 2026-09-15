use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct PrviewManifest {
    #[serde(default)]
    pub project: ProjectConfig,
    #[serde(default)]
    pub lint: LintConfig,
    #[serde(default)]
    pub gate: GateConfig,
    #[serde(default)]
    pub scope: ScopeConfig,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ProjectConfig {
    pub cargo_root: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct LintConfig {
    pub ignore_patterns: Option<Vec<String>>,
}

/// `[gate]` section of `prview.toml`. Controls merge-gate verdict behaviour.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct GateConfig {
    /// Whether a detected breaking API change escalates the merge verdict from
    /// PASS to CONDITIONAL (never BLOCK). `None` → default on. Set to `false`
    /// to keep the breaking findings visible as an informational caveat only,
    /// with no effect on the verdict.
    pub breaking_escalation: Option<bool>,
}

/// `[scope]` section of `prview.toml`. Controls which changed paths are treated
/// as inputs to TEST SELECTION — and nothing else. A path named here still
/// appears in the diff, the artifacts, the signals and the verdict exactly as
/// before; it simply does not decide which tests have to run.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ScopeConfig {
    /// Extra paths this repository can say are not inputs to test selection,
    /// as glob patterns matched against repo-relative paths. Additive to the
    /// built-in list unless that list is disabled.
    ///
    /// This is NOT an ignore list: a matching path is classified
    /// `non-participating`, which means "known not to be an input", and is
    /// skipped for selection without escalating. Anything the classifier cannot
    /// name stays `unknown` and still forces a full run.
    pub non_participating: Option<Vec<String>>,
    /// Whether prview's built-in non-participating rules apply. `None` → on.
    /// Set to `false` to restore strictly escalating behaviour, where only the
    /// repository's own `non_participating` patterns (if any) are neutral.
    pub non_participating_builtins: Option<bool>,
}

impl PrviewManifest {
    pub fn load_from(repo_root: &Path) -> Option<Self> {
        Self::load_from_with_warning_sink(repo_root, |warning| eprintln!("{warning}"))
    }

    fn load_from_with_warning_sink(repo_root: &Path, mut warn: impl FnMut(String)) -> Option<Self> {
        let path = repo_root.join("prview.toml");

        let content = match std::fs::read_to_string(&path) {
            Ok(content) => content,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
            Err(e) => {
                warn(format!("warning: failed to read {}: {}", path.display(), e));
                return None;
            }
        };

        match toml::from_str(&content) {
            Ok(manifest) => Some(manifest),
            Err(e) => {
                warn(format!(
                    "warning: failed to parse {}: {}",
                    path.display(),
                    e
                ));
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn test_load_valid_manifest() {
        let tmp = TempDir::new().unwrap();
        let toml_content = r#"
[project]
cargo_root = "backend"

[lint]
ignore_patterns = ["generated/**", "vendor/**"]
"#;
        fs::write(tmp.path().join("prview.toml"), toml_content).unwrap();

        let manifest = PrviewManifest::load_from(tmp.path());
        assert!(manifest.is_some());
        let m = manifest.unwrap();
        assert_eq!(m.project.cargo_root.as_deref(), Some("backend"));
        assert_eq!(
            m.lint.ignore_patterns.as_deref(),
            Some(vec!["generated/**".to_string(), "vendor/**".to_string()]).as_deref()
        );
    }

    #[test]
    fn test_load_partial_manifest_lint_only() {
        let tmp = TempDir::new().unwrap();
        let toml_content = r#"
[lint]
ignore_patterns = ["*.generated.ts"]
"#;
        fs::write(tmp.path().join("prview.toml"), toml_content).unwrap();

        let manifest = PrviewManifest::load_from(tmp.path());
        assert!(manifest.is_some());
        let m = manifest.unwrap();
        // project section uses defaults
        assert_eq!(m.project.cargo_root, None);
        assert!(m.lint.ignore_patterns.is_some());
        assert_eq!(m.lint.ignore_patterns.unwrap().len(), 1);
    }

    #[test]
    fn test_load_missing_file_returns_none() {
        let tmp = TempDir::new().unwrap();
        let mut warnings = Vec::new();

        let manifest = PrviewManifest::load_from_with_warning_sink(tmp.path(), |warning| {
            warnings.push(warning)
        });

        assert!(manifest.is_none());
        assert!(warnings.is_empty(), "missing manifest should stay quiet");
    }

    #[test]
    fn test_load_unreadable_manifest_warns_and_returns_none() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir(tmp.path().join("prview.toml")).unwrap();
        let mut warnings = Vec::new();

        let manifest = PrviewManifest::load_from_with_warning_sink(tmp.path(), |warning| {
            warnings.push(warning)
        });

        assert!(manifest.is_none());
        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("warning: failed to read")),
            "expected manifest read warning, got: {warnings:?}"
        );
    }

    #[test]
    fn test_load_gate_breaking_escalation_knob() {
        let tmp = TempDir::new().unwrap();
        let toml_content = r#"
[gate]
breaking_escalation = false
"#;
        fs::write(tmp.path().join("prview.toml"), toml_content).unwrap();

        let manifest = PrviewManifest::load_from(tmp.path()).expect("manifest loads");
        assert_eq!(manifest.gate.breaking_escalation, Some(false));
    }

    #[test]
    fn test_gate_section_defaults_to_none_when_absent() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("prview.toml"), "[project]\n").unwrap();

        let manifest = PrviewManifest::load_from(tmp.path()).expect("manifest loads");
        assert_eq!(manifest.gate.breaking_escalation, None);
    }

    #[test]
    fn test_load_scope_non_participating_knobs() {
        let tmp = TempDir::new().unwrap();
        let toml_content = r#"
[scope]
non_participating = ["design/**", "*.drawio"]
non_participating_builtins = false
"#;
        fs::write(tmp.path().join("prview.toml"), toml_content).unwrap();

        let manifest = PrviewManifest::load_from(tmp.path()).expect("manifest loads");
        assert_eq!(
            manifest.scope.non_participating.as_deref(),
            Some(vec!["design/**".to_string(), "*.drawio".to_string()]).as_deref()
        );
        assert_eq!(manifest.scope.non_participating_builtins, Some(false));
    }

    #[test]
    fn test_scope_section_defaults_to_none_when_absent() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("prview.toml"), "[project]\n").unwrap();

        let manifest = PrviewManifest::load_from(tmp.path()).expect("manifest loads");
        assert_eq!(manifest.scope.non_participating, None);
        assert_eq!(manifest.scope.non_participating_builtins, None);
    }

    #[test]
    fn test_load_empty_toml_returns_defaults() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("prview.toml"), "").unwrap();

        let manifest = PrviewManifest::load_from(tmp.path());
        assert!(manifest.is_some());
        let m = manifest.unwrap();
        assert_eq!(m, PrviewManifest::default());
    }
}
