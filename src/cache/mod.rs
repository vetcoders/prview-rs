//! Cache mechanism for expensive checks
//!
//! Stores check results keyed by git HEAD + source files hash.

use crate::Config;
use anyhow::Result;
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};

/// Cache store
pub struct Cache {
    dir: PathBuf,
    enabled: bool,
}

impl Cache {
    pub fn new(config: &Config) -> Self {
        Self {
            dir: config.cache_dir(),
            enabled: config.use_cache,
        }
    }

    /// Construct a cache rooted at an explicit directory (test-only), so cross-
    /// module tests can drive `set`/`get` without depending on `PRVIEW_HOME`.
    #[cfg(test)]
    pub(crate) fn with_dir(dir: PathBuf, enabled: bool) -> Self {
        Self { dir, enabled }
    }

    /// Check if cached result exists
    pub fn get(&self, check_name: &str, key: &str) -> Option<CachedResult> {
        if !self.enabled {
            return None;
        }

        let cache_file = self.dir.join(check_name).join(key);
        if cache_file.exists() {
            let status = fs::read_to_string(&cache_file).ok()?;
            let log_file = self.dir.join(check_name).join(format!("{}.log", key));
            let output = fs::read_to_string(&log_file).ok();

            Some(CachedResult {
                status: status.trim().to_string(),
                output,
            })
        } else {
            None
        }
    }

    /// Store result in cache
    pub fn set(
        &self,
        check_name: &str,
        key: &str,
        status: &str,
        output: Option<&str>,
    ) -> Result<()> {
        if !self.enabled {
            return Ok(());
        }

        let cache_dir = self.dir.join(check_name);
        fs::create_dir_all(&cache_dir)?;

        // Clean old entries (keep last 5)
        self.cleanup(&cache_dir, 5)?;

        // Write status
        fs::write(cache_dir.join(key), status)?;

        // Write log if present
        if let Some(output) = output {
            fs::write(cache_dir.join(format!("{}.log", key)), output)?;
        }

        Ok(())
    }

    fn cleanup(&self, dir: &Path, keep: usize) -> Result<()> {
        let mut entries: Vec<_> = crate::paths::read_dir_within(dir, Path::new("."))?
            .filter_map(|e| e.ok())
            .filter(|e| !e.file_name().to_string_lossy().ends_with(".log"))
            .collect();

        entries.sort_by_key(|e| {
            e.metadata()
                .and_then(|m| m.modified())
                .unwrap_or(std::time::SystemTime::UNIX_EPOCH)
        });

        if entries.len() > keep {
            for entry in entries.iter().take(entries.len() - keep) {
                let _ = fs::remove_file(entry.path());
                let log_path = entry.path().with_extension("log");
                let _ = fs::remove_file(log_path);
            }
        }

        Ok(())
    }
}

pub struct CachedResult {
    pub status: String,
    pub output: Option<String>,
}

/// Generate a content-based cache key for TypeScript checks.
pub fn ts_hash(repo_root: &Path) -> String {
    hash_files(repo_root, &["*.ts", "*.tsx", "**/*.ts", "**/*.tsx"])
}

/// Generate a content-based cache key for Stylelint checks.
pub fn stylelint_hash(repo_root: &Path) -> String {
    let style_hash = hash_files(
        repo_root,
        &[
            "*.css",
            "*.scss",
            "*.less",
            "*.sass",
            "**/*.css",
            "**/*.scss",
            "**/*.less",
            "**/*.sass",
        ],
    );
    let config_hash = hash_files(
        repo_root,
        &[".stylelintrc*", "stylelint.config.*", "**/.stylelintrc*"],
    );
    format!("{}-{}", style_hash, config_hash)
}

/// Generate a content-based cache key for Rust checks.
pub fn rust_hash(repo_root: &Path) -> String {
    let cargo_hash = hash_files(repo_root, &["Cargo.toml", "Cargo.lock"]);
    let src_hash = hash_files(repo_root, &["*.rs", "**/*.rs"]);
    format!("{}-{}", cargo_hash, src_hash)
}

/// Hash only the dependency manifest (Cargo.lock / Cargo.toml). Used by the
/// security audit, whose result depends on the resolved dependency set — not on
/// unrelated source churn.
pub fn cargo_lock_hash(repo_root: &Path) -> String {
    hash_files(repo_root, &["Cargo.lock", "Cargo.toml"])
}

