use anyhow::{Context, Result, anyhow, bail};
use serde::Serialize;
use std::ffi::OsStr;
use std::fs::{self, File, Metadata, ReadDir};
use std::io::Read;
use std::path::{Component, Path, PathBuf};

// ── Repo-relative path normalization ─────────────────────────────────

/// A path normalized to be relative to the repository root.
#[derive(Debug, Clone, Serialize)]
pub struct NormalizedPath {
    /// Display string — repo-relative or `[external]/...`
    pub display: String,
    /// True when the original path was outside the repo root.
    pub is_external: bool,
}

/// Normalize a path to be relative to the repo root.
///
/// - Already relative paths are returned as-is (cleaned of `.` components).
/// - Absolute paths within the repo are stripped of the repo root prefix.
/// - Paths with `..` components are resolved logically before comparison.
/// - Paths outside the repo get an `[external]/...` prefix.
pub fn normalize_to_repo_relative(path: &str, repo_root: &Path) -> NormalizedPath {
    let p = Path::new(path);

    // Relative paths: clean and return.
    if !p.is_absolute() {
        let cleaned = clean_path(p);
        // After cleaning, check if the path escapes via leading `..`
        if cleaned.starts_with("..") {
            return NormalizedPath {
                display: format!("[external]/{}", cleaned),
                is_external: true,
            };
        }
        return NormalizedPath {
            display: cleaned,
            is_external: false,
        };
    }

    // Absolute path — try to strip repo root.
    // Use canonical form when available, fall back to logical cleaning.
    let root_canonical = repo_root
        .canonicalize()
        .unwrap_or_else(|_| repo_root.to_path_buf());
    let path_canonical = p.canonicalize().unwrap_or_else(|_| p.to_path_buf());

    if let Ok(rel) = path_canonical.strip_prefix(&root_canonical) {
        return NormalizedPath {
            display: rel.to_string_lossy().to_string(),
            is_external: false,
        };
    }

    // Fallback: logical strip without canonicalize (for non-existing paths in tests).
    let root_clean = clean_path_buf(repo_root);
    let path_clean = clean_path_buf(p);
    if let Ok(rel) = path_clean.strip_prefix(&root_clean) {
        return NormalizedPath {
            display: rel.to_string_lossy().to_string(),
            is_external: false,
        };
    }

    // Outside repo.
    let cleaned = clean_path(p);
    let cleaned = cleaned.trim_start_matches(['/', '\\']);
    NormalizedPath {
        display: format!("[external]/{}", cleaned),
        is_external: true,
    }
}

/// Convenience wrapper that returns just the display string.
pub fn normalize_path_display(path: &str, repo_root: &Path) -> String {
    normalize_to_repo_relative(path, repo_root).display
}

/// Logically clean a path by resolving `.` and `..` without filesystem access.
fn clean_path(p: &Path) -> String {
    let buf = clean_path_buf(p);
    let s = buf.to_string_lossy().to_string();
    if s.is_empty() { ".".to_string() } else { s }
}

fn clean_path_buf(p: &Path) -> PathBuf {
    let mut parts: Vec<Component> = Vec::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                if let Some(last) = parts.last()
                    && matches!(last, Component::Normal(_))
                {
                    parts.pop();
                    continue;
                }
                parts.push(c);
            }
            _ => parts.push(c),
        }
    }
    parts.iter().collect()
}

fn canonical_root(root: &Path) -> Result<PathBuf> {
    root.canonicalize()
        .with_context(|| format!("Failed to resolve root path: {}", root.display()))
}

pub fn validate_repo_relative_path(path: &Path) -> Result<&Path> {
    if path.as_os_str().is_empty() {
        bail!("Path cannot be empty");
    }
    if path.is_absolute() {
        bail!(
            "Path must be relative to the repository: {}",
            path.display()
        );
    }

    for component in path.components() {
        match component {
            Component::Normal(_) | Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                bail!(
                    "Path must stay repo-relative without traversal: {}",
                    path.display()
                );
            }
        }
    }

    Ok(path)
}

pub fn validate_repo_relative_str(path: &str) -> Result<&Path> {
    let path = Path::new(path);
    validate_repo_relative_path(path)
}

pub fn resolve_path_within(root: &Path, requested: &Path) -> Result<PathBuf> {
    let root_canon = canonical_root(root)?;
    let requested_abs = if requested.is_absolute() {
        requested.to_path_buf()
    } else {
        root_canon.join(requested)
    };

    let resolved = if requested_abs.exists() {
        requested_abs.canonicalize().with_context(|| {
            format!(
                "Failed to resolve existing path: {}",
                requested_abs.display()
            )
        })?
    } else {
        let parent = requested_abs.parent().ok_or_else(|| {
            anyhow!(
                "Path must have a parent directory: {}",
                requested_abs.display()
            )
        })?;
        let parent_canon = parent
            .canonicalize()
            .with_context(|| format!("Failed to resolve parent path: {}", parent.display()))?;
        let file_name = requested_abs.file_name().ok_or_else(|| {
            anyhow!(
                "Path must end with a file name: {}",
                requested_abs.display()
            )
        })?;
        parent_canon.join(file_name)
    };

    if !resolved.starts_with(&root_canon) {
        bail!(
            "Path escapes root {}: {}",
            root_canon.display(),
            resolved.display()
        );
    }

    Ok(resolved)
}

pub fn resolve_existing_path_within(root: &Path, requested: &Path) -> Result<PathBuf> {
    let resolved = resolve_path_within(root, requested)?;
    if !resolved.is_file() {
        bail!("Expected file path inside root: {}", resolved.display());
    }
    Ok(resolved)
}

