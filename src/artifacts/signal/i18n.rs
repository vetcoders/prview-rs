//! i18n parity check — compare keys across locale directories.

use crate::git::Diff;
use std::collections::HashSet;
use std::path::Path;

/// i18n parity analysis result (B2).
#[derive(Debug, Clone)]
pub struct I18nDelta {
    /// Missing keys per locale: (key_path, Vec<missing_locales>)
    pub missing_keys: Vec<(String, Vec<String>)>,
    /// Per-locale key count: (locale_name, key_count)
    pub key_counts: Vec<(String, usize)>,
    /// Total locales analyzed
    pub locale_count: usize,
}

/// Compute i18n parity by analyzing HEAD versions of locale JSON files (B2).
/// Compares keys across sibling locale directories.
/// Uses simple approach: analyze current files on disk, not git base versions.
pub fn compute_i18n_delta(diffs: &[Diff], repo_root: &Path) -> Option<I18nDelta> {
    // Find changed i18n files
    let i18n_files: Vec<&str> = diffs
        .iter()
        .flat_map(|d| &d.files)
        .filter(|f| {
            let lower = f.path.to_lowercase();
            (lower.contains("/locales/")
                || lower.contains("/i18n/")
                || lower.contains("/translations/"))
                && lower.ends_with(".json")
        })
        .map(|f| f.path.as_str())
        .collect();

    if i18n_files.is_empty() {
        return None;
    }

    // Discover the i18n root directory pattern from changed files
    // e.g. from "src/locales/en/common.json" extract "src/locales" as base
    // and find all sibling locale dirs
    let mut locale_bases: HashSet<String> = HashSet::new();
    for path in &i18n_files {
        // Find the locale segment pattern: .../locales/<locale>/... or .../i18n/<locale>/...
        let lower = path.to_lowercase();
        for marker in &["locales/", "i18n/", "translations/"] {
            if let Some(idx) = lower.find(marker) {
                // Use original-case path for filesystem join (case-sensitive FS compat)
                // Safe: marker is ASCII so byte offset is preserved across to_lowercase
                let base = &path[..idx + marker.len()];
                locale_bases.insert(base.to_string());
            }
        }
    }

    if locale_bases.is_empty() {
        return None;
    }

    // For each locale base, discover locale directories and flatten keys
    let mut all_locale_keys: std::collections::BTreeMap<
        String,
        std::collections::BTreeMap<String, HashSet<String>>,
    > = std::collections::BTreeMap::new();
    // locale_base -> locale_name -> set of flattened keys

    for base in &locale_bases {
        let base_path = repo_root.join(base);
        let entries = match std::fs::read_dir(&base_path) {
            Ok(e) => e,
            Err(_) => continue,
        };

        for entry in entries.flatten() {
            let entry_path = entry.path();
            if !entry_path.is_dir() {
                continue;
            }
            let locale_name = entry.file_name().to_string_lossy().to_string();

            // Read all .json files in this locale dir
            let json_files = match std::fs::read_dir(&entry_path) {
                Ok(e) => e,
                Err(_) => continue,
            };

            let mut keys = HashSet::new();
            for jf in json_files.flatten() {
                let jp = jf.path();
                if jp.extension().is_some_and(|e| e == "json")
                    && let Ok(content) = std::fs::read_to_string(&jp)
                    && let Ok(val) = serde_json::from_str::<serde_json::Value>(&content)
                {
                    let ns = jp.file_stem().and_then(|s| s.to_str()).unwrap_or("default");
                    flatten_json_keys(&val, &format!("{}:", ns), &mut keys);
                }
            }

            all_locale_keys
                .entry(base.clone())
                .or_default()
                .insert(locale_name, keys);
        }
    }

    if all_locale_keys.is_empty() {
        return None;
    }

    // Compute the union of all keys across all locales (per base)
    let mut missing_keys: Vec<(String, Vec<String>)> = Vec::new();
    let mut key_counts: Vec<(String, usize)> = Vec::new();

    for locales in all_locale_keys.values() {
        // Union of all keys
        let mut all_keys: HashSet<&str> = HashSet::new();
        for keys in locales.values() {
            for k in keys {
                all_keys.insert(k.as_str());
            }
        }

        // Per-locale key count
        for (locale, keys) in locales {
            key_counts.push((locale.clone(), keys.len()));
        }

        // Find missing keys per key
        let locale_names: Vec<&str> = locales.keys().map(|s| s.as_str()).collect();
        for key in &all_keys {
            let missing: Vec<String> = locale_names
                .iter()
                .filter(|locale| {
                    !locales
                        .get(**locale)
                        .is_some_and(|keys| keys.contains(*key))
                })
                .map(|s| s.to_string())
                .collect();
            if !missing.is_empty() {
                missing_keys.push((key.to_string(), missing));
            }
        }
    }

    missing_keys.sort_by(|a, b| a.0.cmp(&b.0));
    key_counts.sort_by(|a, b| a.0.cmp(&b.0));

    let locale_count = key_counts.len();

    Some(I18nDelta {
        missing_keys,
        key_counts,
        locale_count,
    })
}

