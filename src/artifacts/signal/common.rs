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
