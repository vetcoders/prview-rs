//! Cache mechanism for expensive checks
//!
//! Stores check results keyed by git HEAD + source files hash.

use crate::Config;
use anyhow::Result;
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};

/// Legacy sidecar holding the check's captured output, next to its status entry.
/// Only read now — see [`CacheEntry`].
const LOG_SUFFIX: &str = ".log";

/// Legacy sidecar holding the check's serialized provenance. Only read now, so
/// entries written by an older prview keep replaying instead of failing.
const PROVENANCE_SUFFIX: &str = ".prov.json";

/// Marker for a half-written entry. Named so [`Cache::cleanup`] can tell it from
/// a real entry: counting one as live would evict a good entry in its place.
const TMP_MARKER: &str = ".tmp-";

/// One cache entry — verdict, output and provenance in a SINGLE file.
///
/// They used to be three files written one after another, so two prview
/// processes populating the same key could interleave: one wrote its status
/// while the other overwrote the provenance, leaving a hit that paired a verdict
/// with another execution's command, timestamps and substrate. Provenance exists
/// to prove what produced a result, so a mismatched pair is worse than none.
/// Writing one file and publishing it with a single `rename` makes a torn entry
/// unrepresentable: a reader sees the previous entry whole, or the new one whole.
#[derive(serde::Serialize, serde::Deserialize)]
struct CacheEntry {
    status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    output: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    provenance: Option<String>,
}

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

    /// The on-disk path of one entry (test-only), so a test can age it.
    #[cfg(test)]
    pub(crate) fn entry_path(&self, check_name: &str, key: &str) -> PathBuf {
        self.dir.join(check_name).join(key)
    }

    /// Check if cached result exists
    pub fn get(&self, check_name: &str, key: &str) -> Option<CachedResult> {
        if !self.enabled {
            return None;
        }

        let cache_dir = self.dir.join(check_name);
        let entry_path = cache_dir.join(key);
        let raw = fs::read_to_string(&entry_path).ok()?;
        let age_secs = entry_age_secs(&entry_path);

        // An entry written by this prview is one self-contained JSON document.
        if let Ok(entry) = serde_json::from_str::<CacheEntry>(&raw) {
            return Some(CachedResult {
                status: entry.status.trim().to_string(),
                output: entry.output,
                provenance: entry.provenance,
                age_secs,
            });
        }

        // Legacy layout: a bare status line with the output and provenance in
        // sidecars. Kept readable so an upgrade does not throw away a warm
        // cache; either sidecar may be missing, which replays as unknown rather
        // than a hard failure.
        Some(CachedResult {
            status: raw.trim().to_string(),
            output: fs::read_to_string(sidecar(&cache_dir, key, LOG_SUFFIX)).ok(),
            provenance: fs::read_to_string(sidecar(&cache_dir, key, PROVENANCE_SUFFIX)).ok(),
            age_secs,
        })
    }

    /// Store result in cache.
    ///
    /// `provenance` is an opaque serialized blob the caller round-trips: the
    /// cache stores bytes and never interprets them, so the provenance schema
    /// stays owned by `checks`.
    ///
    /// The verdict, output and provenance are published together by a single
    /// `rename`, so a concurrent prview can never read a verdict paired with
    /// another run's provenance (see [`CacheEntry`]).
    pub fn set(
        &self,
        check_name: &str,
        key: &str,
        status: &str,
        output: Option<&str>,
        provenance: Option<&str>,
    ) -> Result<()> {
        if !self.enabled {
            return Ok(());
        }

        let cache_dir = self.dir.join(check_name);
        fs::create_dir_all(&cache_dir)?;

        // Clean old entries (keep last 5)
        self.cleanup(&cache_dir, 5)?;

        let entry = serde_json::to_string(&CacheEntry {
            status: status.to_string(),
            output: output.map(str::to_string),
            provenance: provenance.map(str::to_string),
        })?;

        // Stage the complete entry beside its destination — same directory, so
        // the rename stays within one filesystem and is therefore atomic — then
        // publish it in one step.
        let staged = cache_dir.join(format!(
            "{key}{TMP_MARKER}{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default()
        ));
        fs::write(&staged, entry)?;
        if let Err(err) = fs::rename(&staged, cache_dir.join(key)) {
            let _ = fs::remove_file(&staged);
            return Err(err.into());
        }

        // Drop the legacy sidecars for this key: the published entry is now the
        // whole truth, and leaving them behind would keep a previous run's
        // output and provenance on disk under a live key.
        let _ = fs::remove_file(sidecar(&cache_dir, key, LOG_SUFFIX));
        let _ = fs::remove_file(sidecar(&cache_dir, key, PROVENANCE_SUFFIX));

        Ok(())
    }

    fn cleanup(&self, dir: &Path, keep: usize) -> Result<()> {
        let mut entries: Vec<_> = crate::paths::read_dir_within(dir, Path::new("."))?
            .filter_map(|e| e.ok())
            .filter(|e| {
                let name = e.file_name().to_string_lossy().to_string();
                // Legacy sidecars and a concurrent writer's staged entry are not
                // entries: counting them would evict live results in their place.
                !name.ends_with(LOG_SUFFIX)
                    && !name.ends_with(PROVENANCE_SUFFIX)
                    && !name.contains(TMP_MARKER)
            })
            .collect();

        entries.sort_by_key(|e| {
            e.metadata()
                .and_then(|m| m.modified())
                .unwrap_or(std::time::SystemTime::UNIX_EPOCH)
        });

        if entries.len() > keep {
            for entry in entries.iter().take(entries.len() - keep) {
                let _ = fs::remove_file(entry.path());
                // Suffix-append, not `with_extension`: a key carrying a dot
                // (`audit-<lock>-2026-08-22` style keys are dot-free today, but
                // nothing enforces it) would otherwise have its own tail
                // replaced and leave the sidecars orphaned.
                let key = entry.file_name().to_string_lossy().to_string();
                let _ = fs::remove_file(sidecar(dir, &key, LOG_SUFFIX));
                let _ = fs::remove_file(sidecar(dir, &key, PROVENANCE_SUFFIX));
            }
        }

        Ok(())
    }
}