pub fn resolve_existing_dir_within(root: &Path, requested: &Path) -> Result<PathBuf> {
    let resolved = resolve_path_within(root, requested)?;
    if !resolved.is_dir() {
        bail!(
            "Expected directory path inside root: {}",
            resolved.display()
        );
    }
    Ok(resolved)
}

pub fn resolve_file_name_within(root: &Path, file_name: &OsStr) -> Result<PathBuf> {
    resolve_path_within(root, Path::new(file_name))
}

pub fn open_file_within(root: &Path, requested: &Path) -> Result<File> {
    let resolved = resolve_existing_path_within(root, requested)?;
    File::open(&resolved).with_context(|| format!("Failed opening file: {}", resolved.display()))
}

pub fn create_file_within(root: &Path, requested: &Path) -> Result<File> {
    let resolved = resolve_path_within(root, requested)?;
    if let Some(parent) = resolved.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("Failed creating parent directory: {}", parent.display()))?;
    }
    File::create(&resolved).with_context(|| format!("Failed creating file: {}", resolved.display()))
}

pub fn metadata_within(root: &Path, requested: &Path) -> Result<Metadata> {
    let resolved = resolve_existing_path_within(root, requested)?;
    fs::metadata(&resolved)
        .with_context(|| format!("Failed reading metadata: {}", resolved.display()))
}

pub fn read_to_string_within(root: &Path, requested: &Path) -> Result<String> {
    let mut file = open_file_within(root, requested)?;
    let mut content = String::new();
    file.read_to_string(&mut content)
        .context("Failed reading UTF-8 file contents")?;
    Ok(content)
}

pub fn read_within(root: &Path, requested: &Path) -> Result<Vec<u8>> {
    let mut file = open_file_within(root, requested)?;
    let mut content = Vec::new();
    file.read_to_end(&mut content)
        .context("Failed reading binary file contents")?;
    Ok(content)
}

pub fn read_dir_within(root: &Path, requested: &Path) -> Result<ReadDir> {
    let resolved = resolve_existing_dir_within(root, requested)?;
    fs::read_dir(&resolved)
        .with_context(|| format!("Failed reading directory: {}", resolved.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn resolve_path_within_rejects_parent_escape() {
        let repo = tempdir().unwrap();
        let err = resolve_path_within(repo.path(), Path::new("../outside.txt")).unwrap_err();
        assert!(err.to_string().contains("escapes root"));
    }

    #[test]
    fn read_to_string_within_reads_repo_local_file() {
        let repo = tempdir().unwrap();
        let target = repo.path().join("nested/file.txt");
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::write(&target, "ok").unwrap();

        let content = read_to_string_within(repo.path(), Path::new("nested/file.txt")).unwrap();
        assert_eq!(content, "ok");
    }

    #[test]
    fn validate_repo_relative_path_rejects_traversal() {
        let err = validate_repo_relative_path(Path::new("../secret.txt")).unwrap_err();
        assert!(err.to_string().contains("repo-relative"));
    }

    // ── normalize_to_repo_relative tests ─────────────────────────────

    #[test]
    fn normalize_absolute_within_repo_becomes_relative() {
        let repo = tempdir().unwrap();
        let abs = format!("{}/src/main.rs", repo.path().display());
        let result = normalize_to_repo_relative(&abs, repo.path());
        assert_eq!(result.display, "src/main.rs");
        assert!(!result.is_external);
    }

    #[test]
    fn normalize_already_relative_unchanged() {
        let repo = tempdir().unwrap();
        let result = normalize_to_repo_relative("src/lib.rs", repo.path());
        assert_eq!(result.display, "src/lib.rs");
        assert!(!result.is_external);
    }

    #[test]
    fn normalize_outside_repo_gets_external_marker() {
        let repo = tempdir().unwrap();
        let result = normalize_to_repo_relative("/usr/local/bin/tool", repo.path());
        assert_eq!(result.display, "[external]/usr/local/bin/tool");
        assert_eq!(
            result.display.strip_prefix("[external]/"),
            Some("usr/local/bin/tool")
        );
        assert!(result.is_external);
    }

    #[test]
    fn normalize_absolute_outside_repo_uses_single_external_separator() {
        let repo = tempdir().unwrap();
        let result =
            normalize_to_repo_relative("/tmp/../definitely-outside-prview-tool", repo.path());
        assert_eq!(result.display, "[external]/definitely-outside-prview-tool");
        assert!(result.is_external);
    }

    #[test]
    fn normalize_relative_with_dotdot_resolves() {
        let repo = tempdir().unwrap();
        let result = normalize_to_repo_relative("src/../src/main.rs", repo.path());
        assert_eq!(result.display, "src/main.rs");
        assert!(!result.is_external);
    }

    #[test]
    fn normalize_relative_escaping_via_dotdot_is_external() {
        let repo = tempdir().unwrap();
        let result = normalize_to_repo_relative("../../etc/passwd", repo.path());
        assert!(result.is_external);
        assert!(result.display.contains("[external]"));
    }

    #[test]
    fn normalize_dot_only_returns_dot() {
        let repo = tempdir().unwrap();
        let result = normalize_to_repo_relative(".", repo.path());
        assert_eq!(result.display, ".");
        assert!(!result.is_external);
    }

    #[test]
    fn normalize_path_display_convenience() {
        let repo = tempdir().unwrap();
        let abs = format!("{}/Cargo.toml", repo.path().display());
        let display = normalize_path_display(&abs, repo.path());
        assert_eq!(display, "Cargo.toml");
    }
}
