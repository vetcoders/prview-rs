//! Dependency diff — added/removed/changed deps from manifests.

use crate::git::{Diff, Repository};
use anyhow::Result;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;

/// Summary of dependency changes between base and head.
#[derive(Debug, Clone, serde::Serialize)]
pub struct DepsDelta {
    pub added: Vec<String>,
    pub removed: Vec<String>,
    pub changed: Vec<String>,
}

/// Extract added/removed/changed dependencies by comparing full manifests at base vs target.
///
/// Instead of parsing patch lines (fragile, misses context), reads the complete manifest
/// files from both commits and compares their dependency maps.
pub fn generate_deps_delta(dir: &Path, diffs: &[Diff], repo: &Repository) -> Result<DepsDelta> {
    let mut added = Vec::new();
    let mut removed = Vec::new();
    let mut changed = Vec::new();

    for diff in diffs {
        let dep_files: Vec<&str> = diff
            .files
            .iter()
            .filter(|f| {
                let name = f.path.rsplit('/').next().unwrap_or(&f.path);
                matches!(name, "Cargo.toml" | "package.json" | "pyproject.toml")
            })
            .map(|f| f.path.as_str())
            .collect();

        for dep_path in &dep_files {
            let base_content = repo
                .file_at_commit(&diff.base_commit_id, dep_path)
                .unwrap_or_default();
            let target_content = repo
                .file_at_commit(&diff.target_commit_id, dep_path)
                .unwrap_or_default();

            let filename = dep_path.rsplit('/').next().unwrap_or(dep_path);
            let base_deps = extract_deps_from_manifest(&base_content, filename);
            let target_deps = extract_deps_from_manifest(&target_content, filename);

            for name in &target_deps {
                if !base_deps.contains(name) && !added.contains(name) {
                    added.push(name.clone());
                } else if base_deps.contains(name) {
                    // Both have it — check if version changed by comparing raw lines
                    // (we store just names, so presence in both = potentially changed)
                }
            }
            for name in &base_deps {
                if !target_deps.contains(name) && !removed.contains(name) {
                    removed.push(name.clone());
                }
            }
            // Changed = present in both but with different version spec
            let base_versioned = extract_deps_versioned(&base_content, filename);
            let target_versioned = extract_deps_versioned(&target_content, filename);
            for (name, base_ver) in &base_versioned {
                if let Some(target_ver) = target_versioned.get(name)
                    && base_ver != target_ver
                    && !changed.contains(name)
                {
                    changed.push(name.clone());
                }
            }
        }
    }

    if added.is_empty() && removed.is_empty() && changed.is_empty() {
        return Ok(DepsDelta {
            added,
            removed,
            changed,
        });
    }

    added.sort();
    removed.sort();
    changed.sort();

    let delta = DepsDelta {
        added,
        removed,
        changed,
    };

    fs::write(
        dir.join("DEPS_DELTA.json"),
        serde_json::to_string_pretty(&delta)?,
    )?;

    Ok(delta)
}

/// Extract dependency names from a full manifest file content.
/// Delegates to `extract_deps_versioned` and discards version info.
fn extract_deps_from_manifest(content: &str, filename: &str) -> HashSet<String> {
    extract_deps_versioned(content, filename)
        .into_keys()
        .collect()
}

