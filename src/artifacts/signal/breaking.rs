//! Breaking changes manifest — heuristic scan for API-breaking changes.

use super::common::{ReviewFileCategory, classify_review_file};
use anyhow::Result;
use std::fmt::Write as FmtWrite;
use std::fs;
use std::path::Path;

/// Risk level for breaking change publicness heuristic (B3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BreakingRisk {
    High,
    Medium,
    Low,
}

#[derive(Debug, Clone)]
pub struct BreakingFinding {
    pub file: String,
    pub kind: BreakingKind,
    pub line: String,
    pub risk_level: BreakingRisk,
}

#[derive(Debug, Clone)]
pub enum BreakingKind {
    RemovedSymbol {
        symbol_type: String,
    },
    /// A symbol that disappeared from one file but reappeared (same kind + name)
    /// in another file in the same diff — a module move, typically still
    /// re-exported, so it is NOT a breaking removal (P1-08).
    RelocatedSymbol {
        symbol_type: String,
    },
    ChangedSignature {
        before: String,
        after: String,
    },
    NewEnvRequirement {
        variable: String,
    },
}

/// Analyze multiple patches and return all breaking change findings.
///
/// After gathering per-patch findings, removed symbols that reappear (same kind
/// and name) in a *different* file are reclassified as `RelocatedSymbol`: a
/// module move/re-export is not a breaking removal. This keeps the machine-facing
/// MERGE_GATE caveat honest instead of reporting module splits as mass removals.
pub fn analyze_all_breaking_changes(patch_texts: &[String]) -> Vec<BreakingFinding> {
    let mut all = Vec::new();
    let mut added_symbols: Vec<(String, String, String)> = Vec::new(); // (file, type, name)
    for patch in patch_texts {
        all.append(&mut analyze_patch_for_breaking_changes(patch));
        added_symbols.extend(collect_added_public_symbols(patch));
    }
    reclassify_relocated_symbols(&mut all, &added_symbols);
    all
}

/// Public-symbol declaration prefixes and the symbol-type label they map to.
/// Shared by removed-symbol detection and added-symbol collection so the two
/// sides use identical names/types when pairing moves.
const PUB_SYMBOL_TYPES: &[(&str, &str)] = &[
    ("pub fn ", "function"),
    ("pub struct ", "struct"),
    ("pub enum ", "enum"),
    ("pub trait ", "trait"),
    ("pub type ", "type alias"),
    ("pub const ", "constant"),
    ("pub static ", "static"),
];

/// Extract the identifier following a `pub <kw> ` prefix (best-effort, mirrors
/// `PUB_SYMBOL_TYPES`). Returns the symbol name for move-pairing.
fn symbol_name(line: &str) -> Option<String> {
    for (prefix, _) in PUB_SYMBOL_TYPES {
        if let Some(rest) = line.strip_prefix(prefix) {
            let name: String = rest
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_')
                .collect();
            if !name.is_empty() {
                return Some(name);
            }
        }
    }
    None
}

/// Classify an added NON-function public declaration as `(symbol_type, name)`.
///
/// `pub fn` is excluded on purpose: added functions go through the multi-line
/// signature accumulator instead, which reconstructs the full signature before
/// recording it.
fn classify_added_pub_symbol(line: &str) -> Option<(&'static str, String)> {
    PUB_SYMBOL_TYPES
        .iter()
        .filter(|(prefix, _)| *prefix != "pub fn ")
        .find(|(prefix, _)| line.starts_with(prefix))
        .and_then(|(_, symbol_type)| symbol_name(line).map(|name| (*symbol_type, name)))
}

/// Collect added public symbols across a patch as `(file, type, name)`.
fn collect_added_public_symbols(patch: &str) -> Vec<(String, String, String)> {
    let mut out = Vec::new();
    let mut current_file = String::new();
    let mut should_scan = false;

    for line in patch.lines() {
        if let Some(rest) = line.strip_prefix("diff --git a/") {
            if let Some(space_idx) = rest.find(" b/") {
                current_file = rest[space_idx + 3..].to_string();
                should_scan = should_scan_for_breaking_changes(&current_file);
            }
            continue;
        }
        if !should_scan {
            continue;
        }
        if let Some(content) = line.strip_prefix('+')
            && !line.starts_with("+++")
        {
            let trimmed = content.trim();
            for (prefix, symbol_type) in PUB_SYMBOL_TYPES {
                if trimmed.starts_with(prefix)
                    && let Some(name) = symbol_name(trimmed)
                {
                    out.push((current_file.clone(), symbol_type.to_string(), name));
                    break;
                }
            }
        }
    }
    out
}

/// Reclassify a removed symbol as relocated when the same (type, name) is added
/// in a different file in the same diff (module move / re-export).
fn reclassify_relocated_symbols(
    findings: &mut [BreakingFinding],
    added: &[(String, String, String)],
) {
    for f in findings.iter_mut() {
        let BreakingKind::RemovedSymbol { symbol_type } = &f.kind else {
            continue;
        };
        let Some(name) = symbol_name(&f.line) else {
            continue;
        };
        let symbol_type = symbol_type.clone();
        let relocated = added.iter().any(|(a_file, a_type, a_name)| {
            a_file != &f.file && a_type == &symbol_type && a_name == &name
        });
        if relocated {
            f.kind = BreakingKind::RelocatedSymbol { symbol_type };
        }
    }
}