/// Flatten nested JSON object keys with dot notation.
fn flatten_json_keys(value: &serde_json::Value, prefix: &str, keys: &mut HashSet<String>) {
    if let serde_json::Value::Object(map) = value {
        for (k, v) in map {
            let full_key = format!("{}{}", prefix, k);
            if v.is_object() {
                flatten_json_keys(v, &format!("{}.", full_key), keys);
            } else {
                keys.insert(full_key);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_helpers::{mock_diff, mock_file_change};
    use super::*;
    use crate::git::FileStatus;
    use serde_json::json;
    use tempfile::TempDir;

    // ── flatten_json_keys ──────────────────────────────────────────

    #[test]
    fn flatten_empty_object() {
        let val = json!({});
        let mut keys = HashSet::new();
        flatten_json_keys(&val, "ns:", &mut keys);
        assert!(keys.is_empty());
    }

    #[test]
    fn flatten_flat_keys() {
        let val = json!({"greeting": "hello", "farewell": "bye"});
        let mut keys = HashSet::new();
        flatten_json_keys(&val, "common:", &mut keys);
        assert!(keys.contains("common:greeting"));
        assert!(keys.contains("common:farewell"));
        assert_eq!(keys.len(), 2);
    }

    #[test]
    fn flatten_nested_keys() {
        let val = json!({"nav": {"home": "Home", "about": "About"}});
        let mut keys = HashSet::new();
        flatten_json_keys(&val, "ui:", &mut keys);
        assert!(keys.contains("ui:nav.home"));
        assert!(keys.contains("ui:nav.about"));
        assert_eq!(keys.len(), 2);
    }

    #[test]
    fn flatten_deeply_nested() {
        let val = json!({"a": {"b": {"c": {"d": "leaf"}}}});
        let mut keys = HashSet::new();
        flatten_json_keys(&val, "ns:", &mut keys);
        assert_eq!(keys.len(), 1);
        assert!(keys.contains("ns:a.b.c.d"));
    }

    #[test]
    fn flatten_mixed_depth() {
        let val = json!({"top": "val", "nested": {"inner": "val2"}});
        let mut keys = HashSet::new();
        flatten_json_keys(&val, ":", &mut keys);
        assert!(keys.contains(":top"));
        assert!(keys.contains(":nested.inner"));
        assert_eq!(keys.len(), 2);
    }

    #[test]
    fn flatten_non_object_is_noop() {
        let val = json!("just a string");
        let mut keys = HashSet::new();
        flatten_json_keys(&val, "ns:", &mut keys);
        assert!(keys.is_empty());
    }

    // ── compute_i18n_delta ─────────────────────────────────────────

    #[test]
    fn no_i18n_files_returns_none() {
        let tmp = TempDir::new().unwrap();
        let diff = mock_diff(vec![mock_file_change(
            "src/main.rs",
            FileStatus::Modified,
            10,
            5,
        )]);
        let result = compute_i18n_delta(&[diff], tmp.path());
        assert!(result.is_none());
    }

    #[test]
    fn empty_diff_returns_none() {
        let tmp = TempDir::new().unwrap();
        let result = compute_i18n_delta(&[], tmp.path());
        assert!(result.is_none());
    }

    #[test]
    fn single_locale_pair_added() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();

        // Create locale dirs with JSON files
        let en_dir = root.join("src/locales/en");
        let pl_dir = root.join("src/locales/pl");
        std::fs::create_dir_all(&en_dir).unwrap();
        std::fs::create_dir_all(&pl_dir).unwrap();

        std::fs::write(
            en_dir.join("common.json"),
            r#"{"hello": "Hello", "bye": "Goodbye"}"#,
        )
        .unwrap();
        std::fs::write(
            pl_dir.join("common.json"),
            r#"{"hello": "Czesc", "bye": "Pa"}"#,
        )
        .unwrap();

        let diff = mock_diff(vec![
            mock_file_change("src/locales/en/common.json", FileStatus::Added, 2, 0),
            mock_file_change("src/locales/pl/common.json", FileStatus::Added, 2, 0),
        ]);

        let result = compute_i18n_delta(&[diff], root);
        let delta = result.expect("should produce delta for i18n files");
        assert_eq!(delta.locale_count, 2);
        assert!(
            delta.missing_keys.is_empty(),
            "identical keys => no orphans"
        );
        // Both locales have 2 keys
        for (_, count) in &delta.key_counts {
            assert_eq!(*count, 2);
        }
    }

    #[test]
    fn key_mismatch_between_locales() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();

        let en_dir = root.join("src/i18n/en");
        let de_dir = root.join("src/i18n/de");
        std::fs::create_dir_all(&en_dir).unwrap();
        std::fs::create_dir_all(&de_dir).unwrap();

        // en has extra key "settings"
        std::fs::write(
            en_dir.join("ui.json"),
            r#"{"home": "Home", "settings": "Settings"}"#,
        )
        .unwrap();
        std::fs::write(de_dir.join("ui.json"), r#"{"home": "Startseite"}"#).unwrap();

        let diff = mock_diff(vec![mock_file_change(
            "src/i18n/en/ui.json",
            FileStatus::Modified,
            1,
            0,
        )]);

        let delta = compute_i18n_delta(&[diff], root).unwrap();
        assert_eq!(delta.locale_count, 2);

        // "settings" should be missing from "de"
        let settings_entry = delta.missing_keys.iter().find(|(k, _)| k == "ui:settings");
        assert!(
            settings_entry.is_some(),
            "should detect missing key 'ui:settings'"
        );
        let (_, missing_locales) = settings_entry.unwrap();
        assert!(missing_locales.contains(&"de".to_string()));

        // "home" should NOT be missing from any locale
        let home_entry = delta.missing_keys.iter().find(|(k, _)| k == "ui:home");
        assert!(
            home_entry.is_none(),
            "shared key should not appear in missing"
        );
    }

    #[test]
    fn identical_locales_zero_orphans() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();

        let en_dir = root.join("app/translations/en");
        let fr_dir = root.join("app/translations/fr");
        let es_dir = root.join("app/translations/es");
        std::fs::create_dir_all(&en_dir).unwrap();
        std::fs::create_dir_all(&fr_dir).unwrap();
        std::fs::create_dir_all(&es_dir).unwrap();

        let content = r#"{"nav": {"home": "x", "about": "y"}, "footer": "z"}"#;
        std::fs::write(en_dir.join("main.json"), content).unwrap();
        std::fs::write(fr_dir.join("main.json"), content).unwrap();
        std::fs::write(es_dir.join("main.json"), content).unwrap();

        let diff = mock_diff(vec![mock_file_change(
            "app/translations/en/main.json",
            FileStatus::Modified,
            1,
            1,
        )]);

        let delta = compute_i18n_delta(&[diff], root).unwrap();
        assert_eq!(delta.locale_count, 3);
        assert!(delta.missing_keys.is_empty(), "all locales have same keys");
        // Each locale should have 3 keys: main:nav.home, main:nav.about, main:footer
        for (_, count) in &delta.key_counts {
            assert_eq!(*count, 3);
        }
    }

    #[test]
    fn locale_dir_not_found_returns_none() {
        let tmp = TempDir::new().unwrap();
        // Diff references i18n files but directory doesn't exist on disk
        let diff = mock_diff(vec![mock_file_change(
            "src/locales/en/common.json",
            FileStatus::Added,
            5,
            0,
        )]);
        let result = compute_i18n_delta(&[diff], tmp.path());
        assert!(result.is_none(), "missing locale dir on disk => None");
    }
}