fn sidecar(cache_dir: &Path, key: &str, suffix: &str) -> PathBuf {
    cache_dir.join(format!("{key}{suffix}"))
}

pub struct CachedResult {
    pub status: String,
    pub output: Option<String>,
    /// Serialized provenance of the run that populated this entry, verbatim as
    /// the caller stored it. `None` for entries written before the sidecar
    /// existed, or for a check that produced no provenance.
    pub provenance: Option<String>,
    /// How long ago this entry was published, in whole seconds — see
    /// [`entry_age_secs`]. `None` when the age cannot be established.
    pub age_secs: Option<u64>,
}

/// Move an entry's mtime `by` into the past (test-only), so the age a replay
/// reports can be asserted without waiting for wall-clock time to pass.
///
/// Only the timestamp moves — the entry's bytes are exactly what `set` wrote,
/// which is what makes this a test of the real published-at reading rather than
/// of a fixture format.
#[cfg(test)]
pub(crate) fn backdate(path: &Path, by: std::time::Duration) {
    let file = fs::OpenOptions::new()
        .write(true)
        .open(path)
        .expect("cache entry must exist to be aged");
    let modified = file
        .metadata()
        .expect("metadata")
        .modified()
        .expect("mtime");
    file.set_modified(modified - by).expect("set mtime");
}