/// Write pre-computed breaking change findings to `BREAKING_CHANGES.md`.
pub fn write_breaking_changes(dir: &Path, findings: &[BreakingFinding]) -> Result<()> {
    if findings.is_empty() {
        return Ok(());
    }

    let md = format_breaking_changes(findings);
    fs::write(dir.join("BREAKING_CHANGES.md"), md)?;

    Ok(())
}

/// Compute breaking risk level based on file path publicness (B3).
fn compute_breaking_risk(path: &str) -> BreakingRisk {
    let fname = path.rsplit('/').next().unwrap_or(path);
    let lower_fname = fname.to_lowercase();

    // High: barrel/re-export files
    if matches!(
        lower_fname.as_str(),
        "index.ts"
            | "index.tsx"
            | "index.js"
            | "index.jsx"
            | "index.mjs"
            | "lib.ts"
            | "lib.rs"
            | "mod.rs"
            | "main.rs"
            | "public-api.ts"
            | "public_api.ts"
    ) {
        return BreakingRisk::High;
    }

    // High: crate roots
    if path == "src/lib.rs" || path == "src/main.rs" {
        return BreakingRisk::High;
    }
    // Workspace crate roots: */src/lib.rs
    if path.ends_with("/src/lib.rs") && path.matches('/').count() <= 3 {
        return BreakingRisk::High;
    }

    // Count path depth (number of slashes)
    let depth = path.matches('/').count();
    if depth <= 1 {
        // e.g. "src/foo.rs" or "foo.rs"
        BreakingRisk::Medium
    } else {
        // Deep paths: internal/private modules
        BreakingRisk::Low
    }
}

