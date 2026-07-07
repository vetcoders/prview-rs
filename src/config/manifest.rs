use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct PrviewManifest {
    #[serde(default)]
    pub project: ProjectConfig,
    #[serde(default)]
    pub lint: LintConfig,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ProjectConfig {
    pub cargo_root: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct LintConfig {
    pub ignore_patterns: Option<Vec<String>>,
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
    fn test_load_empty_toml_returns_defaults() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("prview.toml"), "").unwrap();

        let manifest = PrviewManifest::load_from(tmp.path());
        assert!(manifest.is_some());
        let m = manifest.unwrap();
        assert_eq!(m, PrviewManifest::default());
    }
}
