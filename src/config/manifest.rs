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
        let path = repo_root.join("prview.toml");
        if !path.exists() {
            return None;
        }

        let content = std::fs::read_to_string(&path).ok()?;
        match toml::from_str(&content) {
            Ok(manifest) => Some(manifest),
            Err(e) => {
                eprintln!("warning: failed to parse {}: {}", path.display(), e);
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
        // No prview.toml created
        let manifest = PrviewManifest::load_from(tmp.path());
        assert!(manifest.is_none());
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
