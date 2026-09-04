//! Shared types and helpers used across signal modules.

use crate::regression::tests::{
    is_code_file as regression_is_code_file, is_config_like as regression_is_config_like,
    is_test_file,
};
use std::path::Path;

/// Churn threshold above which a file is considered a hotspot.
pub const HOTSPOT_THRESHOLD: usize = 80;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReviewFileCategory {
    Code,
    Test,
    Config,
    Asset,
    I18n,
    NonCode,
}

pub(crate) fn classify_review_file(path: &str) -> ReviewFileCategory {
    let lower = path.to_lowercase();
    let fname = Path::new(&lower)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("");

    // Image assets
    if lower.ends_with(".webp")
        || lower.ends_with(".png")
        || lower.ends_with(".jpg")
        || lower.ends_with(".jpeg")
        || lower.ends_with(".gif")
        || lower.ends_with(".svg")
        || lower.ends_with(".ico")
        || lower.ends_with(".bmp")
        || lower.ends_with(".avif")
    {
        return ReviewFileCategory::Asset;
    }

    // i18n / locale files
    if (lower.contains("/locales/") || lower.contains("/i18n/") || lower.contains("/translations/"))
        && lower.ends_with(".json")
    {
        return ReviewFileCategory::I18n;
    }

    if is_test_file(path) {
        return ReviewFileCategory::Test;
    }

    if regression_is_code_file(path) && !regression_is_config_like(path) {
        return ReviewFileCategory::Code;
    }

    if regression_is_config_like(path)
        || lower.ends_with(".lock")
        || lower == "package-lock.json"
        || lower == "pnpm-lock.yaml"
        || lower == "yarn.lock"
        || lower.ends_with(".yaml")
        || lower.ends_with(".yml")
        || lower.ends_with(".toml")
        || lower.ends_with(".config.js")
        || lower.ends_with(".config.ts")
        || lower.ends_with(".config.mjs")
        || matches!(
            fname,
            "package.json"
                | "tsconfig.json"
                | ".eslintrc.json"
                | ".prettierrc"
                | ".prettierrc.json"
                | ".editorconfig"
                | "babel.config.js"
                | "jest.config.js"
                | "jest.config.ts"
                | "vitest.config.ts"
                | "vite.config.ts"
                | "webpack.config.js"
                | "rollup.config.js"
                | "tailwind.config.js"
                | "postcss.config.js"
                | ".gitignore"
                | ".dockerignore"
                | "dockerfile"
                | "docker-compose.yml"
                | "docker-compose.yaml"
                | "makefile"
                | ".env.example"
        )
    {
        return ReviewFileCategory::Config;
    }

    ReviewFileCategory::NonCode
}

/// Retain only JS/TS file sections from a unified Git patch.
///
/// The legacy API analyzers remain the JavaScript/TypeScript backend in 0.8,
/// but Rust production truth is revision-backed. Filtering before invoking the
/// legacy analyzer makes that boundary structural: Rust patch lines never enter
/// the legacy parser and cannot become a second source of Rust facts.
pub(crate) fn js_ts_patch_sections(patch: &str) -> String {
    let mut output = String::new();
    let mut section = String::new();
    let mut paths = None;

    for line in patch.split_inclusive('\n') {
        if line.starts_with("diff --git ") {
            flush_js_ts_section(&mut output, &section, paths.take());
            section.clear();
            paths = parse_diff_git_paths(line);
        }
        section.push_str(line);
    }
    flush_js_ts_section(&mut output, &section, paths);
    output
}