/// Extract dependency names with version specs for change detection.
fn extract_deps_versioned(content: &str, filename: &str) -> HashMap<String, String> {
    let mut deps = HashMap::new();
    match filename {
        "Cargo.toml" => {
            if let Some(parsed) = parse_toml_manifest(content) {
                for &section in &["dependencies", "dev-dependencies", "build-dependencies"] {
                    if let Some(table) = parsed.get(section).and_then(|v| v.as_table()) {
                        for (key, val) in table {
                            deps.insert(key.clone(), val.to_string());
                        }
                    }
                }
            }
        }
        "package.json" => {
            if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(content) {
                for section in &[
                    "dependencies",
                    "devDependencies",
                    "peerDependencies",
                    "optionalDependencies",
                ] {
                    if let Some(obj) = parsed.get(section).and_then(|v| v.as_object()) {
                        for (key, val) in obj {
                            deps.insert(key.clone(), val.to_string());
                        }
                    }
                }
            }
        }
        "pyproject.toml" => {
            if let Some(parsed) = parse_toml_manifest(content) {
                if let Some(project) = parsed.get("project") {
                    if let Some(dep_list) = project.get("dependencies").and_then(|v| v.as_array()) {
                        for dep in dep_list {
                            if let Some(s) = dep.as_str()
                                && let Some(name) = pep621_dependency_name(s)
                            {
                                deps.insert(name, s.to_string());
                            }
                        }
                    }

                    if let Some(optional) = project
                        .get("optional-dependencies")
                        .and_then(|v| v.as_table())
                    {
                        for group in optional.values() {
                            if let Some(dep_list) = group.as_array() {
                                for dep in dep_list {
                                    if let Some(s) = dep.as_str()
                                        && let Some(name) = pep621_dependency_name(s)
                                    {
                                        deps.insert(name, s.to_string());
                                    }
                                }
                            }
                        }
                    }
                }

                // Poetry: [tool.poetry.dependencies] / [tool.poetry.group.*.dependencies]
                if let Some(poetry) = parsed.get("tool").and_then(|v| v.get("poetry")) {
                    if let Some(table) = poetry.get("dependencies").and_then(|v| v.as_table()) {
                        for (key, val) in table {
                            if key != "python" {
                                deps.insert(key.clone(), val.to_string());
                            }
                        }
                    }

                    if let Some(groups) = poetry.get("group").and_then(|v| v.as_table()) {
                        for group in groups.values() {
                            if let Some(table) =
                                group.get("dependencies").and_then(|v| v.as_table())
                            {
                                for (key, val) in table {
                                    if key != "python" {
                                        deps.insert(key.clone(), val.to_string());
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        _ => {}
    }
    deps
}

fn pep621_dependency_name(requirement: &str) -> Option<String> {
    let name: String = requirement
        .chars()
        .take_while(|c| c.is_alphanumeric() || *c == '-' || *c == '_' || *c == '.')
        .collect();

    (!name.is_empty()).then_some(name)
}

fn parse_toml_manifest(content: &str) -> Option<toml::Table> {
    toml::from_str(content).ok()
}

#[cfg(test)]
mod tests {
    use super::super::test_helpers::{make_diff_with_ids, make_test_repo, mock_file_change};
    use super::*;
    use crate::git::FileStatus;
    use tempfile::TempDir;

    #[test]
    fn deps_delta_cargo_added_removed() {
        let old_cargo =
            "[package]\nname = \"test\"\n\n[dependencies]\nserde = \"1.0\"\nlog = \"0.4\"\n";
        let new_cargo =
            "[package]\nname = \"test\"\n\n[dependencies]\nserde = \"1.0\"\nanyhow = \"1.0\"\n";
        let (_tmp, repo, base_id, target_id) =
            make_test_repo(&[("Cargo.toml", old_cargo, new_cargo)]);
        let out = TempDir::new().unwrap();
        let diff = make_diff_with_ids(
            base_id,
            target_id,
            vec![mock_file_change("Cargo.toml", FileStatus::Modified, 1, 1)],
        );

        generate_deps_delta(out.path(), &[diff], &repo).unwrap();

        let delta = fs::read_to_string(out.path().join("DEPS_DELTA.json")).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&delta).unwrap();
        let added = parsed["added"].as_array().unwrap();
        let removed = parsed["removed"].as_array().unwrap();
        assert!(
            added.iter().any(|d| d == "anyhow"),
            "Should detect added dep 'anyhow'"
        );
        assert!(
            removed.iter().any(|d| d == "log"),
            "Should detect removed dep 'log'"
        );
    }

    #[test]
    fn deps_delta_empty_diff() {
        let (_tmp, repo, _base_id, _target_id) = make_test_repo(&[("README.md", "old\n", "new\n")]);
        let out = TempDir::new().unwrap();

        generate_deps_delta(out.path(), &[], &repo).unwrap();

        assert!(
            !out.path().join("DEPS_DELTA.json").exists(),
            "Should not create file when no manifest files in diff"
        );
    }

    #[test]
    fn manifest_parser_cargo_extracts_deps() {
        let content = "[package]\nname = \"test\"\nversion = \"0.1.0\"\n\n[dependencies]\nserde = \"1.0\"\nlog = \"0.4\"\n\n[dev-dependencies]\nassert_cmd = \"2\"\n";
        let deps = extract_deps_from_manifest(content, "Cargo.toml");
        assert!(deps.contains("serde"));
        assert!(deps.contains("log"));
        assert!(deps.contains("assert_cmd"));
        assert!(!deps.contains("test"), "package name should not be a dep");
    }

    #[test]
    fn manifest_parser_cargo_ignores_non_deps() {
        let content =
            "[package]\nname = \"myapp\"\nversion = \"0.2.0\"\n\n[profile.release]\nlto = true\n";
        let deps = extract_deps_from_manifest(content, "Cargo.toml");
        assert!(deps.is_empty(), "no deps sections means no deps");
    }

    #[test]
    fn manifest_parser_package_json_deps_only() {
        let content = r#"{"name":"myapp","version":"1.0","scripts":{"build":"tsc"},"dependencies":{"react":"^18"},"devDependencies":{"typescript":"^5"}}"#;
        let deps = extract_deps_from_manifest(content, "package.json");
        assert!(deps.contains("react"));
        assert!(deps.contains("typescript"));
        assert!(!deps.contains("myapp"), "name field not a dep");
        assert!(!deps.contains("build"), "scripts not deps");
    }

    #[test]
    fn manifest_parser_package_json_no_deps_section() {
        let content = r#"{"name":"myapp","version":"1.0","scripts":{"test":"jest"}}"#;
        let deps = extract_deps_from_manifest(content, "package.json");
        assert!(deps.is_empty());
    }

    #[test]
    fn manifest_parser_pyproject_pep621() {
        let content = "[project]\nname = \"myapp\"\ndependencies = [\"requests>=2.28\", \"click>=8.0\"]\n\n[project.optional-dependencies]\ndev = [\"pytest>=7\"]\n";
        let deps = extract_deps_from_manifest(content, "pyproject.toml");
        assert!(deps.contains("requests"));
        assert!(deps.contains("click"));
        assert!(deps.contains("pytest"));
        assert!(!deps.contains("myapp"));
    }

    #[test]
    fn manifest_parser_pyproject_poetry() {
        let content = "[tool.poetry]\nname = \"myapp\"\n\n[tool.poetry.dependencies]\npython = \"^3.11\"\nfastapi = \"^0.100\"\n\n[tool.poetry.group.dev.dependencies]\npytest = \"^7\"\n";
        let deps = extract_deps_from_manifest(content, "pyproject.toml");
        assert!(deps.contains("fastapi"));
        assert!(deps.contains("pytest"));
        assert!(!deps.contains("python"), "python itself excluded");
    }

    #[test]
    fn deps_delta_version_change() {
        let old_cargo = "[package]\nname = \"test\"\n\n[dependencies]\nserde = \"1.0\"\n";
        let new_cargo = "[package]\nname = \"test\"\n\n[dependencies]\nserde = \"2.0\"\n";
        let (_tmp, repo, base_id, target_id) =
            make_test_repo(&[("Cargo.toml", old_cargo, new_cargo)]);
        let out = TempDir::new().unwrap();
        let diff = make_diff_with_ids(
            base_id,
            target_id,
            vec![mock_file_change("Cargo.toml", FileStatus::Modified, 1, 1)],
        );

        generate_deps_delta(out.path(), &[diff], &repo).unwrap();

        let delta = fs::read_to_string(out.path().join("DEPS_DELTA.json")).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&delta).unwrap();
        let changed = parsed["changed"].as_array().unwrap();
        assert!(
            changed.iter().any(|d| d == "serde"),
            "Should detect version-changed dep 'serde'"
        );
        assert!(
            parsed["added"].as_array().unwrap().is_empty(),
            "Should not report version change as added"
        );
        assert!(
            parsed["removed"].as_array().unwrap().is_empty(),
            "Should not report version change as removed"
        );
    }

    #[test]
    fn deps_delta_pyproject_pep621_added_removed() {
        let old = "[project]\ndependencies = [\"requests>=2.28\", \"click>=8.0\"]\n";
        let new = "[project]\ndependencies = [\"requests>=2.28\", \"httpx>=0.24\"]\n";
        let (_tmp, repo, base_id, target_id) = make_test_repo(&[("pyproject.toml", old, new)]);
        let out = TempDir::new().unwrap();
        let diff = make_diff_with_ids(
            base_id,
            target_id,
            vec![mock_file_change(
                "pyproject.toml",
                FileStatus::Modified,
                1,
                1,
            )],
        );

        generate_deps_delta(out.path(), &[diff], &repo).unwrap();

        let delta = fs::read_to_string(out.path().join("DEPS_DELTA.json")).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&delta).unwrap();
        assert!(
            parsed["added"]
                .as_array()
                .unwrap()
                .iter()
                .any(|d| d == "httpx")
        );
        assert!(
            parsed["removed"]
                .as_array()
                .unwrap()
                .iter()
                .any(|d| d == "click")
        );
    }

    #[test]
    fn deps_delta_pyproject_optional_deps_version_change() {
        let old = "\
[project]
dependencies = [\"requests>=2.28\"]

[project.optional-dependencies]
dev = [\"pytest>=7.0\"]
";
        let new = "\
[project]
dependencies = [\"requests>=2.28\"]

[project.optional-dependencies]
dev = [\"pytest>=8.0\"]
";
        let (_tmp, repo, base_id, target_id) = make_test_repo(&[("pyproject.toml", old, new)]);
        let out = TempDir::new().unwrap();
        let diff = make_diff_with_ids(
            base_id,
            target_id,
            vec![mock_file_change(
                "pyproject.toml",
                FileStatus::Modified,
                1,
                1,
            )],
        );

        generate_deps_delta(out.path(), &[diff], &repo).unwrap();

        let delta = fs::read_to_string(out.path().join("DEPS_DELTA.json")).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&delta).unwrap();
        assert!(
            parsed["changed"]
                .as_array()
                .unwrap()
                .iter()
                .any(|d| d == "pytest"),
            "Should detect version change in optional-dependencies"
        );
    }

    #[test]
    fn deps_delta_poetry_added() {
        let old = "\
[tool.poetry.dependencies]
python = \"^3.11\"
requests = \"^2.28\"
";
        let new = "\
[tool.poetry.dependencies]
python = \"^3.11\"
requests = \"^2.28\"
httpx = \"^0.24\"
";
        let (_tmp, repo, base_id, target_id) = make_test_repo(&[("pyproject.toml", old, new)]);
        let out = TempDir::new().unwrap();
        let diff = make_diff_with_ids(
            base_id,
            target_id,
            vec![mock_file_change(
                "pyproject.toml",
                FileStatus::Modified,
                1,
                0,
            )],
        );

        generate_deps_delta(out.path(), &[diff], &repo).unwrap();

        let delta = fs::read_to_string(out.path().join("DEPS_DELTA.json")).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&delta).unwrap();
        assert!(
            parsed["added"]
                .as_array()
                .unwrap()
                .iter()
                .any(|d| d == "httpx"),
            "Should detect added Poetry dependency"
        );
        // python should not appear as dep
        assert!(
            !parsed["added"]
                .as_array()
                .unwrap()
                .iter()
                .any(|d| d == "python"),
            "python should be excluded from Poetry deps"
        );
    }

    #[test]
    fn deps_delta_poetry_version_change() {
        let old = "\
[tool.poetry.dependencies]
python = \"^3.11\"
httpx = \"^0.24\"
";
        let new = "\
[tool.poetry.dependencies]
python = \"^3.11\"
httpx = \"^0.25\"
";
        let (_tmp, repo, base_id, target_id) = make_test_repo(&[("pyproject.toml", old, new)]);
        let out = TempDir::new().unwrap();
        let diff = make_diff_with_ids(
            base_id,
            target_id,
            vec![mock_file_change(
                "pyproject.toml",
                FileStatus::Modified,
                1,
                1,
            )],
        );

        generate_deps_delta(out.path(), &[diff], &repo).unwrap();

        let delta = fs::read_to_string(out.path().join("DEPS_DELTA.json")).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&delta).unwrap();
        assert!(
            parsed["changed"]
                .as_array()
                .unwrap()
                .iter()
                .any(|d| d == "httpx"),
            "Should detect Poetry version change"
        );
        assert!(
            !parsed["changed"]
                .as_array()
                .unwrap()
                .iter()
                .any(|d| d == "python"),
            "python should not appear in changed"
        );
    }

    #[test]
    fn deps_delta_output_is_sorted() {
        let old_cargo = "[dependencies]\n";
        let new_cargo = "[dependencies]\nzlib = \"1.0\"\nalpha = \"1.0\"\nmiddle = \"1.0\"\n";
        let (_tmp, repo, base_id, target_id) =
            make_test_repo(&[("Cargo.toml", old_cargo, new_cargo)]);
        let out = TempDir::new().unwrap();
        let diff = make_diff_with_ids(
            base_id,
            target_id,
            vec![mock_file_change("Cargo.toml", FileStatus::Modified, 3, 0)],
        );

        generate_deps_delta(out.path(), &[diff], &repo).unwrap();

        let delta = fs::read_to_string(out.path().join("DEPS_DELTA.json")).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&delta).unwrap();
        let added: Vec<&str> = parsed["added"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(
            added,
            vec!["alpha", "middle", "zlib"],
            "Output must be sorted"
        );
    }

    #[test]
    fn deps_delta_non_dep_section_ignored() {
        let old_cargo = "[package]\nname = \"old-name\"\nversion = \"0.1.0\"\n";
        let new_cargo = "[package]\nname = \"new-name\"\nversion = \"0.2.0\"\n";
        let (_tmp, repo, base_id, target_id) =
            make_test_repo(&[("Cargo.toml", old_cargo, new_cargo)]);
        let out = TempDir::new().unwrap();
        let diff = make_diff_with_ids(
            base_id,
            target_id,
            vec![mock_file_change("Cargo.toml", FileStatus::Modified, 2, 2)],
        );

        generate_deps_delta(out.path(), &[diff], &repo).unwrap();

        assert!(
            !out.path().join("DEPS_DELTA.json").exists(),
            "Should not create file for changes outside [dependencies] section"
        );
    }

    #[test]
    fn deps_delta_pyproject_version_change_tracks_poetry_and_optional_dependencies() {
        let old_pyproject = "[project]\nname = \"myapp\"\ndependencies = [\"requests>=2.28\"]\n\n[project.optional-dependencies]\ndev = [\"pytest>=7\"]\n\n[tool.poetry.dependencies]\npython = \"^3.11\"\nfastapi = \"^0.100\"\n\n[tool.poetry.group.docs.dependencies]\nmkdocs = \"^1.5\"\n";
        let new_pyproject = "[project]\nname = \"myapp\"\ndependencies = [\"requests>=2.31\"]\n\n[project.optional-dependencies]\ndev = [\"pytest>=8\"]\n\n[tool.poetry.dependencies]\npython = \"^3.11\"\nfastapi = \"^0.111\"\n\n[tool.poetry.group.docs.dependencies]\nmkdocs = \"^1.6\"\n";
        let (_tmp, repo, base_id, target_id) =
            make_test_repo(&[("pyproject.toml", old_pyproject, new_pyproject)]);
        let out = TempDir::new().unwrap();
        let diff = make_diff_with_ids(
            base_id,
            target_id,
            vec![mock_file_change(
                "pyproject.toml",
                FileStatus::Modified,
                4,
                4,
            )],
        );

        generate_deps_delta(out.path(), &[diff], &repo).unwrap();

        let delta = fs::read_to_string(out.path().join("DEPS_DELTA.json")).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&delta).unwrap();
        let changed = parsed["changed"].as_array().unwrap();

        assert!(changed.iter().any(|d| d == "requests"));
        assert!(changed.iter().any(|d| d == "pytest"));
        assert!(changed.iter().any(|d| d == "fastapi"));
        assert!(changed.iter().any(|d| d == "mkdocs"));
    }
}