/// How old the entry file at `path` is, in whole seconds.
///
/// Read from the file's mtime rather than from anything inside it: an entry is
/// published by a single `rename`, so its mtime IS the moment the result became
/// readable, and taking the age this way costs one `stat` and keeps the on-disk
/// format untouched — every entry a previous prview wrote already carries it.
///
/// `None` rather than a guess when the age is unknowable: no metadata to read,
/// a filesystem that does not report mtime, or a timestamp in the future (a
/// clock that moved backwards, a copied tree). A replay of unknown age is a fact
/// a reviewer can act on; a fabricated zero is not.
fn entry_age_secs(path: &Path) -> Option<u64> {
    let modified = fs::metadata(path).and_then(|meta| meta.modified()).ok()?;
    Some(
        std::time::SystemTime::now()
            .duration_since(modified)
            .ok()?
            .as_secs(),
    )
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

/// Encode arbitrary text as ONE file-name-safe cache-key component.
///
/// Cache keys are file names: `Cache::set` writes `<cache_dir>/<check>/<key>`
/// and creates only the check-level directory. A key carrying a path separator
/// therefore names a file in a directory nobody made — the write fails, nothing
/// is ever cached, and the most expensive checks recompute on every run. A colon
/// is legal on unix but illegal on Windows, which would break the same keys
/// there. Hashing sidesteps both without capping how long the source value may
/// be.
pub fn key_token(value: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(value.as_bytes());
    hex::encode(&hasher.finalize()[..8])
}

fn hash_files(repo_root: &Path, patterns: &[&str]) -> String {
    let mut hasher = Sha256::new();
    // Escape glob metacharacters in the repo root so a path like `repo[old]` is
    // matched literally, not parsed as a glob pattern. `glob::Pattern::escape`
    // brackets exactly the chars glob treats as special (`? * [ ]`); braces are
    // literal to this crate (no brace expansion), so no extra handling is needed.
    let escaped_root = glob::Pattern::escape(&repo_root.display().to_string());

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::git_cmd;
    use std::time::Duration;
    use tempfile::TempDir;

    #[test]
    fn test_cached_result_creation() {
        let result = CachedResult {
            status: "passed".to_string(),
            output: Some("test output".to_string()),
            provenance: None,
            age_secs: None,
        };
        assert_eq!(result.status, "passed");
        assert_eq!(result.output, Some("test output".to_string()));
    }

    #[test]
    fn test_cached_result_no_output() {
        let result = CachedResult {
            status: "failed".to_string(),
            output: None,
            provenance: None,
            age_secs: None,
        };
        assert_eq!(result.status, "failed");
        assert!(result.output.is_none());
    }

    #[test]
    fn cache_round_trips_the_provenance_sidecar() {
        let temp_dir = TempDir::new().unwrap();
        let cache = Cache {
            dir: temp_dir.path().to_path_buf(),
            enabled: true,
        };

        cache
            .set(
                "check",
                "key",
                "passed",
                Some("out"),
                Some(r#"{"cwd":"/repo"}"#),
            )
            .unwrap();

        let result = cache.get("check", "key").unwrap();
        assert_eq!(result.provenance.as_deref(), Some(r#"{"cwd":"/repo"}"#));
    }

    #[test]
    fn cache_entry_without_provenance_sidecar_reads_back_as_none() {
        // Backwards compatibility: entries written before the sidecar existed
        // have only the status (+ log) files. Reading them must yield an unknown
        // provenance rather than failing the lookup.
        let temp_dir = TempDir::new().unwrap();
        let cache = Cache {
            dir: temp_dir.path().to_path_buf(),
            enabled: true,
        };

        let legacy_dir = temp_dir.path().join("check");
        fs::create_dir_all(&legacy_dir).unwrap();
        fs::write(legacy_dir.join("legacy-key"), "passed").unwrap();
        fs::write(legacy_dir.join("legacy-key.log"), "out").unwrap();

        let result = cache.get("check", "legacy-key").unwrap();
        assert_eq!(result.status, "passed");
        assert_eq!(result.output.as_deref(), Some("out"));
        assert!(result.provenance.is_none());
    }

    /// A replay is only as good as the answer it replays, and until now the
    /// pack could not say how old that answer was. The age comes from the entry
    /// file's mtime — the moment the single `rename` published it — so no entry
    /// on disk had to change format to carry it.
    #[test]
    fn a_cache_hit_reports_how_old_the_entry_is() {
        let temp_dir = TempDir::new().unwrap();
        let cache = Cache {
            dir: temp_dir.path().to_path_buf(),
            enabled: true,
        };

        cache
            .set("check", "key", "passed", Some("out"), None)
            .unwrap();
        assert_eq!(
            cache.get("check", "key").unwrap().age_secs,
            Some(0),
            "an entry written just now is zero whole seconds old",
        );

        backdate(&cache.entry_path("check", "key"), Duration::from_secs(7200));

        let aged = cache.get("check", "key").unwrap();
        let age = aged.age_secs.expect("an entry on disk has an age");
        assert!(
            (7200..7260).contains(&age),
            "an entry backdated two hours replays as two hours old, got {age}s",
        );
        assert_eq!(
            aged.status, "passed",
            "reading the age must not disturb the entry itself",
        );
        assert_eq!(aged.output.as_deref(), Some("out"));
    }

    /// The legacy layout keeps its status file as the entry, so the same mtime
    /// answers for it — a warm cache from an older prview is not ageless.
    #[test]
    fn a_legacy_cache_hit_also_reports_an_age() {
        let temp_dir = TempDir::new().unwrap();
        let cache = Cache {
            dir: temp_dir.path().to_path_buf(),
            enabled: true,
        };

        let legacy_dir = temp_dir.path().join("check");
        fs::create_dir_all(&legacy_dir).unwrap();
        fs::write(legacy_dir.join("legacy-key"), "passed").unwrap();
        backdate(&legacy_dir.join("legacy-key"), Duration::from_secs(60));

        let age = cache
            .get("check", "legacy-key")
            .unwrap()
            .age_secs
            .expect("a legacy entry has an mtime like any other file");
        assert!((60..120).contains(&age), "got {age}s");
    }

    #[test]
    fn cache_set_without_provenance_drops_a_stale_sidecar() {
        let temp_dir = TempDir::new().unwrap();
        let cache = Cache {
            dir: temp_dir.path().to_path_buf(),
            enabled: true,
        };

        cache
            .set(
                "check",
                "key",
                "passed",
                Some("out"),
                Some(r#"{"cwd":"/old"}"#),
            )
            .unwrap();
        cache
            .set("check", "key", "passed", Some("out"), None)
            .unwrap();

        assert!(cache.get("check", "key").unwrap().provenance.is_none());
    }

    /// The verdict, its output and its provenance must reach disk as ONE
    /// published unit. While they were three independent writes, two prview
    /// processes populating the same key could interleave and leave a hit
    /// pairing one run's verdict with another run's provenance.
    #[test]
    fn cache_entry_is_published_as_one_file() {
        let temp_dir = TempDir::new().unwrap();
        let cache = Cache {
            dir: temp_dir.path().to_path_buf(),
            enabled: true,
        };

        cache
            .set(
                "check",
                "key",
                "passed",
                Some("out"),
                Some(r#"{"cwd":"/repo"}"#),
            )
            .unwrap();

        let dir = temp_dir.path().join("check");
        assert!(
            !dir.join("key.log").exists() && !dir.join("key.prov.json").exists(),
            "the entry must not be spread across separately written sidecars",
        );
        let files: Vec<String> = fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
            .collect();
        assert_eq!(
            files,
            vec!["key".to_string()],
            "one key must leave exactly one file — no staged remnants",
        );

        let entry = cache.get("check", "key").expect("entry");
        assert_eq!(entry.status, "passed");
        assert_eq!(entry.output.as_deref(), Some("out"));
        assert_eq!(entry.provenance.as_deref(), Some(r#"{"cwd":"/repo"}"#));
    }

    /// A warm cache written by an older prview keeps replaying: the bare status
    /// file with its sidecars is still understood.
    #[test]
    fn legacy_sidecar_entries_are_still_readable() {
        let temp_dir = TempDir::new().unwrap();
        let cache = Cache {
            dir: temp_dir.path().to_path_buf(),
            enabled: true,
        };
        let dir = temp_dir.path().join("check");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("key"), "warnings\n").unwrap();
        fs::write(dir.join("key.log"), "legacy out").unwrap();
        fs::write(dir.join("key.prov.json"), r#"{"cwd":"/legacy"}"#).unwrap();

        let entry = cache.get("check", "key").expect("legacy entry");
        assert_eq!(entry.status, "warnings");
        assert_eq!(entry.output.as_deref(), Some("legacy out"));
        assert_eq!(entry.provenance.as_deref(), Some(r#"{"cwd":"/legacy"}"#));

        // Overwriting a legacy entry must not leave its sidecars behind to be
        // paired with the new verdict.
        cache
            .set("check", "key", "passed", Some("fresh"), None)
            .unwrap();
        let entry = cache.get("check", "key").expect("rewritten entry");
        assert_eq!(entry.status, "passed");
        assert_eq!(entry.output.as_deref(), Some("fresh"));
        assert!(
            entry.provenance.is_none(),
            "a run without provenance must never replay the previous run's",
        );
    }

    /// A staged entry from a crashed or concurrent writer is not an entry: it
    /// must neither be served nor counted as one during eviction.
    #[test]
    fn staged_writes_are_not_mistaken_for_entries() {
        let temp_dir = TempDir::new().unwrap();
        let cache = Cache {
            dir: temp_dir.path().to_path_buf(),
            enabled: true,
        };
        let dir = temp_dir.path().join("check");

        for i in 0..5 {
            cache
                .set("check", &format!("key{i}"), "passed", Some("out"), None)
                .unwrap();
        }
        fs::write(dir.join(format!("key0{TMP_MARKER}999-1")), "half written").unwrap();

        // One more entry triggers cleanup; a counted leftover would evict a live
        // entry in its place.
        cache
            .set("check", "key5", "passed", Some("out"), None)
            .unwrap();

        for i in 1..=5 {
            assert!(
                cache.get("check", &format!("key{i}")).is_some(),
                "key{i} must survive cleanup",
            );
        }
    }

    #[test]
    fn cleanup_does_not_count_sidecars_as_cache_entries() {
        let temp_dir = TempDir::new().unwrap();
        let cache = Cache {
            dir: temp_dir.path().to_path_buf(),
            enabled: true,
        };

        // 5 entries, each with a log + provenance sidecar. Counting sidecars as
        // entries would push the total past `keep` and evict live entries.
        for i in 0..5 {
            cache
                .set(
                    "check",
                    &format!("key{i}"),
                    "passed",
                    Some("out"),
                    Some(r#"{"cwd":"/repo"}"#),
                )
                .unwrap();
        }

        for i in 0..5 {
            let entry = cache
                .get("check", &format!("key{i}"))
                .unwrap_or_else(|| panic!("key{i} must survive cleanup"));
            assert_eq!(entry.provenance.as_deref(), Some(r#"{"cwd":"/repo"}"#));
        }
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
            .set("test_check", "key123", "passed", Some("output text"), None)
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

        cache
            .set("test_check", "key456", "failed", None, None)
            .unwrap();

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

        let result = cache.set("test", "key", "passed", Some("output"), None);
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

        cache
            .set("check1", "key1", "passed", Some("out1"), None)
            .unwrap();
        cache
            .set("check2", "key2", "failed", Some("out2"), None)
            .unwrap();
        cache
            .set("check3", "key3", "warnings", Some("out3"), None)
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

        cache
            .set("check", "key", "passed", Some("old"), None)
            .unwrap();
        cache
            .set("check", "key", "failed", Some("new"), None)
            .unwrap();

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
            let result = cache.set(
                "check",
                &format!("key{}", i),
                "passed",
                Some("output"),
                None,
            );
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

        cache
            .set("check", "key1", "passed", Some("out1"), None)
            .unwrap();
        cache
            .set("check", "key2", "failed", Some("out2"), None)
            .unwrap();

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