fn flush_js_ts_section(output: &mut String, section: &str, header_paths: Option<(String, String)>) {
    let Some((header_old, header_new)) = header_paths else {
        return;
    };

    let mut old_marker = None;
    let mut new_marker = None;
    let mut marker_error = false;
    let mut new_file = false;
    let mut deleted_file = false;
    let mut has_hunk_or_change_content = false;
    let mut in_hunk = false;
    for line in section.lines() {
        if let Some(value) = line.strip_prefix("--- ").filter(|_| !in_hunk) {
            if old_marker.is_some() {
                marker_error = true;
            } else {
                old_marker = Some(parse_patch_marker(value, "a/"));
            }
        } else if let Some(value) = line.strip_prefix("+++ ").filter(|_| !in_hunk) {
            if new_marker.is_some() {
                marker_error = true;
            } else {
                new_marker = Some(parse_patch_marker(value, "b/"));
            }
        } else if line.starts_with("new file mode ") {
            new_file = true;
        } else if line.starts_with("deleted file mode ") {
            deleted_file = true;
        } else if line.starts_with("@@") {
            in_hunk = true;
            has_hunk_or_change_content = true;
        } else if (line.starts_with('+') && !line.starts_with("+++"))
            || (line.starts_with('-') && !line.starts_with("---"))
        {
            has_hunk_or_change_content = true;
        }
    }

    if marker_error {
        return;
    }
    let Some((old_path, new_path)) = coherent_patch_paths(
        &header_old,
        &header_new,
        old_marker,
        new_marker,
        new_file,
        deleted_file,
        has_hunk_or_change_content,
    ) else {
        return;
    };

    let old_js = old_path.as_deref().is_some_and(is_js_ts_path);
    let new_js = new_path.as_deref().is_some_and(is_js_ts_path);
    if !old_js && !new_js {
        return;
    }

    // The legacy parsers understand the simple `a/… b/…` header shape. Emit a
    // normalized JS/TS identity even when Git quoted the original path, or when
    // a cross-language rename makes one side non-JS. This prevents either
    // parser from seeing Rust lines while keeping the surviving JS side named.
    let current_path = if new_js {
        new_path.as_deref().expect("new JS path")
    } else {
        old_path.as_deref().expect("old JS path")
    };
    let current_path = legacy_safe_patch_path(current_path);
    output.push_str(&format!("diff --git a/{current_path} b/{current_path}\n"));
    output.push_str(&format!(
        "--- {}\n",
        (if old_js { old_path.as_deref() } else { None })
            .map(|path| format!("a/{}", legacy_safe_patch_path(path)))
            .unwrap_or_else(|| "/dev/null".to_owned())
    ));
    output.push_str(&format!(
        "+++ {}\n",
        (if new_js { new_path.as_deref() } else { None })
            .map(|path| format!("b/{}", legacy_safe_patch_path(path)))
            .unwrap_or_else(|| "/dev/null".to_owned())
    ));

    let mut last_change_kept = false;
    let mut in_hunk = false;
    for line in section.lines() {
        if line.starts_with("@@") {
            in_hunk = true;
            output.push_str(line);
            output.push('\n');
            last_change_kept = false;
        } else if !in_hunk && (line.starts_with("--- ") || line.starts_with("+++ ")) {
            continue;
        } else if line.starts_with('-') {
            last_change_kept = old_js;
            if old_js {
                output.push_str(line);
                output.push('\n');
            }
        } else if line.starts_with('+') {
            last_change_kept = new_js;
            if new_js {
                output.push_str(line);
                output.push('\n');
            }
        } else if line == "\\ No newline at end of file" {
            if last_change_kept {
                output.push_str(line);
                output.push('\n');
            }
        } else if old_js && new_js && (line.starts_with(' ') || line.is_empty()) {
            output.push_str(line);
            output.push('\n');
            last_change_kept = false;
        }
    }
}

fn legacy_safe_patch_path(path: &str) -> String {
    path.replace('\n', "\\n")
        .replace('\r', "\\r")
        .replace('\t', "\\t")
}