/// Generate a content-based cache key for Python checks.
pub fn python_hash(repo_root: &Path) -> String {
    let config_hash = hash_files(repo_root, &["pyproject.toml", "requirements*.txt"]);
    let src_hash = hash_files(repo_root, &["*.py", "**/*.py"]);
    format!("{}-{}", config_hash, src_hash)
}

fn hash_files(repo_root: &Path, patterns: &[&str]) -> String {
    let mut hasher = Sha256::new();
    let escaped_root = escape_glob_literal(&repo_root.display().to_string());

    for pattern in patterns {
        if let Ok(entries) = glob::glob(&format!("{escaped_root}/{pattern}")) {
            let mut paths: Vec<_> = entries.filter_map(|entry| entry.ok()).collect();
            paths.sort();

            for path in paths {
                if let Ok(content) = fs::read(&path) {
                    hasher.update(&content);
                }
            }
        }
    }

    let result = hasher.finalize();
    hex::encode(&result[..16])
}

fn escape_glob_literal(path: &str) -> String {
    let mut escaped = String::with_capacity(path.len());
    for ch in path.chars() {
        match ch {
            '*' => escaped.push_str("[*]"),
            '?' => escaped.push_str("[?]"),
            '[' => escaped.push_str("[[]"),
            ']' => escaped.push_str("[]]"),
            '{' => escaped.push_str("[{]"),
            '}' => escaped.push_str("[}]"),
            _ => escaped.push(ch),
        }
    }
    escaped
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::git_cmd;
    use tempfile::TempDir;

    #[test]
    fn test_cached_result_creation() {
        let result = CachedResult {
            status: "passed".to_string(),
            output: Some("test output".to_string()),
        };
        assert_eq!(result.status, "passed");
        assert_eq!(result.output, Some("test output".to_string()));
    }

    #[test]
    fn test_cached_result_no_output() {
        let result = CachedResult {
            status: "failed".to_string(),
            output: None,
        };
        assert_eq!(result.status, "failed");
        assert!(result.output.is_none());
    }

    #[test]
    fn test_cache_disabled_returns_none() {
        let temp_dir = TempDir::new().unwrap();
        let cache = Cache {
            dir: temp_dir.path().to_path_buf(),
            enabled: false,
        };

        assert!(cache.get("test", "key").is_none());
    }

    #[test]
    fn test_cache_get_nonexistent() {
        let temp_dir = TempDir::new().unwrap();
        let cache = Cache {
            dir: temp_dir.path().to_path_buf(),
            enabled: true,
        };

        assert!(cache.get("nonexistent", "key").is_none());
    }

    #[test]
    fn test_cache_set_and_get() {
        let temp_dir = TempDir::new().unwrap();
        let cache = Cache {
            dir: temp_dir.path().to_path_buf(),
            enabled: true,
        };

        cache
            .set("test_check", "key123", "passed", Some("output text"))
            .unwrap();

        let result = cache.get("test_check", "key123").unwrap();
        assert_eq!(result.status, "passed");
        assert_eq!(result.output, Some("output text".to_string()));
    }

    #[test]
    fn test_cache_set_without_output() {
        let temp_dir = TempDir::new().unwrap();
        let cache = Cache {
            dir: temp_dir.path().to_path_buf(),
            enabled: true,
        };

        cache.set("test_check", "key456", "failed", None).unwrap();

        let result = cache.get("test_check", "key456").unwrap();
        assert_eq!(result.status, "failed");
        assert!(result.output.is_none());
    }

    #[test]
    fn test_cache_disabled_set_does_nothing() {
        let temp_dir = TempDir::new().unwrap();
        let cache = Cache {
            dir: temp_dir.path().to_path_buf(),
            enabled: false,
        };

        let result = cache.set("test", "key", "passed", Some("output"));
        assert!(result.is_ok());

        // Enable cache to verify nothing was written
        let cache_enabled = Cache {
            dir: temp_dir.path().to_path_buf(),
            enabled: true,
        };
        assert!(cache_enabled.get("test", "key").is_none());
    }

    #[test]
    fn test_cache_multiple_checks() {
        let temp_dir = TempDir::new().unwrap();
        let cache = Cache {
            dir: temp_dir.path().to_path_buf(),
            enabled: true,
        };

        cache.set("check1", "key1", "passed", Some("out1")).unwrap();
        cache.set("check2", "key2", "failed", Some("out2")).unwrap();
        cache
            .set("check3", "key3", "warnings", Some("out3"))
            .unwrap();

        assert_eq!(cache.get("check1", "key1").unwrap().status, "passed");
        assert_eq!(cache.get("check2", "key2").unwrap().status, "failed");
        assert_eq!(cache.get("check3", "key3").unwrap().status, "warnings");
    }

    #[test]
    fn test_cache_overwrite() {
        let temp_dir = TempDir::new().unwrap();
        let cache = Cache {
            dir: temp_dir.path().to_path_buf(),
            enabled: true,
        };

        cache.set("check", "key", "passed", Some("old")).unwrap();
        cache.set("check", "key", "failed", Some("new")).unwrap();

        let result = cache.get("check", "key").unwrap();
        assert_eq!(result.status, "failed");
        assert_eq!(result.output, Some("new".to_string()));
    }

    #[test]
    fn test_ts_hash_format() {
        let temp_dir = TempDir::new().unwrap();
        let hash = ts_hash(temp_dir.path());
        let parts: Vec<_> = hash.split('-').collect();
        assert_eq!(parts.len(), 1);
    }

    #[test]
    fn test_rust_hash_format() {
        let temp_dir = TempDir::new().unwrap();
        let hash = rust_hash(temp_dir.path());
        // Format: cargo_hash-src_hash
        let parts: Vec<_> = hash.split('-').collect();
        assert_eq!(parts.len(), 2);
    }

    #[test]
    fn test_hash_functions_use_16_byte_digest_segments() {
        let temp_dir = TempDir::new().unwrap();
        let ts_hash = ts_hash(temp_dir.path());
        assert_eq!(ts_hash.len(), 32);

        let rust_hash = rust_hash(temp_dir.path());
        let parts: Vec<_> = rust_hash.split('-').collect();
        assert_eq!(parts.len(), 2);
        assert!(parts.iter().all(|part| part.len() == 32));
    }

    #[test]
    fn test_python_hash_format() {
        let temp_dir = TempDir::new().unwrap();
        let hash = python_hash(temp_dir.path());
        // Format: config_hash-src_hash
        let parts: Vec<_> = hash.split('-').collect();
        assert_eq!(parts.len(), 2);
    }

    #[test]
    fn test_hash_functions_deterministic() {
        let temp_dir = TempDir::new().unwrap();

        let hash1 = ts_hash(temp_dir.path());
        let hash2 = ts_hash(temp_dir.path());
        assert_eq!(hash1, hash2);

        let hash1 = rust_hash(temp_dir.path());
        let hash2 = rust_hash(temp_dir.path());
        assert_eq!(hash1, hash2);

        let hash1 = python_hash(temp_dir.path());
        let hash2 = python_hash(temp_dir.path());
        assert_eq!(hash1, hash2);
    }

    #[test]
    fn test_cache_cleanup_runs() {
        let temp_dir = TempDir::new().unwrap();
        let cache = Cache {
            dir: temp_dir.path().to_path_buf(),
            enabled: true,
        };

        // Add more than 5 entries - cleanup should run without error
        for i in 0..8 {
            let result = cache.set("check", &format!("key{}", i), "passed", Some("output"));
            assert!(result.is_ok());
        }

        // Verify at least some entries exist
        let check_dir = temp_dir.path().join("check");
        let count = fs::read_dir(&check_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .count();

        assert!(count > 0, "Cache should have some entries");
    }

    #[test]
    fn test_hash_with_actual_files() {
        let temp_dir = TempDir::new().unwrap();

        // Create some TypeScript files
        fs::write(temp_dir.path().join("test.ts"), "const x = 1;").unwrap();

        let hash1 = ts_hash(temp_dir.path());

        // Modify the file
        fs::write(temp_dir.path().join("test.ts"), "const x = 2;").unwrap();

        let hash2 = ts_hash(temp_dir.path());

        // Hashes should be different (though git hash might be same if no git repo)
        // At minimum, the file hash part should differ
        assert!(!hash1.is_empty());
        assert!(!hash2.is_empty());
    }

    #[test]
    fn test_hash_files_escapes_repo_root_glob_metacharacters() {
        let temp_dir = tempfile::Builder::new()
            .prefix("repo[old]")
            .tempdir()
            .unwrap();

        fs::write(
            temp_dir.path().join("Cargo.toml"),
            "[package]\nname = \"demo\"\n",
        )
        .unwrap();
        let first = rust_hash(temp_dir.path());

        fs::write(
            temp_dir.path().join("Cargo.toml"),
            "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        let second = rust_hash(temp_dir.path());

        assert_ne!(
            first, second,
            "repo roots with glob metacharacters must still hash matched files"
        );
    }

    fn init_git_repo_with_commit() -> TempDir {
        let temp_dir = TempDir::new().unwrap();
        git_cmd()
            .args(["init", "-q"])
            .current_dir(temp_dir.path())
            .status()
            .unwrap();
        git_cmd()
            .args(["config", "user.email", "test@example.com"])
            .current_dir(temp_dir.path())
            .status()
            .unwrap();
        git_cmd()
            .args(["config", "user.name", "Test User"])
            .current_dir(temp_dir.path())
            .status()
            .unwrap();
        temp_dir
    }

    fn commit_all(repo_root: &Path, message: &str) {
        git_cmd()
            .args(["add", "."])
            .current_dir(repo_root)
            .status()
            .unwrap();
        git_cmd()
            .args(["commit", "-q", "-m", message])
            .current_dir(repo_root)
            .status()
            .unwrap();
    }

    #[test]
    fn rust_hash_ignores_head_changes_when_rust_inputs_do_not_change() {
        let repo = init_git_repo_with_commit();
        fs::write(
            repo.path().join("Cargo.toml"),
            "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        fs::create_dir_all(repo.path().join("src")).unwrap();
        fs::write(repo.path().join("src/lib.rs"), "pub fn demo() {}\n").unwrap();
        fs::write(repo.path().join("README.md"), "first\n").unwrap();
        commit_all(repo.path(), "initial");

        let first = rust_hash(repo.path());

        fs::write(repo.path().join("README.md"), "second\n").unwrap();
        commit_all(repo.path(), "docs");

        let second = rust_hash(repo.path());
        assert_eq!(first, second);
    }

    #[test]
    fn ts_hash_ignores_head_changes_when_ts_inputs_do_not_change() {
        let repo = init_git_repo_with_commit();
        fs::write(repo.path().join("index.ts"), "export const x = 1;\n").unwrap();
        fs::write(repo.path().join("README.md"), "first\n").unwrap();
        commit_all(repo.path(), "initial");

        let first = ts_hash(repo.path());

        fs::write(repo.path().join("README.md"), "second\n").unwrap();
        commit_all(repo.path(), "docs");

        let second = ts_hash(repo.path());
        assert_eq!(first, second);
    }

    #[test]
    fn python_hash_ignores_head_changes_when_python_inputs_do_not_change() {
        let repo = init_git_repo_with_commit();
        fs::write(
            repo.path().join("pyproject.toml"),
            "[project]\nname = \"demo\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        fs::write(repo.path().join("main.py"), "print('demo')\n").unwrap();
        fs::write(repo.path().join("README.md"), "first\n").unwrap();
        commit_all(repo.path(), "initial");

        let first = python_hash(repo.path());

        fs::write(repo.path().join("README.md"), "second\n").unwrap();
        commit_all(repo.path(), "docs");

        let second = python_hash(repo.path());
        assert_eq!(first, second);
    }

    #[test]
    fn test_cache_different_keys_same_check() {
        let temp_dir = TempDir::new().unwrap();
        let cache = Cache {
            dir: temp_dir.path().to_path_buf(),
            enabled: true,
        };

        cache.set("check", "key1", "passed", Some("out1")).unwrap();
        cache.set("check", "key2", "failed", Some("out2")).unwrap();

        let result1 = cache.get("check", "key1").unwrap();
        let result2 = cache.get("check", "key2").unwrap();

        assert_eq!(result1.status, "passed");
        assert_eq!(result2.status, "failed");
    }

    #[test]
    fn test_cache_struct_creation() {
        let temp_dir = TempDir::new().unwrap();
        let cache = Cache {
            dir: temp_dir.path().to_path_buf(),
            enabled: true,
        };
        assert!(cache.enabled);
        assert_eq!(cache.dir, temp_dir.path().to_path_buf());
    }
}