/// Analyze a unified diff patch for breaking changes.
fn analyze_patch_for_breaking_changes(patch: &str) -> Vec<BreakingFinding> {
    let mut findings = Vec::new();
    let mut current_file = String::new();
    let mut should_scan_current_file = false;

    // Track removed/added public symbol declarations (ALL kinds in
    // `PUB_SYMBOL_TYPES`, not just `pub fn`) for remove+re-add pairing and
    // signature change detection: (file, symbol_type, name, full_line).
    let mut removed_syms: Vec<(String, String, String, String)> = Vec::new();
    let mut added_syms: Vec<(String, String, String, String)> = Vec::new();

    // When an added `pub fn` signature spans multiple diff lines, accumulate the
    // continuation lines so the "After" is the FULL signature, not just the
    // truncated opening `pub fn name(` line (BUG-4 / TOOLING-15).
    // (file, name, accumulated signature so far)
    let mut pending_added_fn: Option<(String, String, String)> = None;

    for line in patch.lines() {
        // Track current file from diff headers
        if let Some(rest) = line.strip_prefix("diff --git a/") {
            if let Some(space_idx) = rest.find(" b/") {
                current_file = rest[space_idx + 3..].to_string();
                should_scan_current_file = should_scan_for_breaking_changes(&current_file);
            }
            continue;
        }

        if !should_scan_current_file {
            continue;
        }

        // A pending multi-line signature is finalized by any non-added line.
        let is_added_line = line.starts_with('+') && !line.starts_with("+++");
        if !is_added_line && let Some((f, n, sig)) = pending_added_fn.take() {
            added_syms.push((f, "function".to_string(), n, sig));
        }

        // Removed lines
        if let Some(content) = line.strip_prefix('-') {
            let trimmed = content.trim();

            for (pattern, symbol_type) in PUB_SYMBOL_TYPES {
                if trimmed.starts_with(pattern) {
                    // Record EVERY public symbol kind for remove+re-add pairing,
                    // not only `pub fn` — a non-fn declaration re-emitted
                    // unchanged by the diff used to leak a phantom removal.
                    if let Some(name) = symbol_name(trimmed) {
                        removed_syms.push((
                            current_file.clone(),
                            (*symbol_type).to_string(),
                            name,
                            trimmed.to_string(),
                        ));
                    }
                    findings.push(BreakingFinding {
                        file: current_file.clone(),
                        kind: BreakingKind::RemovedSymbol {
                            symbol_type: symbol_type.to_string(),
                        },
                        line: trimmed.to_string(),
                        risk_level: compute_breaking_risk(&current_file),
                    });
                    break;
                }
            }

            // JS/TS exports
            if trimmed.starts_with("export ") || trimmed.starts_with("export default") {
                findings.push(BreakingFinding {
                    file: current_file.clone(),
                    kind: BreakingKind::RemovedSymbol {
                        symbol_type: "export".to_string(),
                    },
                    line: trimmed.to_string(),
                    risk_level: compute_breaking_risk(&current_file),
                });
            }
        }

        // Added lines — track public functions for signature comparison + env requirements
        if let Some(content) = line.strip_prefix('+')
            && !line.starts_with("+++")
        {
            let trimmed = content.trim();

            // Continuation of a multi-line signature already in progress.
            if let Some((_, _, sig)) = pending_added_fn.as_mut() {
                if !sig.ends_with('(') && !trimmed.is_empty() {
                    sig.push(' ');
                }
                sig.push_str(trimmed);
                if signature_complete(sig)
                    && let Some((f, n, s)) = pending_added_fn.take()
                {
                    added_syms.push((f, "function".to_string(), n, s));
                }
            } else if trimmed.starts_with("pub fn ")
                && let Some(name) = extract_fn_name(trimmed)
            {
                if signature_complete(trimmed) {
                    added_syms.push((
                        current_file.clone(),
                        "function".to_string(),
                        name,
                        trimmed.to_string(),
                    ));
                } else {
                    // Signature spans multiple lines — start accumulating.
                    pending_added_fn = Some((current_file.clone(), name, trimmed.to_string()));
                }
            } else if let Some((symbol_type, name)) = classify_added_pub_symbol(trimmed) {
                // Non-fn public declarations are single-line: record them
                // directly so the pairing below can cancel a matching removal.
                added_syms.push((
                    current_file.clone(),
                    symbol_type.to_string(),
                    name,
                    trimmed.to_string(),
                ));
            }

            // New env requirements
            if trimmed.contains("REQUIRED_ENV") || trimmed.contains(".env") {
                for word in trimmed.split_whitespace() {
                    // Segment each word on non-identifier boundaries (anything
                    // outside `[A-Za-z0-9_]`) so a glued token yields the bare
                    // candidate identifier(s) instead of a smeared string:
                    //   `"MY_VAR";`                  -> ["", "MY_VAR", ""]
                    //   `MY_VAR=value`               -> ["MY_VAR", "value"]
                    //   `process.env.MY_DB_TOKEN;`   -> ["process","env","MY_DB_TOKEN",""]
                    // Cleaning the whole word instead would fuse the value into
                    // the name (`MY_VARvalue`) and lose the match entirely.
                    for candidate in word.split(|c: char| !c.is_ascii_alphanumeric() && c != '_') {
                        // Skip the trigger keyword itself: `REQUIRED_ENV` is the
                        // marker we grep for, not a variable being introduced.
                        // The all-uppercase check keeps the existing semantics of
                        // rejecting names with digits (e.g. `MY_VAR2`).
                        if candidate.len() > 3
                            && candidate != "REQUIRED_ENV"
                            && candidate.contains('_')
                            && candidate
                                .chars()
                                .all(|c| c.is_ascii_uppercase() || c == '_')
                        {
                            findings.push(BreakingFinding {
                                file: current_file.clone(),
                                kind: BreakingKind::NewEnvRequirement {
                                    variable: candidate.to_string(),
                                },
                                line: trimmed.to_string(),
                                risk_level: compute_breaking_risk(&current_file),
                            });
                        }
                    }
                }
            }
        }
    }

    // Finalize a signature still being accumulated at end of patch.
    if let Some((f, n, s)) = pending_added_fn.take() {
        added_syms.push((f, "function".to_string(), n, s));
    }

    // Pair removed + added public symbols of the SAME kind and name in the same
    // file — every kind in `PUB_SYMBOL_TYPES`, not just `pub fn` (P1-09/10):
    //   - identical declaration line -> no-op remove+readd, drop the removal
    //     (e.g. a fn body rewritten to delegate, or a struct whose fields
    //     changed below an unchanged `pub struct` line, emitted as -/+ by the
    //     diff)
    //   - different declaration line -> a signature change, not a removal
    for (r_file, r_type, r_name, r_line) in &removed_syms {
        let Some((_, _, _, a_line)) = added_syms.iter().find(|(a_file, a_type, a_name, _)| {
            a_file == r_file && a_type == r_type && a_name == r_name
        }) else {
            continue;
        };

        // Either way the removed-symbol finding is a false positive: drop it.
        findings.retain(|f| {
            !(f.file == *r_file
                && matches!(
                    &f.kind,
                    BreakingKind::RemovedSymbol { symbol_type } if symbol_type == r_type
                )
                && f.line == *r_line)
        });

        if a_line != r_line {
            findings.push(BreakingFinding {
                file: r_file.clone(),
                kind: BreakingKind::ChangedSignature {
                    before: r_line.clone(),
                    after: a_line.clone(),
                },
                line: String::new(),
                risk_level: compute_breaking_risk(r_file),
            });
        }
    }

    findings
}

fn should_scan_for_breaking_changes(path: &str) -> bool {
    matches!(classify_review_file(path), ReviewFileCategory::Code)
}

/// Has this (possibly partial) `pub fn` signature reached its end?
///
/// A signature is complete once its parameter parens are balanced and it has
/// reached the body opener `{` or a `;` (trait method / declaration). Used to
/// decide whether to keep accumulating continuation lines for the "After"
/// reconstruction (BUG-4 / TOOLING-15).
fn signature_complete(sig: &str) -> bool {
    let mut depth: i32 = 0;
    for ch in sig.chars() {
        match ch {
            '(' => depth += 1,
            ')' => depth -= 1,
            '{' if depth <= 0 => return true,
            ';' if depth <= 0 => return true,
            _ => {}
        }
    }
    false
}

/// Extract function name from a `pub fn name(...)` line.
fn extract_fn_name(line: &str) -> Option<String> {
    let rest = line.strip_prefix("pub fn ")?;
    let name_end = rest.find('(')?;
    let name = &rest[..name_end];
    // Handle generics: `pub fn foo<T>(...)`
    let name = name.split('<').next().unwrap_or(name);
    Some(name.trim().to_string())
}