fn parse_diff_git_paths(line: &str) -> Option<(String, String)> {
    let mut rest = line.strip_prefix("diff --git ")?.trim_end();
    if !rest.starts_with('"')
        && let Some(index) = rest.rfind(" b/")
    {
        let old = rest[..index].trim_end();
        let new = &rest[index + 1..];
        return Some((
            strip_git_side_prefix(old)?.to_owned(),
            strip_git_side_prefix(new)?.to_owned(),
        ));
    }
    let old = parse_git_path_token(&mut rest)?;
    let new = parse_git_path_token(&mut rest)?;
    if !rest.trim().is_empty() {
        return None;
    }
    Some((
        strip_git_side_prefix(&old)?.to_owned(),
        strip_git_side_prefix(&new)?.to_owned(),
    ))
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PatchMarker {
    Path(String),
    DevNull,
    Invalid,
}

fn parse_patch_marker(value: &str, expected_prefix: &str) -> PatchMarker {
    let value = value.trim_end();
    if value == "/dev/null" {
        return PatchMarker::DevNull;
    }
    let path = if value.starts_with('"') {
        let mut rest = value;
        let Some(path) = parse_git_path_token(&mut rest) else {
            return PatchMarker::Invalid;
        };
        if !rest.is_empty() && !rest.starts_with('\t') {
            return PatchMarker::Invalid;
        }
        path
    } else {
        let Some(path) = value.split('\t').next() else {
            return PatchMarker::Invalid;
        };
        path.to_owned()
    };
    let Some(path) = path.strip_prefix(expected_prefix) else {
        return PatchMarker::Invalid;
    };
    if path.is_empty() {
        return PatchMarker::Invalid;
    }
    PatchMarker::Path(path.to_owned())
}

/// Resolve the section's effective sides without allowing hunk markers to
/// replace the identity established by `diff --git`. Any marker/header
/// disagreement discards the whole section, so malformed input cannot move a
/// Rust deletion into the legacy JS/TS analyzers.
fn coherent_patch_paths(
    header_old: &str,
    header_new: &str,
    old_marker: Option<PatchMarker>,
    new_marker: Option<PatchMarker>,
    new_file: bool,
    deleted_file: bool,
    has_hunk_or_change_content: bool,
) -> Option<(Option<String>, Option<String>)> {
    if new_file && deleted_file {
        return None;
    }

    match (old_marker, new_marker) {
        (None, None) => {
            if has_hunk_or_change_content {
                return None;
            }
            if (new_file || deleted_file) && header_old != header_new {
                return None;
            }
            if new_file {
                Some((None, Some(header_new.to_owned())))
            } else if deleted_file {
                Some((Some(header_old.to_owned()), None))
            } else {
                Some((Some(header_old.to_owned()), Some(header_new.to_owned())))
            }
        }
        (Some(old), Some(new)) => match (old, new) {
            (PatchMarker::Path(old), PatchMarker::Path(new))
                if !new_file && !deleted_file && old == header_old && new == header_new =>
            {
                Some((Some(header_old.to_owned()), Some(header_new.to_owned())))
            }
            (PatchMarker::DevNull, PatchMarker::Path(new))
                if new_file && !deleted_file && header_old == header_new && new == header_new =>
            {
                Some((None, Some(header_new.to_owned())))
            }
            (PatchMarker::Path(old), PatchMarker::DevNull)
                if deleted_file && !new_file && header_old == header_new && old == header_old =>
            {
                Some((Some(header_old.to_owned()), None))
            }
            _ => None,
        },
        _ => None,
    }
}

fn strip_git_side_prefix(path: &str) -> Option<&str> {
    path.strip_prefix("a/").or_else(|| path.strip_prefix("b/"))
}

/// Parse one Git path token, including the C-style quoted form emitted when a
/// path contains whitespace, quotes, backslashes, or non-ASCII bytes.
fn parse_git_path_token(input: &mut &str) -> Option<String> {
    *input = input.trim_start();
    if !input.starts_with('"') {
        let end = input.find(char::is_whitespace).unwrap_or(input.len());
        let token = input[..end].to_owned();
        *input = &input[end..];
        return Some(token);
    }

    let bytes = input.as_bytes();
    let mut decoded = Vec::new();
    let mut index = 1usize;
    while index < bytes.len() {
        match bytes[index] {
            b'"' => {
                *input = &input[index + 1..];
                return Some(String::from_utf8_lossy(&decoded).into_owned());
            }
            b'\\' => {
                index += 1;
                let escaped = *bytes.get(index)?;
                if escaped.is_ascii_digit() && escaped < b'8' {
                    let mut value = 0u16;
                    let mut digits = 0;
                    while digits < 3
                        && index < bytes.len()
                        && bytes[index].is_ascii_digit()
                        && bytes[index] < b'8'
                    {
                        value = value * 8 + u16::from(bytes[index] - b'0');
                        index += 1;
                        digits += 1;
                    }
                    decoded.push(value as u8);
                    continue;
                }
                decoded.push(match escaped {
                    b'a' => 0x07,
                    b'b' => 0x08,
                    b't' => b'\t',
                    b'n' => b'\n',
                    b'v' => 0x0b,
                    b'f' => 0x0c,
                    b'r' => b'\r',
                    other => other,
                });
                index += 1;
            }
            byte => {
                decoded.push(byte);
                index += 1;
            }
        }
    }
    None
}

fn is_js_ts_path(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    [".js", ".jsx", ".ts", ".tsx", ".mjs", ".cjs", ".mts", ".cts"]
        .iter()
        .any(|extension| lower.ends_with(extension))
}

/// Check if a path is a non-code file (assets, i18n, config, docs, scripts, metadata).
pub(crate) fn is_non_code_file(path: &str) -> bool {
    !matches!(
        classify_review_file(path),
        ReviewFileCategory::Code | ReviewFileCategory::Test
    )
}

pub(super) fn parse_patch_new_start(line: &str) -> Option<usize> {
    if !line.starts_with("@@") {
        return None;
    }

    let plus = line.split_whitespace().find(|part| part.starts_with('+'))?;
    let start = plus[1..].split(',').next()?;
    start.parse().ok()
}

pub(super) fn is_identifier_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

/// Identifier-aware token match: boundaries are non-identifier bytes, so `_`
/// counts as part of the token. Correct for matching module/symbol needles in
/// code (coverage import scanning), where `foo_bar_baz` is ONE identifier and
/// must not match the needle "bar".
pub(super) fn contains_token_match(haystack: &str, needle: &str) -> bool {
    haystack.match_indices(needle).any(|(pos, _)| {
        let before_ok = pos == 0 || !is_identifier_byte(haystack.as_bytes()[pos - 1]);
        let after_pos = pos + needle.len();
        let after_ok =
            after_pos >= haystack.len() || !is_identifier_byte(haystack.as_bytes()[after_pos]);
        before_ok && after_ok
    })
}

/// Path-aware token match: boundaries are any non-alphanumeric byte, so `_` and
/// `-` (snake/kebab path separators) split tokens. Correct for matching a
/// keyword against a file PATH — `auth_token.rs` matches "auth", while "author"
/// still does NOT match "auth" (the trailing 'o' is alphanumeric)
/// (PR #12 review #23).
pub(super) fn contains_path_token_match(haystack: &str, needle: &str) -> bool {
    haystack.match_indices(needle).any(|(pos, _)| {
        let before_ok = pos == 0 || !haystack.as_bytes()[pos - 1].is_ascii_alphanumeric();
        let after_pos = pos + needle.len();
        let after_ok =
            after_pos >= haystack.len() || !haystack.as_bytes()[after_pos].is_ascii_alphanumeric();
        before_ok && after_ok
    })
}

/// Rust lexer state carried across the lines of a single contiguous source
/// region (e.g. one diff-hunk side). Tracks open block comments and raw
/// strings so multi-line constructs are lexed correctly. Reset it on every
/// hunk boundary — hunks are not contiguous source.
#[derive(Default)]
pub(crate) struct RustLexState {
    in_block_comment: bool,
    raw_string_hashes: Option<usize>,
}

/// A source line split into its code and comment portions, with string and
/// raw-string (and optionally char) literals removed from `code`.
pub(crate) struct StrippedLine {
    pub code: String,
    pub comment: String,
}

/// Strip Rust comments and string/raw-string literals from a single `line`,
/// carrying multi-line block-comment / raw-string state in `state`.
///
/// When `strip_char_literals` is true, `'...'` char literals are also removed.
/// Enable it for presence checks (e.g. the unsafe audit, which only greps the
/// stripped code for `unsafe {`). Keep it DISABLED for signature parsing (the
/// public-API audit): a `'a` lifetime would otherwise be consumed as an
/// unterminated char literal and corrupt the signature.
pub(crate) fn strip_rust_non_code(
    line: &str,
    state: &mut RustLexState,
    strip_char_literals: bool,
) -> StrippedLine {
    let mut code = String::new();
    let mut comment = String::new();
    let chars: Vec<char> = line.chars().collect();
    let mut i = 0usize;

    while i < chars.len() {
        if let Some(hashes) = state.raw_string_hashes {
            if raw_string_end_matches(&chars, i, hashes) {
                i += 1 + hashes;
                state.raw_string_hashes = None;
            } else {
                i += 1;
            }
            continue;
        }

        if state.in_block_comment {
            if i + 1 < chars.len() && chars[i] == '*' && chars[i + 1] == '/' {
                state.in_block_comment = false;
                comment.push('*');
                comment.push('/');
                i += 2;
            } else {
                comment.push(chars[i]);
                i += 1;
            }
            continue;
        }

        if i + 1 < chars.len() && chars[i] == '/' && chars[i + 1] == '/' {
            comment.extend(chars[i..].iter());
            break;
        }

        if i + 1 < chars.len() && chars[i] == '/' && chars[i + 1] == '*' {
            state.in_block_comment = true;
            comment.push('/');
            comment.push('*');
            i += 2;
            continue;
        }

        if chars[i] == '"' {
            i += 1;
            let mut escaped = false;
            while i < chars.len() {
                let ch = chars[i];
                i += 1;
                if escaped {
                    escaped = false;
                } else if ch == '\\' {
                    escaped = true;
                } else if ch == '"' {
                    break;
                }
            }
            continue;
        }

        if strip_char_literals && chars[i] == '\'' {
            i += 1;
            let mut escaped = false;
            while i < chars.len() {
                let ch = chars[i];
                i += 1;
                if escaped {
                    escaped = false;
                } else if ch == '\\' {
                    escaped = true;
                } else if ch == '\'' {
                    break;
                }
            }
            continue;
        }

        if chars[i] == 'r' {
            let mut j = i + 1;
            while j < chars.len() && chars[j] == '#' {
                j += 1;
            }
            if j < chars.len() && chars[j] == '"' {
                let hashes = j.saturating_sub(i + 1);
                i = j + 1;
                let mut found = false;
                let mut k = i;
                while k < chars.len() {
                    if raw_string_end_matches(&chars, k, hashes) {
                        i = k + 1 + hashes;
                        found = true;
                        break;
                    }
                    k += 1;
                }
                if !found {
                    state.raw_string_hashes = Some(hashes);
                }
                continue;
            }
        }

        code.push(chars[i]);
        i += 1;
    }

    StrippedLine { code, comment }
}

/// Does a raw-string end delimiter (`"` followed by `hashes` `#`) begin at
/// `pos` in `chars`? Allocation-free: replaces the former
/// `chars[pos..].iter().collect::<String>().starts_with(&delim)` per-char probe,
/// which made raw-string scanning O(N²) in allocations over a line.
fn raw_string_end_matches(chars: &[char], pos: usize, hashes: usize) -> bool {
    if pos >= chars.len() || chars[pos] != '"' {
        return false;
    }
    if pos + 1 + hashes > chars.len() {
        return false;
    }
    chars[pos + 1..pos + 1 + hashes].iter().all(|&c| c == '#')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn js_ts_patch_sections_excludes_rust_before_legacy_analysis() {
        let patch = "diff --git a/src/lib.rs b/src/lib.rs\n--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1 +1 @@\n-pub fn old() {}\n+pub fn new() {}\ndiff --git a/src/api.ts b/src/api.ts\n--- a/src/api.ts\n+++ b/src/api.ts\n@@ -1 +0,0 @@\n-export function keptByLegacy() {}\n";
        let filtered = js_ts_patch_sections(patch);
        assert!(!filtered.contains("src/lib.rs"));
        assert!(!filtered.contains("pub fn old"));
        assert!(filtered.contains("src/api.ts"));
        assert!(filtered.contains("keptByLegacy"));
    }

    #[test]
    fn js_ts_patch_sections_is_side_aware_for_cross_language_renames() {
        let rust_to_ts = "diff --git a/src/lib.rs b/src/api.ts\nsimilarity index 61%\nrename from src/lib.rs\nrename to src/api.ts\n--- a/src/lib.rs\n+++ b/src/api.ts\n@@ -1 +1 @@\n-pub fn rust_removed() {}\n+export function js_added() {}\n";
        let filtered = js_ts_patch_sections(rust_to_ts);
        assert!(filtered.contains("src/api.ts"));
        assert!(filtered.contains("js_added"));
        assert!(!filtered.contains("src/lib.rs"));
        assert!(!filtered.contains("rust_removed"));

        let ts_to_rust = "diff --git a/src/api.ts b/src/lib.rs\nsimilarity index 61%\nrename from src/api.ts\nrename to src/lib.rs\n--- a/src/api.ts\n+++ b/src/lib.rs\n@@ -1 +1 @@\n-export function js_removed() {}\n+pub fn rust_added() {}\n";
        let filtered = js_ts_patch_sections(ts_to_rust);
        assert!(filtered.contains("src/api.ts"));
        assert!(filtered.contains("js_removed"));
        assert!(!filtered.contains("src/lib.rs"));
        assert!(!filtered.contains("rust_added"));
    }

    #[test]
    fn js_ts_patch_sections_decodes_quoted_paths_and_preserves_js_renames() {
        let patch = "diff --git \"a/src/quoted\\040old.ts\" \"b/src/quoted\\040new.ts\"\nsimilarity index 70%\nrename from \"src/quoted old.ts\"\nrename to \"src/quoted new.ts\"\n--- \"a/src/quoted\\040old.ts\"\n+++ \"b/src/quoted\\040new.ts\"\n@@ -1,2 +1,2 @@\n export const stable = true;\n-export function old_name() {}\n+export function new_name() {}\n";
        let filtered = js_ts_patch_sections(patch);
        assert!(filtered.contains("src/quoted new.ts"));
        assert!(filtered.contains("old_name"));
        assert!(filtered.contains("new_name"));
        assert!(filtered.contains("export const stable"));
    }

    #[test]
    fn js_ts_patch_sections_handles_dev_null_add_and_delete() {
        let added = "diff --git a/src/new.ts b/src/new.ts\nnew file mode 100644\n--- /dev/null\n+++ b/src/new.ts\n@@ -0,0 +1 @@\n+export const added = 1;\n";
        let deleted = "diff --git a/src/old.ts b/src/old.ts\ndeleted file mode 100644\n--- a/src/old.ts\n+++ /dev/null\n@@ -1 +0,0 @@\n-export const removed = 1;\n";
        assert!(js_ts_patch_sections(added).contains("export const added"));
        assert!(js_ts_patch_sections(deleted).contains("export const removed"));
    }

    #[test]
    fn js_ts_patch_sections_does_not_parse_removed_content_as_old_marker() {
        let patch = "diff --git a/src/api.ts b/src/api.ts\n--- a/src/api.ts\n+++ b/src/api.ts\n@@ -1,2 +1 @@\n--- content collision\n-export function removed() {}\n";
        let filtered = js_ts_patch_sections(patch);
        assert!(filtered.contains("--- content collision"));
        assert!(filtered.contains("removed"));
    }

    #[test]
    fn js_ts_patch_sections_does_not_parse_added_content_as_new_marker() {
        let patch = "diff --git a/src/api.ts b/src/api.ts\n--- a/src/api.ts\n+++ b/src/api.ts\n@@ -1 +1,2 @@\n export const kept = true;\n+++ content collision\n";
        let filtered = js_ts_patch_sections(patch);
        assert!(filtered.contains("+++ content collision"));
        assert!(filtered.contains("src/api.ts"));
    }

    #[test]
    fn js_ts_patch_sections_rejects_marker_header_identity_mismatches() {
        for patch in [
            "diff --git a/src/lib.rs b/src/api.ts\n--- a/src/fake.ts\n+++ b/src/api.ts\n@@ -1 +1 @@\n-pub fn rust_secret() {}\n+export function js_added() {}\n",
            "diff --git a/src/api.ts b/src/lib.rs\n--- a/src/api.ts\n+++ b/src/fake.ts\n@@ -1 +1 @@\n-export function js_secret() {}\n+pub fn rust_added() {}\n",
            "diff --git \"a/src/quoted\\040old.ts\" \"b/src/quoted\\040new.ts\"\n--- \"a/src/other\\040old.ts\"\n+++ \"b/src/quoted\\040new.ts\"\n@@ -1 +1 @@\n-export function old_api() {}\n+export function new_api() {}\n",
        ] {
            assert_eq!(js_ts_patch_sections(patch), "", "{patch}");
        }
    }

    #[test]
    fn js_ts_patch_sections_rejects_malformed_or_truncated_identity_headers() {
        for patch in [
            "diff --git a/src/api.ts\n--- a/src/api.ts\n+++ b/src/api.ts\n@@ -1 +1 @@\n-export const old_api = 1;\n+export const new_api = 1;\n",
            "diff --git \"a/src/api.ts\" \"b/src/api.ts\n--- \"a/src/api.ts\"\n+++ \"b/src/api.ts\"\n@@ -1 +1 @@\n-export const old_api = 1;\n+export const new_api = 1;\n",
            "diff --git a/src/api.ts b/src/api.ts\n--- \"a/src/api.ts\n+++ b/src/api.ts\n@@ -1 +1 @@\n-export const old_api = 1;\n+export const new_api = 1;\n",
            "diff --git a/src/api.ts b/src/api.ts\n--- /dev/null\n+++ b/src/api.ts\n@@ -0,0 +1 @@\n+export const unproven_add = 1;\n",
            "diff --git a/src/api.ts b/src/api.ts\nnew file mode 100644\n--- a/src/api.ts\n+++ b/src/api.ts\n@@ -0,0 +1 @@\n+export const contradictory_add = 1;\n",
            "diff --git a/src/api.ts b/src/api.ts\n--- a/src/api.ts\n@@ -1 +0,0 @@\n-export const partial_markers = 1;\n",
            "diff --git a/src/api.ts b/src/api.ts\n--- a/src/api.ts\n--- a/src/api.ts\n+++ b/src/api.ts\n@@ -1 +0,0 @@\n-export const duplicate_markers = 1;\n",
        ] {
            assert_eq!(js_ts_patch_sections(patch), "", "{patch}");
        }
    }

    #[test]
    fn js_ts_patch_sections_rejects_hunks_without_both_file_markers() {
        let patch = "diff --git a/src/lib.rs b/src/api.ts\nsimilarity index 61%\nrename from src/lib.rs\nrename to src/api.ts\n@@ -1 +1 @@\n-pub fn rust_secret() {}\n+export function js_added() {}\n";
        assert_eq!(js_ts_patch_sections(patch), "");
    }

    #[test]
    fn js_ts_patch_sections_keeps_mode_proven_add_delete_without_hunks() {
        let added =
            "diff --git a/src/new.ts b/src/new.ts\nnew file mode 100644\nindex 0000000..1111111\n";
        let deleted = "diff --git a/src/old.ts b/src/old.ts\ndeleted file mode 100644\nindex 1111111..0000000\n";
        assert!(js_ts_patch_sections(added).contains("+++ b/src/new.ts"));
        assert!(js_ts_patch_sections(added).contains("--- /dev/null"));
        assert!(js_ts_patch_sections(deleted).contains("--- a/src/old.ts"));
        assert!(js_ts_patch_sections(deleted).contains("+++ /dev/null"));
    }

    #[test]
    fn test_classify_rs_file() {
        assert_eq!(
            classify_review_file("src/main.rs"),
            ReviewFileCategory::Code
        );
        assert_eq!(
            classify_review_file("lib/parser.rs"),
            ReviewFileCategory::Code
        );
    }

    #[test]
    fn test_classify_md_file() {
        assert_eq!(
            classify_review_file("README.md"),
            ReviewFileCategory::NonCode
        );
        assert_eq!(
            classify_review_file("docs/guide.md"),
            ReviewFileCategory::NonCode
        );
    }

    #[test]
    fn test_classify_json_i18n_file() {
        assert_eq!(
            classify_review_file("src/locales/en.json"),
            ReviewFileCategory::I18n
        );
        assert_eq!(
            classify_review_file("assets/i18n/fr.json"),
            ReviewFileCategory::I18n
        );
        assert_eq!(
            classify_review_file("translations/de.json"),
            ReviewFileCategory::NonCode, // no leading /translations/ segment
        );
        assert_eq!(
            classify_review_file("src/translations/de.json"),
            ReviewFileCategory::I18n
        );
    }

    #[test]
    fn test_is_non_code_file() {
        // Code and Test should return false
        assert!(!is_non_code_file("src/lib.rs"));
        // NonCode should return true
        assert!(is_non_code_file("README.md"));
        // Config should return true
        assert!(is_non_code_file("package.json"));
        // Asset should return true
        assert!(is_non_code_file("logo.png"));
        // I18n should return true
        assert!(is_non_code_file("src/locales/en.json"));
    }

    #[test]
    fn test_parse_patch_new_start_standard_hunk() {
        assert_eq!(parse_patch_new_start("@@ -10,3 +20,5 @@"), Some(20));
        assert_eq!(parse_patch_new_start("@@ -0,0 +1,42 @@"), Some(1));
        assert_eq!(
            parse_patch_new_start("@@ -100,10 +200,15 @@ fn context()"),
            Some(200)
        );
    }

    #[test]
    fn test_parse_patch_new_start_no_hunk() {
        assert_eq!(parse_patch_new_start("+pub fn added() {}"), None);
        assert_eq!(parse_patch_new_start("regular line"), None);
        assert_eq!(parse_patch_new_start("--- a/file.rs"), None);
    }

    #[test]
    fn test_contains_token_match_exact() {
        // Exact token match (surrounded by non-identifier boundaries)
        assert!(contains_token_match("use foo::bar;", "bar"));
        assert!(contains_token_match("bar is here", "bar"));
        assert!(contains_token_match("call(bar)", "bar"));

        // Should NOT match inside a larger identifier
        assert!(!contains_token_match("use foobar;", "bar"));
        assert!(!contains_token_match("barnacle::swim()", "bar"));
        assert!(!contains_token_match("rebar_count", "bar"));

        // Underscore is an identifier byte, so embedded matches fail
        assert!(!contains_token_match("foo_bar_baz", "bar"));

        // Exact full match
        assert!(contains_token_match("bar", "bar"));
    }

    #[test]
    fn test_contains_path_token_match_treats_underscore_and_dash_as_boundary() {
        // PR #12 review #23: a security keyword must match inside snake/kebab
        // path segments — `_` and `-` are boundaries for PATH matching.
        assert!(contains_path_token_match("src/auth_token.rs", "auth"));
        assert!(contains_path_token_match("src/token-auth.rs", "auth"));
        assert!(contains_path_token_match("src/auth.rs", "auth"));
        assert!(contains_path_token_match("crypto/mod.rs", "crypto"));

        // But an alphanumeric-adjacent substring is still NOT a token match:
        // "author" must not trip the "auth" keyword.
        assert!(!contains_path_token_match("src/author.rs", "auth"));
        assert!(!contains_path_token_match("src/reauth.rs", "auth"));
        assert!(!contains_path_token_match("src/auth2fa.rs", "auth"));

        // Path matching diverges from identifier matching exactly on `_`.
        assert!(contains_path_token_match("foo_bar_baz", "bar"));
        assert!(!contains_token_match("foo_bar_baz", "bar"));
    }

    #[test]
    fn test_strip_long_raw_string_single_line() {
        // A long, multi-hash raw string must be stripped in full on one line,
        // with the trailing code preserved. Exercises the allocation-free
        // delimiter scan on a long body.
        let body = "a\"#".repeat(1000); // embeds `"#` sequences that are NOT the `"##` terminator
        let line = format!("let x = r##\"{body}\"## + 1;");
        let mut state = RustLexState::default();
        let out = strip_rust_non_code(&line, &mut state, false);

        assert_eq!(out.code, "let x =  + 1;");
        assert!(state.raw_string_hashes.is_none());
    }

    #[test]
    fn test_strip_long_raw_string_multiline_carry() {
        // Unterminated long raw string carries state to the next line, then
        // closes on the correct `"##` delimiter.
        let mut state = RustLexState::default();

        let open = format!("let x = r##\"{}", "z".repeat(2000));
        let out1 = strip_rust_non_code(&open, &mut state, false);
        assert_eq!(out1.code, "let x = ");
        assert_eq!(state.raw_string_hashes, Some(2));

        let close = format!("{}\"## ;", "z".repeat(2000));
        let out2 = strip_rust_non_code(&close, &mut state, false);
        assert_eq!(out2.code, " ;");
        assert!(state.raw_string_hashes.is_none());
    }
}
