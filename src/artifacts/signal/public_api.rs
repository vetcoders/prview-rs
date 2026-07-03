//! Public API Diff — heuristic scan for public API surface changes.

use super::common::{ReviewFileCategory, RustLexState, classify_review_file, strip_rust_non_code};
use crate::checks::{CheckResult, CheckStatus};
use anyhow::Result;
use std::collections::HashSet;
use std::fmt::Write as FmtWrite;
use std::fs;
use std::path::Path;

/// A summarized result of public API changes between base and target.
#[derive(Debug, Clone, serde::Serialize)]
pub struct PublicApiDiff {
    pub added: Vec<ApiFinding>,
    pub removed: Vec<ApiFinding>,
    pub changed: Vec<ApiSignatureChange>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ApiFinding {
    pub file: String,
    pub symbol_type: String, // "function", "struct", "enum", "trait", "export"
    pub signature: String,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ApiSignatureChange {
    pub file: String,
    pub symbol_type: String,
    pub before: String,
    pub after: String,
}

/// Analyze patch texts and compute a diff of public symbols.
pub fn generate_public_api_diff(dir: &Path, patch_texts: &[String]) -> Result<Option<CheckResult>> {
    let mut added_findings = Vec::new();
    let mut removed_findings = Vec::new();
    let mut changed_findings = Vec::new();

    for patch in patch_texts {
        let (add, rm, ch) = analyze_patch_for_api_diff(patch);
        added_findings.extend(add);
        removed_findings.extend(rm);
        changed_findings.extend(ch);
    }

    if added_findings.is_empty() && removed_findings.is_empty() && changed_findings.is_empty() {
        return Ok(None);
    }

    // Sort for determinism, then drop exact duplicates (the same symbol can be
    // emitted more than once, e.g. a line repeated across hunks or feature-gated
    // variants of the same signature) — TOOLING-06.
    added_findings.sort_by(|a, b| a.file.cmp(&b.file).then(a.signature.cmp(&b.signature)));
    added_findings.dedup_by(|a, b| {
        a.file == b.file && a.symbol_type == b.symbol_type && a.signature == b.signature
    });
    removed_findings.sort_by(|a, b| a.file.cmp(&b.file).then(a.signature.cmp(&b.signature)));
    removed_findings.dedup_by(|a, b| {
        a.file == b.file && a.symbol_type == b.symbol_type && a.signature == b.signature
    });
    changed_findings.sort_by(|a, b| a.file.cmp(&b.file).then(a.before.cmp(&b.before)));
    dedupe_api_findings(&mut added_findings);
    dedupe_api_findings(&mut removed_findings);
    dedupe_signature_changes(&mut changed_findings);

    let diff = PublicApiDiff {
        added: added_findings,
        removed: removed_findings,
        changed: changed_findings,
    };

    fs::create_dir_all(dir)?;
    fs::write(
        dir.join("PUBLIC_API_DIFF.json"),
        serde_json::to_string_pretty(&diff)?,
    )?;

    let md = format_public_api_diff(&diff);
    fs::write(dir.join("PUBLIC_API_DIFF.md"), md)?;

    let msg = format!(
        "[Heuristic] Public API changed: {} new, {} removed, {} modified",
        diff.added.len(),
        diff.removed.len(),
        diff.changed.len()
    );

    Ok(Some(CheckResult {
        name: "public_api_diff".to_string(),
        status: CheckStatus::Warnings,
        duration: std::time::Duration::ZERO,
        output: msg,
        cached: false,
        provenance: None,
    }))
}

fn analyze_patch_for_api_diff(
    patch: &str,
) -> (Vec<ApiFinding>, Vec<ApiFinding>, Vec<ApiSignatureChange>) {
    let mut added = Vec::new();
    let mut removed = Vec::new();
    let mut changed = Vec::new();

    let mut current_file = String::new();
    let mut should_scan = false;
    // The old (removed) and new (added) sides of a diff are two different file
    // versions interleaved. Lexing them through one shared state let a `/*`
    // opened on one side swallow symbols on the other. Track them separately;
    // context lines feed both.
    let mut rust_state_old = RustLexState::default();
    let mut rust_state_new = RustLexState::default();

    let mut raw_added = Vec::new(); // (file, type, sig, used)
    let mut raw_removed = Vec::new();

    for line in patch.lines() {
        if let Some(rest) = line.strip_prefix("diff --git a/") {
            if let Some(space_idx) = rest.find(" b/") {
                current_file = rest[space_idx + 3..].to_string();
                should_scan = matches!(
                    classify_review_file(&current_file),
                    ReviewFileCategory::Code
                );
                rust_state_old = RustLexState::default();
                rust_state_new = RustLexState::default();
            }
            continue;
        }

        // Hunks are not contiguous source: reset both lexers so a block comment
        // opened in one hunk cannot bleed into the next.
        if line.starts_with("@@") {
            rust_state_old = RustLexState::default();
            rust_state_new = RustLexState::default();
            continue;
        }

        if !should_scan {
            continue;
        }

        if let Some(content) = line.strip_prefix(' ') {
            if current_file.ends_with(".rs") {
                let _ = strip_rust_non_code(content, &mut rust_state_old, false);
                let _ = strip_rust_non_code(content, &mut rust_state_new, false);
            }
            continue;
        }

        if let Some(content) = line.strip_prefix('-')
            && !line.starts_with("---")
            && let Some((sym_type, sig)) =
                extract_public_symbol_for_file(&current_file, content, &mut rust_state_old)
        {
            raw_removed.push((current_file.clone(), sym_type, sig));
        }

        if let Some(content) = line.strip_prefix('+')
            && !line.starts_with("+++")
            && let Some((sym_type, sig)) =
                extract_public_symbol_for_file(&current_file, content, &mut rust_state_new)
        {
            raw_added.push((current_file.clone(), sym_type, sig, false));
        }
    }

    // Detect signature changes: same type and name, but different full signature
    for (r_file, r_type, r_sig) in &raw_removed {
        let r_name = extract_name(r_sig);
        let mut matched = false;

        for (a_file, a_type, a_sig, used) in &mut raw_added {
            if *used {
                continue;
            }
            let a_name = extract_name(a_sig);
            // Match by exact prefix if it changed slightly
            if r_file == a_file && r_type == a_type && r_name == a_name && r_name.is_some() {
                if r_sig != a_sig {
                    changed.push(ApiSignatureChange {
                        file: r_file.clone(),
                        symbol_type: r_type.clone(),
                        before: r_sig.clone(),
                        after: a_sig.clone(),
                    });
                }
                *used = true;
                matched = true;
                break;
            }
        }

        if !matched {
            removed.push(ApiFinding {
                file: r_file.clone(),
                symbol_type: r_type.clone(),
                signature: r_sig.clone(),
            });
        }
    }

    // Now adding the remaining added that weren't matched as changed
    for (a_file, a_type, a_sig, used) in raw_added {
        if !used {
            added.push(ApiFinding {
                file: a_file,
                symbol_type: a_type,
                signature: a_sig,
            });
        }
    }

    (added, removed, changed)
}

fn extract_public_symbol_for_file(
    file: &str,
    content: &str,
    rust_state: &mut RustLexState,
) -> Option<(String, String)> {
    let trimmed = content.trim();
    if file.ends_with(".rs") {
        let code = strip_rust_non_code(content, rust_state, false).code;
        let code = code.trim();
        if code.is_empty() {
            return None;
        }
        return extract_public_symbol(code, true);
    }

    extract_public_symbol(trimmed, false)
}

fn extract_public_symbol(line: &str, rust_file: bool) -> Option<(String, String)> {
    let patterns = [
        ("pub const fn ", "function"),
        ("pub async fn ", "function"),
        ("pub unsafe fn ", "function"),
        ("pub extern fn ", "function"),
        ("pub fn ", "function"),
        ("pub struct ", "struct"),
        ("pub enum ", "enum"),
        ("pub trait ", "trait"),
        ("pub type ", "type alias"),
        ("pub const ", "constant"),
        ("pub static ", "static"),
        ("pub use ", "re-export"),
        ("export const ", "export"),
        ("export function ", "export"),
        ("export default ", "export"),
        ("export class ", "export"),
        ("export interface ", "export"),
        ("export type ", "export"),
    ];

    for (pfx, sym_type) in patterns {
        if rust_file && pfx.starts_with("export ") {
            continue;
        }
        if line.starts_with(pfx) {
            return Some((sym_type.to_string(), line.to_string()));
        }
    }

    None
}

fn extract_name(sig: &str) -> Option<String> {
    let fn_prefixes = [
        "pub const fn ",
        "pub async fn ",
        "pub unsafe fn ",
        "pub extern fn ",
        "pub fn ",
    ];
    for prefix in fn_prefixes {
        let Some(rest) = sig.strip_prefix(prefix) else {
            continue;
        };
        let name_end = rest.find('(')?;
        let name = &rest[..name_end];
        let name = name.split('<').next().unwrap_or(name);
        return Some(name.trim().to_string());
    }
    if sig.starts_with("pub struct ") {
        let rest = sig.strip_prefix("pub struct ")?;
        let name = rest.split_whitespace().next().unwrap_or(rest);
        let name = name.split('<').next().unwrap_or(name);
        let name = name.split('{').next().unwrap_or(name);
        return Some(name.trim().to_string());
    } else if sig.starts_with("pub enum ") {
        let rest = sig.strip_prefix("pub enum ")?;
        let name = rest.split_whitespace().next().unwrap_or(rest);
        let name = name.split('<').next().unwrap_or(name);
        let name = name.split('{').next().unwrap_or(name);
        return Some(name.trim().to_string());
    } else if sig.starts_with("pub trait ") {
        let rest = sig.strip_prefix("pub trait ")?;
        let name = rest.split_whitespace().next().unwrap_or(rest);
        let name = name.split('<').next().unwrap_or(name);
        let name = name.split('{').next().unwrap_or(name);
        return Some(name.trim().to_string());
    } else if sig.starts_with("pub type ") {
        let rest = sig.strip_prefix("pub type ")?;
        let name = rest.split_whitespace().next().unwrap_or(rest);
        return Some(name.trim().to_string());
    }
    None
}

fn dedupe_api_findings(findings: &mut Vec<ApiFinding>) {
    let mut seen = HashSet::new();
    findings.retain(|finding| {
        seen.insert((
            finding.file.clone(),
            finding.symbol_type.clone(),
            finding.signature.clone(),
        ))
    });
}

fn dedupe_signature_changes(findings: &mut Vec<ApiSignatureChange>) {
    let mut seen = HashSet::new();
    findings.retain(|finding| {
        let name = extract_name(&finding.before).unwrap_or_else(|| finding.before.clone());
        seen.insert((finding.file.clone(), finding.symbol_type.clone(), name))
    });
}

fn format_public_api_diff(diff: &PublicApiDiff) -> String {
    let mut md = String::new();
    let _ = writeln!(md, "# Public API Diff\n");
    let _ = writeln!(
        md,
        "> ⚠️ **NEEDS VERIFICATION**: *Generated by a fast text heuristic. It may miss AST details or raise false positives (e.g. macros). `export ...` is only scanned in JS/TS files; `pub use` is labelled as a re-export.* \n"
    );

    if !diff.added.is_empty() {
        let _ = writeln!(md, "## Added ({} elements)", diff.added.len());
        for item in &diff.added {
            let _ = writeln!(
                md,
                "- **{}** in `{}`: `{}`",
                item.symbol_type, item.file, item.signature
            );
        }
        let _ = writeln!(md);
    }

    if !diff.removed.is_empty() {
        let _ = writeln!(md, "## Removed ({} elements)", diff.removed.len());
        for item in &diff.removed {
            let _ = writeln!(
                md,
                "- **{}** in `{}`: `{}`",
                item.symbol_type, item.file, item.signature
            );
        }
        let _ = writeln!(md);
    }

    if !diff.changed.is_empty() {
        let _ = writeln!(md, "## Changed ({} elements)", diff.changed.len());
        for item in &diff.changed {
            let _ = writeln!(
                md,
                "- **{}** in `{}`:\n  - Before: `{}`\n  - After: `{}`",
                item.symbol_type, item.file, item.before, item.after
            );
        }
        let _ = writeln!(md);
    }

    md
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn public_api_diff_detects_additions() {
        let patch = "diff --git a/src/lib.rs b/src/lib.rs\n\
             --- a/src/lib.rs\n\
             +++ b/src/lib.rs\n\
             @@ -10,0 +10,1 @@\n\
             +pub fn new_super_api() {}\n";

        let (add, rm, ch) = analyze_patch_for_api_diff(patch);
        assert_eq!(add.len(), 1);
        assert!(rm.is_empty());
        assert!(ch.is_empty());
        assert_eq!(add[0].signature, "pub fn new_super_api() {}");
    }

    #[test]
    fn public_api_diff_detects_signature_changes() {
        let patch = "diff --git a/src/lib.rs b/src/lib.rs\n\
             --- a/src/lib.rs\n\
             +++ b/src/lib.rs\n\
             @@ -10,1 +10,1 @@\n\
             -pub fn old_api(x: u32) -> bool {\n\
             +pub fn old_api(x: u32, y: u32) -> bool {\n";

        let (add, rm, ch) = analyze_patch_for_api_diff(patch);
        assert!(add.is_empty());
        assert!(rm.is_empty());
        assert_eq!(ch.len(), 1);
        assert_eq!(ch[0].before, "pub fn old_api(x: u32) -> bool {");
        assert_eq!(ch[0].after, "pub fn old_api(x: u32, y: u32) -> bool {");
    }

    #[test]
    fn test_detect_removed_public_symbol() {
        let patch = "diff --git a/src/lib.rs b/src/lib.rs\n\
             --- a/src/lib.rs\n\
             +++ b/src/lib.rs\n\
             @@ -10,2 +10,0 @@\n\
             -pub fn deprecated_api(x: u32) -> bool {\n\
             -}\n";

        let (add, rm, ch) = analyze_patch_for_api_diff(patch);
        assert!(add.is_empty());
        assert_eq!(rm.len(), 1);
        assert!(ch.is_empty());
        assert_eq!(rm[0].symbol_type, "function");
        assert_eq!(rm[0].signature, "pub fn deprecated_api(x: u32) -> bool {");
        assert_eq!(rm[0].file, "src/lib.rs");
    }

    #[test]
    fn removed_block_comment_does_not_swallow_added_symbol() {
        // The removed line opens `/*` on the OLD side; with a shared lexer that
        // open comment bled into the NEW side and hid the added public fn.
        let patch = "diff --git a/src/lib.rs b/src/lib.rs\n\
             --- a/src/lib.rs\n\
             +++ b/src/lib.rs\n\
             @@ -10,1 +10,1 @@\n\
             -pub fn old_thing() { /* stray open on old side\n\
             +pub fn brand_new_api() {}\n";

        let (add, rm, _ch) = analyze_patch_for_api_diff(patch);
        assert_eq!(add.len(), 1, "added symbol must survive old-side comment");
        assert_eq!(add[0].signature, "pub fn brand_new_api() {}");
        assert_eq!(rm.len(), 1);
    }

    #[test]
    fn test_detect_removed_js_export() {
        let patch = "diff --git a/src/utils.ts b/src/utils.ts\n\
             --- a/src/utils.ts\n\
             +++ b/src/utils.ts\n\
             @@ -5,2 +5,0 @@\n\
             -export function helperA() {}\n\
             -export const MY_CONSTANT = 42;\n";

        let (add, rm, ch) = analyze_patch_for_api_diff(patch);
        assert!(add.is_empty());
        assert_eq!(rm.len(), 2);
        assert!(ch.is_empty());
        assert!(rm.iter().any(|r| r.signature.contains("helperA")));
        assert!(rm.iter().any(|r| r.signature.contains("MY_CONSTANT")));
    }

    #[test]
    fn test_rust_fixture_js_exports_are_ignored() {
        let patch = "diff --git a/src/analyzer.rs b/src/analyzer.rs\n\
             --- a/src/analyzer.rs\n\
             +++ b/src/analyzer.rs\n\
             @@ -1,0 +1,5 @@\n\
             +let fixture = r#\"\n\
             +export function getCount() { return count; }\n\
             +export default class HeroSection {}\n\
             +\"#;\n";

        let (add, rm, ch) = analyze_patch_for_api_diff(patch);
        assert!(add.is_empty());
        assert!(rm.is_empty());
        assert!(ch.is_empty());
    }

    #[test]
    fn test_pub_const_fn_is_function_not_constant() {
        let patch = "diff --git a/src/lib.rs b/src/lib.rs\n\
             --- a/src/lib.rs\n\
             +++ b/src/lib.rs\n\
             @@ -1,0 +1,1 @@\n\
             +pub const fn new(name: &'static str) -> Self {\n";

        let (add, _, _) = analyze_patch_for_api_diff(patch);
        assert_eq!(add.len(), 1);
        assert_eq!(add[0].symbol_type, "function");
    }

    #[test]
    fn test_generate_public_api_diff_wrapper() {
        let tmp = TempDir::new().unwrap();
        let out_dir = tmp.path().join("30_context");

        let patches = vec![
            "diff --git a/src/lib.rs b/src/lib.rs\n\
             --- a/src/lib.rs\n\
             +++ b/src/lib.rs\n\
             @@ -1,0 +1,1 @@\n\
             +pub fn brand_new_api() -> String {}\n"
                .to_string(),
        ];

        let result = generate_public_api_diff(&out_dir, &patches).unwrap();
        assert!(result.is_some(), "should detect the added pub fn");

        let cr = result.unwrap();
        assert_eq!(cr.name, "public_api_diff");
        assert!(cr.output.contains("1 new"));

        // Verify output files
        assert!(out_dir.join("PUBLIC_API_DIFF.json").exists());
        assert!(out_dir.join("PUBLIC_API_DIFF.md").exists());
    }

    #[test]
    fn const_fn_is_classified_as_function_not_constant() {
        let patch = "diff --git a/src/lib.rs b/src/lib.rs\n\
             +++ b/src/lib.rs\n\
             @@ -1,0 +1,1 @@\n\
             +pub const fn new(name: &'static str) -> Self {\n";
        let (add, _, _) = analyze_patch_for_api_diff(patch);
        assert_eq!(add.len(), 1);
        assert_eq!(add[0].symbol_type, "function");
    }

    #[test]
    fn js_export_inside_rust_source_is_ignored() {
        // A JS fixture embedded in a Rust string must not become a Rust symbol.
        let patch = "diff --git a/src/analyzer/ast_js/mod.rs b/src/analyzer/ast_js/mod.rs\n\
             +++ b/src/analyzer/ast_js/mod.rs\n\
             @@ -1,0 +1,2 @@\n\
             +export default class HeroSection {}\n\
             +export function getCount() { return count; }\n";
        let (add, _, _) = analyze_patch_for_api_diff(patch);
        assert!(
            add.is_empty(),
            "JS `export` syntax in a .rs file is not a Rust public symbol"
        );
    }

    #[test]
    fn pub_use_is_labelled_re_export() {
        let patch = "diff --git a/src/lib.rs b/src/lib.rs\n\
             +++ b/src/lib.rs\n\
             @@ -1,0 +1,1 @@\n\
             +pub use intent_source::{CliIntentSource, IntentSource};\n";
        let (add, _, _) = analyze_patch_for_api_diff(patch);
        assert_eq!(add.len(), 1);
        assert_eq!(add[0].symbol_type, "re-export");
    }

    #[test]
    fn duplicate_added_symbols_are_deduped() {
        let tmp = TempDir::new().unwrap();
        let out_dir = tmp.path().join("30_context");
        let patch = "diff --git a/src/main.rs b/src/main.rs\n\
             +++ b/src/main.rs\n\
             @@ -1,0 +1,3 @@\n\
             +pub fn public_entry() {}\n\
             +pub fn public_entry() {}\n\
             +pub fn public_entry() {}\n"
            .to_string();
        let cr = generate_public_api_diff(&out_dir, &[patch])
            .unwrap()
            .unwrap();
        assert!(
            cr.output.contains("1 new"),
            "exact duplicates collapse to a single entry"
        );
    }
}