/// Format breaking changes as markdown.
fn format_breaking_changes(findings: &[BreakingFinding]) -> String {
    let mut md = String::new();

    md.push_str("# Breaking Changes (auto-detected)\n\n");
    md.push_str("> Heuristic scan — may contain false positives. Verify manually.\n\n");

    let removed: Vec<_> = findings
        .iter()
        .filter(|f| matches!(&f.kind, BreakingKind::RemovedSymbol { .. }))
        .collect();

    let relocated: Vec<_> = findings
        .iter()
        .filter(|f| matches!(&f.kind, BreakingKind::RelocatedSymbol { .. }))
        .collect();

    let changed: Vec<_> = findings
        .iter()
        .filter(|f| matches!(&f.kind, BreakingKind::ChangedSignature { .. }))
        .collect();

    let env_reqs: Vec<_> = findings
        .iter()
        .filter(|f| matches!(&f.kind, BreakingKind::NewEnvRequirement { .. }))
        .collect();

    if !relocated.is_empty() {
        let _ = writeln!(
            md,
            "> Note: {} symbol{} below moved to another file in this diff (same name + kind) and are typically still re-exported — treat as module-move false positives, not breaking removals, unless a separate signature change is called out.\n",
            relocated.len(),
            if relocated.len() == 1 { "" } else { "s" }
        );
    }

    if !removed.is_empty() {
        md.push_str("## Removed Public Symbols\n\n");
        md.push_str("| File | Symbol | Type |\n");
        md.push_str("|------|--------|------|\n");
        for f in &removed {
            if let BreakingKind::RemovedSymbol { symbol_type } = &f.kind {
                let _ = writeln!(md, "| {} | `{}` | {} |", f.file, f.line, symbol_type);
            }
        }
        md.push('\n');
    }

    if !relocated.is_empty() {
        md.push_str("## Relocated / Re-exported (non-breaking)\n\n");
        md.push_str("| File | Symbol | Type |\n");
        md.push_str("|------|--------|------|\n");
        for f in &relocated {
            if let BreakingKind::RelocatedSymbol { symbol_type } = &f.kind {
                let _ = writeln!(md, "| {} | `{}` | {} |", f.file, f.line, symbol_type);
            }
        }
        md.push('\n');
    }

    if !changed.is_empty() {
        md.push_str("## Changed Signatures\n\n");
        md.push_str("| File | Before | After |\n");
        md.push_str("|------|--------|-------|\n");
        // Collapse feature-gated duplicates: the same logical signature change
        // is often emitted once per `#[cfg(feature = ...)]` variant. Group by
        // (file, fn name), render one row, and note the variant count
        // (BUG-4 / TOOLING-15).
        let mut order: Vec<(String, String)> = Vec::new();
        let mut groups: std::collections::HashMap<(String, String), Vec<(&String, &String)>> =
            std::collections::HashMap::new();
        for f in &changed {
            if let BreakingKind::ChangedSignature { before, after } = &f.kind {
                let name = extract_fn_name(before)
                    .or_else(|| extract_fn_name(after))
                    .or_else(|| symbol_name(before))
                    .unwrap_or_else(|| before.clone());
                let key = (f.file.clone(), name);
                if !groups.contains_key(&key) {
                    order.push(key.clone());
                }
                groups.entry(key).or_default().push((before, after));
            }
        }
        for key in &order {
            let variants = &groups[key];
            let (before, after) = variants[0];
            if variants.len() > 1 {
                let _ = writeln!(
                    md,
                    "| {} | `{}` | `{}` _(+{} feature-gated variant{})_ |",
                    key.0,
                    before,
                    after,
                    variants.len() - 1,
                    if variants.len() - 1 == 1 { "" } else { "s" }
                );
            } else {
                let _ = writeln!(md, "| {} | `{}` | `{}` |", key.0, before, after);
            }
        }
        md.push('\n');
    }

    if !env_reqs.is_empty() {
        md.push_str("## New Environment Requirements\n\n");
        md.push_str("| File | Variable |\n");
        md.push_str("|------|----------|\n");
        for f in &env_reqs {
            if let BreakingKind::NewEnvRequirement { variable } = &f.kind {
                let _ = writeln!(md, "| {} | `{}` |", f.file, variable);
            }
        }
        md.push('\n');
    }

    md
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::regression::tests::is_test_file;

    #[test]
    fn breaking_changes_detects_removed_pub_fn() {
        let patch = "diff --git a/src/lib.rs b/src/lib.rs\n\
             index abc..def 100644\n\
             --- a/src/lib.rs\n\
             +++ b/src/lib.rs\n\
             @@ -10,5 +10,3 @@\n\
              fn internal() {}\n\
             -pub fn old_api(x: u32) -> bool {\n\
             -    x > 0\n\
             -}\n\
              fn another_internal() {}\n";

        let findings = analyze_patch_for_breaking_changes(patch);

        assert!(!findings.is_empty(), "Should detect removed pub fn");
        assert!(findings.iter().any(|f| {
            f.file == "src/lib.rs"
                && matches!(
                    &f.kind,
                    BreakingKind::RemovedSymbol { symbol_type } if symbol_type == "function"
                )
        }));
    }

    #[test]
    fn breaking_changes_skips_non_code_markdown() {
        let patch = "diff --git a/CLAUDE.md b/CLAUDE.md\n\
             index abc..def 100644\n\
             --- a/CLAUDE.md\n\
             +++ b/CLAUDE.md\n\
             @@ -1,3 +0,0 @@\n\
             -pub trait Check: Send + Sync {\n";

        let findings = analyze_patch_for_breaking_changes(patch);
        assert!(
            findings.is_empty(),
            "Non-code markdown should not produce breaking findings"
        );
    }

    #[test]
    fn breaking_changes_clean_diff() {
        let patch = "diff --git a/src/lib.rs b/src/lib.rs\n\
             index abc..def 100644\n\
             --- a/src/lib.rs\n\
             +++ b/src/lib.rs\n\
             @@ -10,3 +10,5 @@\n\
              fn internal() {}\n\
             +fn new_internal() {}\n\
             +pub fn new_api() -> bool { true }\n";

        let findings = analyze_patch_for_breaking_changes(patch);
        assert!(findings.is_empty(), "Clean diff should produce no findings");
    }

    #[test]
    fn breaking_changes_detects_signature_change() {
        let patch = "diff --git a/src/lib.rs b/src/lib.rs\n\
             index abc..def 100644\n\
             --- a/src/lib.rs\n\
             +++ b/src/lib.rs\n\
             @@ -10,3 +10,3 @@\n\
             -pub fn process(x: u32) -> bool {\n\
             +pub fn process(x: u32, y: bool) -> bool {\n";

        let findings = analyze_patch_for_breaking_changes(patch);

        assert!(
            findings.iter().any(|f| matches!(
                &f.kind,
                BreakingKind::ChangedSignature { before, after }
                    if before.contains("process(x: u32)")
                    && after.contains("process(x: u32, y: bool)")
            )),
            "Should detect signature change"
        );

        assert!(
            !findings.iter().any(|f| matches!(
                &f.kind,
                BreakingKind::RemovedSymbol { symbol_type } if symbol_type == "function"
            )),
            "Should not report removed symbol for signature change"
        );
    }

    #[test]
    fn new_env_requirement_detected_despite_adjacent_punctuation() {
        // Regression (PR #13 / #16): an UPPER_CASE env var glued to punctuation
        // must still be detected, but the fix must not (a) misfire on the trigger
        // keyword `REQUIRED_ENV` itself, nor (b) miss a var glued to a value via
        // `=` (no spaces). Each word is now segmented on non-identifier boundaries
        // and the trigger keyword is skipped.
        //
        // Every candidate-bearing line must contain `.env` or `REQUIRED_ENV`,
        // since that is the gate that opens the env-requirement scan.
        let patch = "diff --git a/src/config.rs b/src/config.rs\n\
             index abc..def 100644\n\
             --- a/src/config.rs\n\
             +++ b/src/config.rs\n\
             @@ -1,2 +1,4 @@\n\
              fn load() {}\n\
             +    // read from .env: \"MY_DATABASE_TOKEN\";\n\
             +    const token = process.env.MY_DATABASE_TOKEN;\n\
             +    // REQUIRED_ENV MY_DATABASE_URL=postgres://localhost/db\n";

        let findings = analyze_patch_for_breaking_changes(patch);
        let vars: Vec<String> = findings
            .iter()
            .filter_map(|f| match &f.kind {
                BreakingKind::NewEnvRequirement { variable } => Some(variable.clone()),
                _ => None,
            })
            .collect();

        // (a) property-path access `process.env.MY_DATABASE_TOKEN` and the
        //     quote/semicolon-glued `"MY_DATABASE_TOKEN";` both yield the clean var.
        assert!(
            vars.contains(&"MY_DATABASE_TOKEN".to_string()),
            "punctuation/property-path env var must be detected, got: {:?}",
            vars
        );
        // (b) assignment `MY_DATABASE_URL=postgres://...` (no spaces) must be
        //     detected — segmenting on `=` recovers the bare name.
        assert!(
            vars.contains(&"MY_DATABASE_URL".to_string()),
            "assignment-glued env var must be detected, got: {:?}",
            vars
        );
        // The trigger keyword itself must never be reported as a variable.
        assert!(
            !vars.contains(&"REQUIRED_ENV".to_string()),
            "REQUIRED_ENV trigger keyword must not be detected as a variable, got: {:?}",
            vars
        );
    }

    #[test]
    fn relocated_symbols_are_not_reported_as_removed() {
        // A module split: `compute_coverage_signal` leaves signal.rs and
        // reappears in signal/coverage.rs. It must be classified as relocated
        // (non-breaking re-export), not a removed public symbol (P1-08).
        let removed = "diff --git a/src/artifacts/signal.rs b/src/artifacts/signal.rs\n\
             --- a/src/artifacts/signal.rs\n\
             +++ b/src/artifacts/signal.rs\n\
             @@ -1,3 +0,0 @@\n\
             -pub fn compute_coverage_signal(diffs: &[Diff]) -> CoverageSignal {\n\
             -}\n"
            .to_string();
        let added =
            "diff --git a/src/artifacts/signal/coverage.rs b/src/artifacts/signal/coverage.rs\n\
             --- a/src/artifacts/signal/coverage.rs\n\
             +++ b/src/artifacts/signal/coverage.rs\n\
             @@ -0,0 +1,2 @@\n\
             +pub fn compute_coverage_signal(diffs: &[Diff]) -> CoverageSignal {\n\
             +}\n"
                .to_string();

        let findings = analyze_all_breaking_changes(&[removed, added]);

        assert!(
            findings.iter().any(|f| matches!(
                &f.kind,
                BreakingKind::RelocatedSymbol { symbol_type } if symbol_type == "function"
            )),
            "moved symbol should be relocated"
        );
        assert!(
            !findings.iter().any(|f| matches!(
                &f.kind,
                BreakingKind::RemovedSymbol { symbol_type } if symbol_type == "function"
            )),
            "moved symbol must NOT be reported as a breaking removal"
        );

        let md = format_breaking_changes(&findings);
        assert!(md.contains("Relocated / Re-exported (non-breaking)"));
        assert!(md.contains("moved to another file"));
    }

    #[test]
    fn genuine_removal_in_single_file_stays_removed() {
        // No re-add anywhere: a real removal must remain a RemovedSymbol.
        let patch = "diff --git a/src/lib.rs b/src/lib.rs\n\
             --- a/src/lib.rs\n\
             +++ b/src/lib.rs\n\
             @@ -1,2 +0,0 @@\n\
             -pub fn gone_for_good(x: u32) -> bool {\n\
             -}\n"
            .to_string();

        let findings = analyze_all_breaking_changes(&[patch]);
        assert!(findings.iter().any(|f| matches!(
            &f.kind,
            BreakingKind::RemovedSymbol { symbol_type } if symbol_type == "function"
        )));
    }

    #[test]
    fn identical_remove_readd_in_same_file_is_not_breaking() {
        // paths.rs case (P1-10): a `pub fn` body is rewritten to delegate, so
        // the diff emits the unchanged signature line as both - and +. It is
        // neither a removal nor a signature change.
        let patch = "diff --git a/src/paths.rs b/src/paths.rs\n\
             --- a/src/paths.rs\n\
             +++ b/src/paths.rs\n\
             @@ -1,3 +1,3 @@\n\
             -pub fn read_within(root: &Path, requested: &Path) -> Result<Vec<u8>> {\n\
             -    old_impl()\n\
             +pub fn read_within(root: &Path, requested: &Path) -> Result<Vec<u8>> {\n\
             +    open_file_within(root, requested)\n\
             }\n"
        .to_string();

        let findings = analyze_all_breaking_changes(&[patch]);
        assert!(
            !findings.iter().any(|f| matches!(
                &f.kind,
                BreakingKind::RemovedSymbol { .. } | BreakingKind::ChangedSignature { .. }
            )),
            "identical remove+readd must produce no breaking finding, got: {:?}",
            findings.iter().map(|f| &f.kind).collect::<Vec<_>>()
        );
    }

    /// Build a single-file patch from raw diff body lines (each already carrying
    /// its `-`/`+`/` ` prefix).
    fn one_file_patch(file: &str, body: &[&str]) -> String {
        let mut patch =
            format!("diff --git a/{file} b/{file}\n--- a/{file}\n+++ b/{file}\n@@ -1,2 +1,2 @@\n");
        for line in body {
            patch.push_str(line);
            patch.push('\n');
        }
        patch
    }

    fn removed_symbol_types(findings: &[BreakingFinding]) -> Vec<String> {
        findings
            .iter()
            .filter_map(|f| match &f.kind {
                BreakingKind::RemovedSymbol { symbol_type } => Some(symbol_type.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn identical_remove_readd_non_fn_symbols_are_not_breaking() {
        // P1-09/10 residual: the same-file remove+re-add pairing used to cover
        // `pub fn` ONLY, so a struct/enum/trait/type/const/static whose
        // declaration line was re-emitted unchanged by the diff (e.g. a body or
        // field reordering below it) produced a phantom RemovedSymbol.
        let cases: [(&str, &str, &str); 6] = [
            ("struct", "src/model.rs", "pub struct Config {"),
            ("enum", "src/model.rs", "pub enum Mode {"),
            ("trait", "src/model.rs", "pub trait Check {"),
            ("type alias", "src/model.rs", "pub type Alias = u32;"),
            ("constant", "src/model.rs", "pub const LIMIT: usize = 8;"),
            ("static", "src/model.rs", "pub static NAME: &str = \"a\";"),
        ];

        for (label, file, decl) in cases {
            let findings = analyze_all_breaking_changes(&[one_file_patch(
                file,
                &[
                    &format!("-{decl}"),
                    "-    old_detail: u8,",
                    &format!("+{decl}"),
                    "+    new_detail: u8,",
                ],
            )]);
            assert!(
                !findings.iter().any(|f| matches!(
                    &f.kind,
                    BreakingKind::RemovedSymbol { .. } | BreakingKind::ChangedSignature { .. }
                )),
                "identical remove+readd of {label} must produce no breaking finding, got: {:?}",
                findings.iter().map(|f| &f.kind).collect::<Vec<_>>()
            );
        }
    }

    #[test]
    fn changed_non_fn_declaration_is_signature_change_not_removal() {
        // A genuinely modified public declaration must surface as a signature
        // change (one finding), never as removal + silent re-addition.
        let cases: [(&str, &str, &str); 3] = [
            (
                "struct",
                "pub struct Config<T> {",
                "pub struct Config<T, U> {",
            ),
            (
                "type alias",
                "pub type Alias = u32;",
                "pub type Alias = u64;",
            ),
            (
                "constant",
                "pub const LIMIT: usize = 8;",
                "pub const LIMIT: u32 = 8;",
            ),
        ];

        for (label, before, after) in cases {
            let findings = analyze_all_breaking_changes(&[one_file_patch(
                "src/model.rs",
                &[&format!("-{before}"), &format!("+{after}")],
            )]);
            assert!(
                findings.iter().any(|f| matches!(
                    &f.kind,
                    BreakingKind::ChangedSignature { before: b, after: a }
                        if b == before && a == after
                )),
                "{label} change must be a ChangedSignature, got: {:?}",
                findings.iter().map(|f| &f.kind).collect::<Vec<_>>()
            );
            assert!(
                removed_symbol_types(&findings).is_empty(),
                "{label} change must not also report a removal, got: {:?}",
                removed_symbol_types(&findings)
            );
        }
    }

    #[test]
    fn genuine_non_fn_removal_stays_removed() {
        // Guard against the pairing over-reaching: with no re-add, real removals
        // of every public symbol kind must still be breaking.
        let cases: [(&str, &str); 6] = [
            ("struct", "pub struct Config {"),
            ("enum", "pub enum Mode {"),
            ("trait", "pub trait Check {"),
            ("type alias", "pub type Alias = u32;"),
            ("constant", "pub const LIMIT: usize = 8;"),
            ("static", "pub static NAME: &str = \"a\";"),
        ];

        for (expected_type, decl) in cases {
            let findings = analyze_all_breaking_changes(&[one_file_patch(
                "src/model.rs",
                &[&format!("-{decl}"), "-    detail: u8,"],
            )]);
            assert!(
                removed_symbol_types(&findings).contains(&expected_type.to_string()),
                "real removal of {expected_type} must stay a RemovedSymbol, got: {:?}",
                findings.iter().map(|f| &f.kind).collect::<Vec<_>>()
            );
        }
    }

    #[test]
    fn pub_use_reexport_is_not_a_tracked_public_symbol() {
        // `pub use` is deliberately outside PUB_SYMBOL_TYPES: re-export lines
        // churn constantly and were never emitted as RemovedSymbol, so neither
        // an identical nor a changed remove+re-add may invent a breaking
        // finding. Pins that contract so a future symbol-kind addition cannot
        // reintroduce the phantom asymmetry unpaired.
        let identical = analyze_all_breaking_changes(&[one_file_patch(
            "src/lib.rs",
            &[
                "-pub use crate::model::Config;",
                "+pub use crate::model::Config;",
            ],
        )]);
        let changed = analyze_all_breaking_changes(&[one_file_patch(
            "src/lib.rs",
            &[
                "-pub use crate::model::Config;",
                "+pub use crate::model::Settings;",
            ],
        )]);

        for (label, findings) in [("identical", identical), ("changed", changed)] {
            assert!(
                !findings.iter().any(|f| matches!(
                    &f.kind,
                    BreakingKind::RemovedSymbol { .. } | BreakingKind::ChangedSignature { .. }
                )),
                "{label} pub use re-export must produce no breaking finding, got: {:?}",
                findings.iter().map(|f| &f.kind).collect::<Vec<_>>()
            );
        }
    }

    #[test]
    fn changed_signature_reconstructs_multiline_after() {
        // The new signature is split across several lines in the diff. The
        // "After" must be the FULL reconstructed signature, not just the
        // truncated opening `pub fn query_index(` line (BUG-4 / TOOLING-15).
        let patch = "diff --git a/src/vector_index.rs b/src/vector_index.rs\n\
             --- a/src/vector_index.rs\n\
             +++ b/src/vector_index.rs\n\
             @@ -1,2 +1,5 @@\n\
             -pub fn query_index(project: Option<&str>, query: &str, limit: usize) -> Result<Vec<QueryHit>> {\n\
             +pub fn query_index(\n\
             +    project: Option<&str>,\n\
             +    query: &str,\n\
             +    limit: usize,\n\
             +) -> Result<Vec<QueryHit>> {\n"
            .to_string();

        let findings = analyze_all_breaking_changes(&[patch]);
        let changed = findings
            .iter()
            .find(|f| matches!(&f.kind, BreakingKind::ChangedSignature { .. }))
            .expect("signature change detected");
        if let BreakingKind::ChangedSignature { after, .. } = &changed.kind {
            assert!(
                after.contains("project: Option<&str>")
                    && after.contains("limit: usize")
                    && after.contains("-> Result<Vec<QueryHit>>"),
                "After must be the full reconstructed signature, got: {after:?}"
            );
        }
    }

    #[test]
    fn changed_signature_dedups_feature_gated_variants() {
        // The same `pub fn` signature change appears multiple times (once per
        // #[cfg(feature = ...)] variant). The Changed Signatures section must
        // collapse them into a single row with a variant-count note, not 4
        // identical rows (BUG-4 / TOOLING-15).
        let mk = |before_args: &str, after_open: &str| {
            format!(
                "diff --git a/src/vector_index.rs b/src/vector_index.rs\n\
                 --- a/src/vector_index.rs\n\
                 +++ b/src/vector_index.rs\n\
                 @@ -1,1 +1,1 @@\n\
                 -pub fn query_index({before_args}) -> Result<Vec<QueryHit>> {{\n\
                 +pub fn query_index({after_open}) -> Result<QueryHit> {{\n"
            )
        };
        // Two cfg variants (real + stubbed `_`-prefixed), each emitted twice by
        // the diff — the kind of duplication seen in feature-gated code.
        let patches = vec![
            mk("project: Option<&str>", "project: Option<&str>"),
            mk("project: Option<&str>", "project: Option<&str>"),
            mk("_project: Option<&str>", "_project: Option<&str>"),
            mk("_project: Option<&str>", "_project: Option<&str>"),
        ];

        let findings = analyze_all_breaking_changes(&patches);
        let changed_count = findings
            .iter()
            .filter(|f| matches!(&f.kind, BreakingKind::ChangedSignature { .. }))
            .count();
        assert!(
            changed_count > 0,
            "at least one signature change must survive"
        );

        let md = format_breaking_changes(&findings);
        // The Changed Signatures table must have a single data row for
        // query_index, not four.
        let query_rows = md
            .lines()
            .filter(|l| l.contains("query_index") && l.starts_with("| "))
            .count();
        assert_eq!(
            query_rows, 1,
            "feature-gated duplicates must collapse to one row, got:\n{md}"
        );
        assert!(
            md.contains("variant"),
            "collapsed row should note the variant count, got:\n{md}"
        );
    }

    // ── compute_breaking_risk tests ──────────────────────────────────

    #[test]
    fn test_breaking_risk_barrel_files_are_high() {
        assert_eq!(compute_breaking_risk("src/index.ts"), BreakingRisk::High);
        assert_eq!(compute_breaking_risk("src/index.tsx"), BreakingRisk::High);
        assert_eq!(compute_breaking_risk("src/index.js"), BreakingRisk::High);
        assert_eq!(compute_breaking_risk("src/index.jsx"), BreakingRisk::High);
        assert_eq!(compute_breaking_risk("src/index.mjs"), BreakingRisk::High);
        assert_eq!(compute_breaking_risk("src/mod.rs"), BreakingRisk::High);
        assert_eq!(compute_breaking_risk("src/lib.rs"), BreakingRisk::High);
        assert_eq!(
            compute_breaking_risk("packages/core/public-api.ts"),
            BreakingRisk::High
        );
        assert_eq!(
            compute_breaking_risk("packages/core/public_api.ts"),
            BreakingRisk::High
        );
    }

    #[test]
    fn test_breaking_risk_crate_roots_are_high() {
        assert_eq!(compute_breaking_risk("src/lib.rs"), BreakingRisk::High);
        assert_eq!(compute_breaking_risk("src/main.rs"), BreakingRisk::High);
    }

    #[test]
    fn test_breaking_risk_workspace_crate_root_high() {
        assert_eq!(
            compute_breaking_risk("crates/core/src/lib.rs"),
            BreakingRisk::High
        );
    }

    #[test]
    fn test_breaking_risk_shallow_path_is_medium() {
        assert_eq!(compute_breaking_risk("src/foo.rs"), BreakingRisk::Medium);
        assert_eq!(compute_breaking_risk("foo.rs"), BreakingRisk::Medium);
        assert_eq!(compute_breaking_risk("src/utils.ts"), BreakingRisk::Medium);
    }

    #[test]
    fn test_breaking_risk_deep_barrel_is_high() {
        // Deep path but filename is a barrel -> still High (filename check fires first)
        assert_eq!(
            compute_breaking_risk("src/components/button/index.ts"),
            BreakingRisk::High
        );
        assert_eq!(
            compute_breaking_risk("src/modules/auth/mod.rs"),
            BreakingRisk::High
        );
    }

    #[test]
    fn test_breaking_risk_deep_non_barrel_is_low() {
        assert_eq!(
            compute_breaking_risk("src/components/button/utils.ts"),
            BreakingRisk::Low
        );
        assert_eq!(
            compute_breaking_risk("src/features/auth/helpers.rs"),
            BreakingRisk::Low
        );
    }

    #[test]
    fn test_breaking_risk_empty_path_is_medium() {
        // Empty string: no slashes (depth=0 <=1), no barrel match -> Medium
        assert_eq!(compute_breaking_risk(""), BreakingRisk::Medium);
    }

    #[test]
    fn test_should_scan_for_breaking_changes_matches_code_only() {
        assert!(should_scan_for_breaking_changes("src/lib.rs"));
        assert!(should_scan_for_breaking_changes(
            "src/components/button.tsx"
        ));
        assert!(!should_scan_for_breaking_changes("tests/integration.rs"));
        assert!(!should_scan_for_breaking_changes("README.md"));
        assert!(!should_scan_for_breaking_changes(
            ".github/workflows/ci.yml"
        ));
        assert!(!should_scan_for_breaking_changes("src/types.d.ts"));
    }

    #[test]
    fn is_test_file_patterns() {
        assert!(is_test_file("src/lib_test.rs"));
        assert!(is_test_file("tests/integration/foo.rs"));
        assert!(is_test_file("src/components/Button.test.tsx"));
        assert!(is_test_file("src/components/__tests__/Button.tsx"));
        assert!(is_test_file("src/utils.spec.ts"));

        assert!(!is_test_file("src/lib.rs"));
        assert!(!is_test_file("src/main.rs"));
        assert!(!is_test_file("src/config/mod.rs"));
    }
}
