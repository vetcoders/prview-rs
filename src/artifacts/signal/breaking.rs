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

/// Symbol-type label of a `pub <kw> ` declaration line (mirrors
/// `PUB_SYMBOL_TYPES`). Distinguishes namespaces that may share an identifier.
fn symbol_kind(line: &str) -> Option<&'static str> {
    PUB_SYMBOL_TYPES
        .iter()
        .find(|(prefix, _)| line.starts_with(prefix))
        .map(|(_, symbol_type)| *symbol_type)
}

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

/// Classify a public declaration line as `(symbol_type, name)`.
///
/// Covers every kind in `PUB_SYMBOL_TYPES`, `pub fn` included: all of them go
/// through the same multi-line accumulator, so the recorded text is the full
/// declaration rather than its truncated opening line.
fn classify_pub_declaration(line: &str) -> Option<(&'static str, String)> {
    PUB_SYMBOL_TYPES
        .iter()
        .find(|(prefix, _)| line.starts_with(prefix))
        .and_then(|(_, symbol_type)| symbol_name(line).map(|name| (*symbol_type, name)))
}

/// Which side of the unified diff a declaration came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DiffSide {
    Removed,
    Added,
}

/// Where a declaration sits: everything about its position that pairing needs,
/// tracked per diff side.
struct DeclSite<'a> {
    file: &'a str,
    scope: &'a ModScope,
    /// The `#[cfg(…)]` conjunction currently standing above the next
    /// declaration on this side, `None` when the diff has not shown one.
    cfg_guard: Option<&'a [String]>,
    side: DiffSide,
}

/// A public declaration collected from one side of the diff.
#[derive(Debug)]
struct SymbolDecl {
    file: String,
    symbol_type: String,
    name: String,
    /// Full declaration text — continuation lines joined, not just the opener.
    ///
    /// Verbatim, comments included: this is what a reader sees in
    /// `BREAKING_CHANGES.md` and in a `ChangedSignature`. Comparisons use
    /// [`identity`](Self::identity) instead.
    text: String,
    /// The same declaration with its comments resolved away.
    ///
    /// What pairing COMPARES. A comment inside a declaration is not part of the
    /// API: rewording one used to make a remove+re-add of a byte-identical
    /// signature come out as a `ChangedSignature` — a breaking-change claim
    /// about text no consumer can observe. Literals are kept, because a literal
    /// IS code: `pub const GREETING: &str = "hello";` and the same line ending
    /// `"bye";` are different declarations.
    identity: String,
    /// Hunk-local inline-module path (`""` when the diff never showed one).
    scope: String,
    /// Every `#[cfg(…)]` predicate guarding this declaration, whitespace
    /// removed and sorted. `None` means the diff never showed one for this
    /// side — unknown, not "unguarded".
    cfg_guard: Option<Vec<String>>,
    side: DiffSide,
    /// Continuation lines absorbed so far, capped by
    /// [`MAX_DECL_CONTINUATION_LINES`].
    continuation_lines: usize,
}

/// Continuation lines a single declaration may absorb before it is finalized
/// as-is. Bounds runaway accumulation — a `Lazy::new(|| { .. })` static body or
/// a hundred-line `pub const WORDS: &[&str] = &[` table, where "the rest of the
/// declaration" is data, not signature.
///
/// The bound is a safety valve, NOT a display width: what gets truncated here is
/// the text the pairing COMPARES. At eight lines it cut inside the real
/// distribution, so two long declarations agreeing on their opener and first
/// eight lines finalized to the same truncated text, paired as an unchanged
/// re-add and swallowed a parameter, bound or return type changed below the cut.
/// Measured over 2,970,120 `pub` declarations in the local crates.io registry:
/// 94.76% wrap over no continuation line at all, 4.96% over one to eight, and
/// 0.27% over more — of which this bound now covers everything up to 32 lines
/// (87% of that remainder). What is left beyond it is dominated by generated
/// data tables. A declaration longer than the bound is still compared on its
/// first 32 lines, so a change below the cut can still hide; widening it further
/// trades that for smearing whole static bodies into one "declaration".
const MAX_DECL_CONTINUATION_LINES: usize = 32;

/// Inline-module nesting for ONE side of a unified diff.
///
/// Context lines feed both sides, `-` lines only the "before" side and `+` lines
/// only the "after" side, so a rename or a moved block cannot unbalance the
/// tracker. State is hunk-local: it resets at every `@@` header because hunks
/// are not contiguous, and an unseen module opener simply leaves the scope
/// unknown (`""`) rather than inventing one.
#[derive(Default)]
struct ModScope {
    /// `(module name, brace depth the module was opened at)`.
    stack: Vec<(String, i32)>,
    depth: i32,
    /// Carries a `/* … */` or a string literal left open by an earlier line of
    /// this side.
    scanner: crate::rust_source::SourceScanner,
}

impl ModScope {
    fn reset(&mut self) {
        self.stack.clear();
        self.depth = 0;
        self.scanner.reset();
    }

    /// Feed one diff payload line to this side's tracker.
    ///
    /// Only the CODE part is counted. A brace inside a literal or a comment is
    /// data: `const CLOSE: &str = "}";` inside `mod a` used to pop the module,
    /// leaving a later removal of `a::Config` with an unknown scope — which
    /// pairs with anything, so an unrelated `b::Config` addition cancelled a
    /// real API removal. Block comments AND string literals are tracked across
    /// lines: commenting a block of code out is exactly how an unbalanced brace
    /// ends up inside a comment, and a multi-line template or JSON fixture is
    /// exactly how one ends up inside a literal. State is per side and per
    /// hunk — see [`ModScope::reset`].
    fn feed(&mut self, payload: &str) {
        let code = self.scanner.code_only(payload);
        let opened = mod_opening_name(code.trim());
        let start_depth = self.depth;
        for ch in code.chars() {
            match ch {
                '{' => self.depth += 1,
                '}' => self.depth -= 1,
                _ => {}
            }
        }
        if let Some(name) = opened
            && self.depth > start_depth
        {
            self.stack.push((name, start_depth));
        }
        while let Some((_, opened_at)) = self.stack.last() {
            if self.depth <= *opened_at {
                self.stack.pop();
            } else {
                break;
            }
        }
    }

    fn path(&self) -> String {
        self.stack
            .iter()
            .map(|(name, _)| name.as_str())
            .collect::<Vec<_>>()
            .join("::")
    }
}

/// Name of the inline module opened by `mod name {` / `pub mod name {` /
/// `pub(crate) mod name {`, if this line opens one.
///
/// The brace must sit on the SAME line, which is what rustfmt emits and what
/// `mod name;` (a file module, not a scope) is told apart by. A declaration
/// written `mod name` with `{` on the next line is not recorded, so lines under
/// it carry no scope — `None`, which pairs with anything, the same conservative
/// default the diff already produces when a hunk omits the context line.
/// Carrying a pending name across lines is deliberately not done: the style is
/// 20 sites in a single crate out of 2025 sampled from crates.io (0.12% of
/// module declarations, zero here), and the state it needs would itself be
/// heuristic at hunk boundaries.
fn mod_opening_name(trimmed: &str) -> Option<String> {
    if !trimmed.contains('{') {
        return None;
    }
    let mut rest = trimmed;
    if let Some(after_pub) = rest.strip_prefix("pub") {
        match after_pub.chars().next() {
            Some('(') => rest = &after_pub[after_pub.find(')')? + 1..],
            Some(c) if c.is_whitespace() => rest = after_pub,
            _ => return None,
        }
    }
    let rest = rest.trim_start().strip_prefix("mod")?;
    if !rest.starts_with(char::is_whitespace) {
        return None;
    }
    let rest = rest.trim_start();
    // A module may be named with a keyword through a raw identifier. Stopping at
    // the `#` recorded `r#type` and `r#match` both as `r`, so two different
    // namespaces looked like one and a removal from the first paired away
    // against an unrelated addition in the second. The prefix is kept in the
    // name because it is part of how the path is written.
    let (prefix, rest) = match rest.strip_prefix("r#") {
        Some(after) => ("r#", after),
        None => ("", rest),
    };
    let name: String = rest
        .chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_')
        .collect();
    (!name.is_empty()).then(|| format!("{prefix}{name}"))
}

/// May a removal in `removed_scope` and an addition in `added_scope` describe
/// the same declaration site?
///
/// Scopes are hunk-local and often unknown, so an unknown scope stays
/// compatible with anything — that keeps today's pairing everywhere the diff
/// does not show a module boundary. Two *known and different* module paths mean
/// two different namespaces: `a::Config` disappearing while `b::Config` appears
/// is a real removal, not a no-op re-add.
///
/// The known gap — two empty scopes pair even when the file's real modules
/// differ — is deliberate and measured, not overlooked. Over 173 commits of this
/// repository the current rule reports 3 removals and 4 signature changes, all
/// genuine. Treating an unknown scope as incompatible instead reports 7 removals
/// and 0 signature changes: it invents removals of symbols that are alive today
/// (`build_cli_json_summary`, `compute_exit_code`, `generate_diffs`, `McpArgs`)
/// and erases every real signature change, because a symbol whose declaration
/// moved across a hunk boundary then looks deleted. Seeding the scope from the
/// `@@` section heading does not close the gap either: only 149 of 1022 hunk
/// headers name a module at all, and virtually all of them say `mod tests`.
/// Closing it honestly needs the module path of the declaration site in the
/// source and target files, which means reading those files at those revisions —
/// input `analyze_patch_for_breaking_changes(patch: &str)` does not have. Until
/// this analysis is given repo + revision access, the ambiguous case resolves
/// toward not fabricating a breaking change.
fn scopes_may_pair(removed_scope: &str, added_scope: &str) -> bool {
    removed_scope.is_empty() || added_scope.is_empty() || removed_scope == added_scope
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
    // signature change detection.
    let mut removed_syms: Vec<SymbolDecl> = Vec::new();
    let mut added_syms: Vec<SymbolDecl> = Vec::new();

    // A public declaration may span several diff lines — `pub fn name(` with the
    // parameters below it (BUG-4 / TOOLING-15), but equally `pub struct Name<`
    // with its bounds below it. Accumulate continuation lines on BOTH sides so
    // remove+re-add pairing compares full declarations: a change confined to a
    // continuation line used to hide behind an identical opening line.
    let mut pending_removed: Option<PendingDecl> = None;
    let mut pending_added: Option<PendingDecl> = None;

    // Inline-module nesting, tracked per diff side (see `ModScope`).
    let mut before_scope = ModScope::default();
    let mut after_scope = ModScope::default();

    // The `#[cfg(…)]` currently standing above the next declaration, per side.
    // Context lines feed both, so an unchanged guard above a re-emitted
    // declaration is KNOWN on both sides and the pair is not split by it.
    let mut before_cfg = CfgGuard::default();
    let mut after_cfg = CfgGuard::default();

    for line in patch.lines() {
        // Track current file from diff headers
        if let Some(rest) = line.strip_prefix("diff --git a/") {
            finalize_decl(&mut pending_removed, &mut removed_syms, &mut findings);
            finalize_decl(&mut pending_added, &mut added_syms, &mut findings);
            before_scope.reset();
            after_scope.reset();
            before_cfg.reset();
            after_cfg.reset();
            if let Some(space_idx) = rest.find(" b/") {
                current_file = rest[space_idx + 3..].to_string();
                should_scan_current_file = should_scan_for_breaking_changes(&current_file);
            }
            continue;
        }

        if !should_scan_current_file {
            continue;
        }

        // Hunks are not contiguous: a boundary ends any pending declaration and
        // invalidates the brace depth both scope trackers were carrying.
        if line.starts_with("@@") {
            finalize_decl(&mut pending_removed, &mut removed_syms, &mut findings);
            finalize_decl(&mut pending_added, &mut added_syms, &mut findings);
            before_scope.reset();
            after_scope.reset();
            before_cfg.reset();
            after_cfg.reset();
            continue;
        }

        let removed_content = if line.starts_with("---") {
            None
        } else {
            line.strip_prefix('-')
        };
        let added_content = if line.starts_with("+++") {
            None
        } else {
            line.strip_prefix('+')
        };

        // Removed lines
        if let Some(content) = removed_content {
            finalize_decl(&mut pending_added, &mut added_syms, &mut findings);
            let trimmed = content.trim();

            // Record EVERY public symbol kind for remove+re-add pairing, not
            // only `pub fn` — a non-fn declaration re-emitted unchanged by the
            // diff used to leak a phantom removal.
            accumulate_decl(
                &mut pending_removed,
                &mut removed_syms,
                &mut findings,
                trimmed,
                &DeclSite {
                    file: &current_file,
                    scope: &before_scope,
                    cfg_guard: before_cfg.guard(),
                    side: DiffSide::Removed,
                },
            );
            before_cfg.feed(trimmed);

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

            before_scope.feed(content);
            continue;
        }

        // A pending declaration is finalized by any line from the other side.
        finalize_decl(&mut pending_removed, &mut removed_syms, &mut findings);

        // Added lines — track public declarations for signature comparison + env requirements
        if let Some(content) = added_content {
            let trimmed = content.trim();

            accumulate_decl(
                &mut pending_added,
                &mut added_syms,
                &mut findings,
                trimmed,
                &DeclSite {
                    file: &current_file,
                    scope: &after_scope,
                    cfg_guard: after_cfg.guard(),
                    side: DiffSide::Added,
                },
            );
            after_cfg.feed(trimmed);

            after_scope.feed(content);

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
            continue;
        }

        // Context (or non-hunk) line: it belongs to both sides, and it ends any
        // declaration that was still accumulating on the added side.
        finalize_decl(&mut pending_added, &mut added_syms, &mut findings);
        let content = line.strip_prefix(' ').unwrap_or(line);
        let trimmed = content.trim();
        before_cfg.feed(trimmed);
        after_cfg.feed(trimmed);
        before_scope.feed(content);
        after_scope.feed(content);
    }

    // Finalize declarations still being accumulated at end of patch.
    finalize_decl(&mut pending_removed, &mut removed_syms, &mut findings);
    finalize_decl(&mut pending_added, &mut added_syms, &mut findings);

    // Pair removed + added public symbols of the SAME kind and name in the same
    // file — every kind in `PUB_SYMBOL_TYPES`, not just `pub fn` (P1-09/10):
    //   - identical declaration -> no-op remove+readd, drop the removal
    //     (e.g. a fn body rewritten to delegate, or a struct whose fields
    //     changed below an unchanged `pub struct` line, emitted as -/+ by the
    //     diff)
    //   - different declaration -> a signature change, not a removal
    //
    // Pairing additionally requires compatible inline-module scopes, so a
    // removal in one module is not cancelled by an unrelated same-named
    // declaration added in another module of the same file.
    //
    // Pairing is one-to-one: an addition is consumed once. `cfg`-gated variants
    // share (file, kind, name), so a non-consuming search let every removal
    // cancel against the same unchanged re-add — the addition that actually
    // replaced one of them was left unpaired and its change went unreported.
    // Exact matches are claimed first (pass 1) so an unchanged re-add is never
    // spent on a removal that a different addition replaces.
    //
    // ACCEPTED LIMIT (deferred to 0.8, do not re-litigate). Pairing sees only
    // the declaration LINES the diff emitted. An enum variant, a trait method or
    // a struct field removed below an unchanged `pub enum` / `pub trait` /
    // `pub struct` opener is a breaking change this scanner does not report:
    // the opener was never emitted as -/+, so nothing enters `removed_syms` to
    // pair at all. Closing it needs the item's body from BOTH commits, which a
    // diff-only scanner does not have; the fix is the repo-backed breaking
    // analysis planned for 0.8, not a deeper heuristic here. The limit was
    // reviewed and accepted deliberately — widening it here would trade a known
    // blind spot for guesses about text the scanner never saw.
    let mut added_used = vec![false; added_syms.len()];
    let mut unpaired_removed = Vec::new();

    for removed in &removed_syms {
        match find_pairable_addition(&added_syms, &added_used, removed, true) {
            Some(index) => {
                added_used[index] = true;
                drop_removal_finding(&mut findings, removed);
            }
            None => unpaired_removed.push(removed),
        }
    }

    for removed in unpaired_removed {
        let Some(index) = find_pairable_addition(&added_syms, &added_used, removed, false) else {
            continue;
        };
        added_used[index] = true;
        let added = &added_syms[index];

        // The removed-symbol finding is a false positive either way: drop it.
        drop_removal_finding(&mut findings, removed);

        // Compared on the comment-free identity, REPORTED verbatim: a reworded
        // comment is not a signature change, but a reader shown the change
        // should see the declaration as it is actually written.
        if added.identity != removed.identity {
            findings.push(BreakingFinding {
                file: removed.file.clone(),
                kind: BreakingKind::ChangedSignature {
                    before: removed.text.clone(),
                    after: added.text.clone(),
                },
                line: String::new(),
                risk_level: compute_breaking_risk(&removed.file),
            });
        }
    }

    findings
}

/// Index of the first not-yet-consumed addition that may pair with `removed`.
///
/// `require_identical_code` restricts the search to a declaration re-emitted
/// unchanged, which is what makes the two-pass pairing stable when several
/// declarations share (file, kind, name). "Unchanged" is judged on
/// [`SymbolDecl::identity`], so a re-emission that only reworded a comment is
/// still the exact match it looks like to a compiler.
fn find_pairable_addition(
    added_syms: &[SymbolDecl],
    added_used: &[bool],
    removed: &SymbolDecl,
    require_identical_code: bool,
) -> Option<usize> {
    added_syms.iter().enumerate().find_map(|(index, added)| {
        (!added_used[index]
            && added.file == removed.file
            && added.symbol_type == removed.symbol_type
            && added.name == removed.name
            && scopes_may_pair(&removed.scope, &added.scope)
            && cfgs_may_pair(&removed.cfg_guard, &added.cfg_guard)
            && (!require_identical_code || added.identity == removed.identity))
            .then_some(index)
    })
}

/// May a removal and an addition guarded by these `cfg` predicates be the same
/// declaration?
///
/// Two KNOWN predicates that differ never pair. `#[cfg(feature = "a")]
/// pub struct Config;` replaced by the same struct under feature `b` is an
/// exact text match, so the pairing dropped the removal — but `Config` really
/// did disappear for anyone building with feature `a`, which is precisely the
/// breaking change the report exists to name.
///
/// A guard is the WHOLE conjunction of the attributes above the declaration.
/// Keeping only the last one made `#[cfg(unix)] #[cfg(feature = "x")]` and
/// `#[cfg(windows)] #[cfg(feature = "x")]` compare equal on the shared feature
/// alone, so a removal that really happened on Unix paired with a Windows-only
/// re-add and vanished.
///
/// An unknown guard (`None`) pairs with anything, the same tolerance
/// [`scopes_may_pair`] gives an unseen module opener: the attribute may simply
/// sit on a context line this hunk did not re-emit on that side, and treating
/// "not shown" as "no cfg" would turn ordinary re-adds into phantom removals.
fn cfgs_may_pair(removed: &Option<Vec<String>>, added: &Option<Vec<String>>) -> bool {
    match (removed, added) {
        (Some(removed), Some(added)) => removed == added,
        _ => true,
    }
}

/// Does this line end the run of attributes standing above a declaration?
///
/// Attributes, doc comments and blank lines sit between a `cfg` and the item it
/// guards without breaking the link; anything else is a new item. The line
/// arrives with its comments already resolved away, so `/** … */` — the block
/// form of `///`, wrapped or not — reaches this as the blank line it is.
fn breaks_attribute_run(trimmed: &str) -> bool {
    !trimmed.is_empty() && !trimmed.starts_with("#[")
}

/// An attribute may wrap over this many lines before the tracker gives up on it.
///
/// A diff shows attributes the same way it shows everything else — partially. An
/// opener whose close never arrives would otherwise swallow the rest of the hunk
/// as continuation lines and keep a stale guard standing over declarations it
/// does not gate.
const MAX_ATTRIBUTE_CONTINUATION_LINES: usize = 32;

/// An attribute whose delimiters have not closed yet.
struct OpenAttribute {
    /// Everything read so far, whitespace removed.
    text: String,
    /// How many delimiters are still open.
    depth: usize,
    /// How many lines it has absorbed.
    lines: usize,
}

/// One diff side's `#[cfg(…)]` conjunction standing above the next declaration.
///
/// Attributes wrap. `#[cfg(any(` on its own line used to be recorded as the
/// whole predicate, and the very next line — `feature = "a",` — was then read as
/// a new item and cleared the guard: the declaration below it came out
/// unguarded, so a struct that really disappeared for one configuration paired
/// with its re-add under a different one and left no finding. An attribute is
/// therefore accumulated until its delimiters balance, and only the finished
/// text becomes a guard.
///
/// Whitespace is dropped so `#[cfg(feature="a")]`, `#[cfg(feature = "a")]` and
/// the same predicate wrapped across four lines are ONE predicate: reformatting
/// an attribute is not a different gate, and reading it as one would report a
/// removal that never happened.
///
/// Comments are resolved away before any of that, by one
/// [`SourceScanner`](crate::rust_source::SourceScanner) per side fed one
/// physical line at a time. A block comment is not syntax on either count: a
/// `/** … */` doc comment standing between the `cfg` and its item used to read
/// as a new item and clear the guard, and a `/* ))) */` inside a wrapped
/// predicate balanced the attribute early with the same result — both sides
/// unguarded, the identical declaration text paired, and a struct that really
/// left one configuration produced no finding. Literals are KEPT by that view,
/// because `#[cfg(feature = "a")]` and `#[cfg(feature = "b")]` are different
/// gates and a view that dropped literal bodies would make them one.
#[derive(Default)]
struct CfgGuard {
    /// The accumulated conjunction, or `None` for "not known on this side".
    guards: Option<Vec<String>>,
    open: Option<OpenAttribute>,
    /// Resolves comments away, carrying an open `/* … */` or literal between
    /// lines. Reset with the guard, because the diff has jumped elsewhere.
    scanner: crate::rust_source::SourceScanner,
}

impl CfgGuard {
    /// The conjunction currently standing above the next declaration.
    fn guard(&self) -> Option<&[String]> {
        self.guards.as_deref()
    }

    /// Forget everything: the diff has jumped somewhere else.
    fn reset(&mut self) {
        self.forget_attributes();
        self.scanner.reset();
    }

    /// Drop the guard and any attribute in flight, keeping the scanner state.
    ///
    /// Used when an attribute overruns its continuation cap: the diff has NOT
    /// jumped anywhere, so a literal or `/* … */` still open on this side is
    /// still open on the next line.
    fn forget_attributes(&mut self) {
        self.guards = None;
        self.open = None;
    }

    /// Advance this side past `trimmed`.
    ///
    /// Call it AFTER the line has been offered to the declaration accumulator: a
    /// declaration is guarded by the attribute above it, not by one on its own
    /// line.
    fn feed(&mut self, trimmed: &str) {
        let resolved = self.scanner.code_with_literals(trimmed);
        let trimmed = resolved.trim();
        if let Some(open) = self.open.as_mut() {
            open.text
                .extend(trimmed.chars().filter(|c| !c.is_whitespace()));
            open.depth = delimiter_depth(trimmed, open.depth);
            open.lines += 1;
            if open.depth == 0 {
                let finished = self.open.take().expect("open attribute").text;
                self.record(finished);
            } else if open.lines >= MAX_ATTRIBUTE_CONTINUATION_LINES {
                // Unknown pairs with anything, which is the tolerant direction:
                // a guard invented from an unfinished attribute would fabricate
                // removals out of ordinary re-adds.
                self.forget_attributes();
            }
            return;
        }

        if trimmed.starts_with("#[") {
            let text: String = trimmed.chars().filter(|c| !c.is_whitespace()).collect();
            let depth = delimiter_depth(trimmed, 0);
            if depth == 0 {
                self.record(text);
            } else {
                self.open = Some(OpenAttribute {
                    text,
                    depth,
                    lines: 1,
                });
            }
            return;
        }

        if breaks_attribute_run(trimmed) {
            self.guards = None;
        }
    }

    /// Add one finished attribute to the conjunction.
    ///
    /// Consecutive `#[cfg(…)]` attributes ACCUMULATE — stacking them is Rust's
    /// `AND` — and the accumulated set is sorted, because `#[cfg(a)] #[cfg(b)]`
    /// and `#[cfg(b)] #[cfg(a)]` gate the item identically and a reorder is not
    /// an API change. Any other attribute keeps the run alive but adds nothing:
    /// a `#[derive(…)]` between the `cfg` and its item does not change the gate.
    fn record(&mut self, attribute: String) {
        if !gates_the_item(&attribute) {
            return;
        }
        let guards = self.guards.get_or_insert_with(Vec::new);
        guards.push(attribute);
        guards.sort();
        guards.dedup();
    }
}

/// Does this whitespace-stripped attribute decide whether the item exists?
///
/// `#[cfg(…)]` obviously does. So does `#[cfg_attr(feature = "a", cfg(unix))]`:
/// it applies a `cfg` under a condition, so the item is gated just as surely —
/// reading only the literal `#[cfg(` spelling dropped BOTH sides' guards, the
/// identical declaration text then paired, and a struct that really left the
/// Unix build produced no finding at all.
///
/// The rest of the `cfg_attr` family — `#[cfg_attr(unix, derive(Debug))]`,
/// `#[cfg_attr(docsrs, doc(cfg(…)))]` — decides an attribute ON the item, not
/// the item, and must stay out: a gate invented there would split an ordinary
/// re-add into a phantom removal, which is the error direction that costs
/// trust. `,cfg(` is the whole distinction, applied to text whitespace has
/// already been stripped from, and it separates the two families exactly across
/// the 44,562 `cfg_attr` attributes in the local registry: 189 apply a `cfg`
/// (12 crates, the `portable-atomic` idiom), and in none of them does the
/// substring fall inside a string literal.
fn gates_the_item(attribute: &str) -> bool {
    attribute.starts_with("#[cfg(")
        || (attribute.starts_with("#[cfg_attr(") && attribute.contains(",cfg("))
}

/// How many delimiters `line` leaves open, starting from `depth`.
///
/// Delimiters inside a string literal are text, not structure: `#[doc = "a ("]`
/// closes on its own line. Escapes are honoured so a `\"` does not end the
/// string early.
///
/// Block comments never reach this counter at all: [`CfgGuard::feed`] resolves
/// them away before the line gets here, so `/* ))) */` inside a wrapped
/// `#[cfg(…)]` predicate can no longer balance the attribute early. The view it
/// uses keeps literals, which is why the `in_string` handling below is still
/// this function's own job.
fn delimiter_depth(line: &str, depth: usize) -> usize {
    let mut depth = depth;
    let mut in_string = false;
    let mut escaped = false;
    for c in line.chars() {
        if in_string {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        match c {
            '"' => in_string = true,
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    depth
}

/// Drop ONE removed-symbol finding matching `removed`.
///
/// One removal cancelled by one addition retires exactly one finding: two
/// identical `cfg`-gated removals against a single re-add must leave the second
/// one reported.
fn drop_removal_finding(findings: &mut Vec<BreakingFinding>, removed: &SymbolDecl) {
    let matching = findings.iter().position(|f| {
        f.file == removed.file
            && matches!(
                &f.kind,
                BreakingKind::RemovedSymbol { symbol_type } if *symbol_type == removed.symbol_type
            )
            && f.line == removed.text
    });
    if let Some(index) = matching {
        findings.remove(index);
    }
}

/// A declaration still absorbing continuation lines.
///
/// `decl.text` is the verbatim join used for identity and for the
/// `BREAKING_CHANGES.md` row. Completeness is decided on `code` instead, which
/// is the same lines read through a [`SourceScanner`] — one call per PHYSICAL
/// line, which is what makes a `//` end where it really ends. Joining first and
/// scanning the result once cannot: the join has no line breaks, so a comment on
/// any continuation line commented out every line appended after it, the closing
/// `)` and body `{` were never seen, and the accumulator ran on into the body
/// until the cap — turning a body-only rewrite into a phantom signature change.
///
/// The scanner is what keeps the other direction right too: it carries an open
/// literal or `/* … */` from one continuation line to the next, so a brace
/// inside a multi-line string still does not end the declaration.
struct PendingDecl {
    decl: SymbolDecl,
    code: String,
    /// Feeds `code`: literal bodies dropped, because this view counts delimiters.
    completeness: crate::rust_source::SourceScanner,
    /// Feeds `decl.identity`: literals kept, because this view compares source.
    /// Two scanners rather than one because the two views want different output
    /// from the same resolution; both see the same lines in the same order, so
    /// their carried state never diverges.
    identity: crate::rust_source::SourceScanner,
}

/// Start or continue accumulating a public declaration on one diff side.
///
/// A declaration in progress absorbs `trimmed` as a continuation line; otherwise
/// `trimmed` may open a new one. Either way the declaration is emitted as soon
/// as it is complete (or once it has absorbed [`MAX_DECL_CONTINUATION_LINES`]).
fn accumulate_decl(
    pending: &mut Option<PendingDecl>,
    collected: &mut Vec<SymbolDecl>,
    findings: &mut Vec<BreakingFinding>,
    trimmed: &str,
    site: &DeclSite<'_>,
) {
    if let Some(open) = pending.as_mut() {
        if !open.decl.text.ends_with('(') && !trimmed.is_empty() {
            open.decl.text.push(' ');
        }
        open.decl.text.push_str(trimmed);
        open.push_code(trimmed);
        open.decl.continuation_lines += 1;
        if declaration_complete(&open.code)
            || open.decl.continuation_lines >= MAX_DECL_CONTINUATION_LINES
        {
            finalize_decl(pending, collected, findings);
        }
        return;
    }

    let Some((symbol_type, name)) = classify_pub_declaration(trimmed) else {
        return;
    };
    let decl = SymbolDecl {
        file: site.file.to_string(),
        symbol_type: symbol_type.to_string(),
        name,
        text: trimmed.to_string(),
        identity: String::new(),
        scope: site.scope.path(),
        cfg_guard: site.cfg_guard.map(<[String]>::to_vec),
        side: site.side,
        continuation_lines: 0,
    };
    let mut open = PendingDecl {
        decl,
        code: String::new(),
        completeness: crate::rust_source::SourceScanner::default(),
        identity: crate::rust_source::SourceScanner::default(),
    };
    open.push_code(trimmed);
    if declaration_complete(&open.code) {
        emit_decl(open.decl, collected, findings);
    } else {
        *pending = Some(open);
    }
}

impl PendingDecl {
    /// Read one more physical line into both derived views.
    fn push_code(&mut self, trimmed: &str) {
        self.code.push_str(&self.completeness.code_only(trimmed));
        // The line ended: whatever the scanner still carries is a literal or a
        // block comment, never a line comment.
        self.code.push(' ');

        // Read BEFORE this line is scanned: a literal the PREVIOUS line left
        // open makes the break between the two part of the value rather than
        // layout.
        let continues_literal = self.identity.carries_literal();

        // A line that is nothing but a comment contributes nothing to the
        // identity, and a line whose code ends where its comment begins must not
        // contribute the whitespace between them either — otherwise `a: u8, //x`
        // and `a: u8,// y` would read as different declarations. Inside a
        // literal there is no comment to strip and a blank line is a blank line
        // in the value, so it is kept.
        let line = self.identity.code_with_literals(trimmed);
        let line = line.trim();
        if line.is_empty() && !continues_literal {
            return;
        }
        if !self.decl.identity.is_empty() {
            // Physical boundaries are preserved only where they are part of the
            // value. Joining a literal's lines with a space made a constant
            // written across two lines compare equal to the same constant
            // rewritten with a space in it, so a changed public value paired
            // away as an unchanged re-add. Everywhere else the break is layout:
            // preserving it there made `pub type Alias =` + `u32;` a different
            // declaration from `pub type Alias = u32;`, and a purely cosmetic
            // reflow was reported as a changed signature — with an identical
            // "before" and "after", since those are joined with a space.
            self.decl
                .identity
                .push(if continues_literal { '\n' } else { ' ' });
        }
        self.decl.identity.push_str(line);
    }
}

/// Emit a declaration that is no longer accumulating, if any.
fn finalize_decl(
    pending: &mut Option<PendingDecl>,
    collected: &mut Vec<SymbolDecl>,
    findings: &mut Vec<BreakingFinding>,
) {
    if let Some(open) = pending.take() {
        emit_decl(open.decl, collected, findings);
    }
}

/// Record a finished declaration; removals also become a `RemovedSymbol`
/// finding, which the pairing pass may later drop or upgrade.
fn emit_decl(
    decl: SymbolDecl,
    collected: &mut Vec<SymbolDecl>,
    findings: &mut Vec<BreakingFinding>,
) {
    if decl.side == DiffSide::Removed {
        findings.push(BreakingFinding {
            file: decl.file.clone(),
            kind: BreakingKind::RemovedSymbol {
                symbol_type: decl.symbol_type.clone(),
            },
            line: decl.text.clone(),
            risk_level: compute_breaking_risk(&decl.file),
        });
    }
    collected.push(decl);
}

fn should_scan_for_breaking_changes(path: &str) -> bool {
    matches!(classify_review_file(path), ReviewFileCategory::Code)
}

/// Has this (possibly partial) public declaration reached its end?
///
/// A declaration is complete once its parens are balanced and it has reached the
/// body opener `{` or a `;` (trait method, type alias, const, static). Used to
/// decide whether to keep accumulating continuation lines so both "Before" and
/// "After" are full declarations (BUG-4 / TOOLING-15).
///
/// Only real delimiters count, and `code` has already had them resolved: it is
/// the declaration's lines read through the pending declaration's own
/// [`SourceScanner`], one call per physical line. `pub const TEMPLATE: &str =
/// r#"{` opens a multi-line literal, and reading that `{` as the body opener
/// finalized a TRUNCATED declaration — identical on both diff sides, so the
/// removal was cancelled and the literal change the patch actually made went
/// unreported. The scanner carries that literal across the continuation lines,
/// and ends a `//` comment at the line that wrote it.
///
/// A `{` in TYPE position is not a body opener either. `pub type Alias =
/// Buffer<{` states a const argument, and finalizing there truncated both diff
/// sides to the same prefix: they paired as an unchanged re-add and a changed
/// const expression — a different public type — produced no finding.
fn declaration_complete(code: &str) -> bool {
    let mut depth: i32 = 0;
    // How deep inside a generic argument list the scan is. A `{` opened there —
    // `Buffer<{ LIMIT * 2 }>`, `Buffer<u8, { LIMIT * 2 }>` — is type-level
    // syntax, not the item's body opener.
    let mut angle: i32 = 0;
    // How deep inside a brace that is NOT the item's body the scan is: a const
    // argument, or an initializer block after a top-level `=`.
    let mut block: i32 = 0;
    // Has a top-level `=` been seen? After one, the item states a VALUE and
    // runs to its `;` — every `{` from there on opens the initializer, never
    // the body of a struct, enum, trait or function.
    let mut initializes = false;
    let mut prev = '\0';
    let mut chars = code.chars().peekable();
    while let Some(ch) = chars.next() {
        // `<<` is the shift operator, never a nesting of two argument lists in
        // any declaration this scanner sees. Consuming both characters is what
        // keeps the 4,666 public `const`/`static` declarations in the local
        // registry that state a shift on their own line terminating at their
        // `;` instead of accumulating past it.
        if ch == '<' && chars.peek() == Some(&'<') {
            chars.next();
            prev = '<';
            continue;
        }
        match ch {
            // `<` opens an argument list only directly after an identifier or a
            // closing `>` — `Buffer<`, `Vec<u8>>` — which is where a type names
            // its arguments and is not where a comparison puts it.
            '<' if prev.is_alphanumeric() || prev == '_' || prev == '>' => angle += 1,
            // `->` is a return arrow, not a closing bracket.
            '>' if prev != '-' && angle > 0 => angle -= 1,
            // Only a top-level `=` is an initializer. Inside a generic argument
            // list it states a default (`struct Foo<const N: usize = 4>`) or an
            // associated type (`impl Iterator<Item = u8>`), and both of those
            // are followed by a body brace that must still end the declaration.
            // `==`, `=>` and the compound assignments are not initializers either.
            '=' if depth <= 0
                && angle == 0
                && block == 0
                && !matches!(chars.peek(), Some(&('=' | '>')))
                && !"=!<>+-*/%&|^".contains(prev) =>
            {
                initializes = true
            }
            // Tracking the whole argument list rather than the exact `<{`
            // sequence is what catches a const argument that is not the FIRST
            // one, where the `{` follows a comma. Measured against the local
            // registry (59,946 files, 2,025 crates, 4,354,142 public
            // declaration lines), the two rules judge zero lines differently.
            '{' if angle > 0 || block > 0 || initializes => block += 1,
            '}' if block > 0 => block -= 1,
            // Square brackets are counted for the same reason parentheses are:
            // an array type states its length with a `;` — `pub const TABLE:
            // [u8; 2] = [` — and reading that as the terminator finalized the
            // declaration at its opener. Both sides of a diff then held the same
            // opener text, paired as an unchanged re-add, and a changed
            // initializer below produced no finding at all.
            '(' | '[' => depth += 1,
            ')' | ']' => depth -= 1,
            // A `;` inside an initializer block is a statement terminator, not
            // the declaration's.
            '{' | ';' if depth <= 0 && block == 0 => return true,
            _ => {}
        }
        if !ch.is_whitespace() {
            prev = ch;
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

/// Grouping identity of a `ChangedSignature` row: `(file, symbol kind, name)`.
/// The kind separates namespaces that may legally share an identifier.
type ChangedSignatureKey = (String, &'static str, String);

/// Format breaking changes as markdown.
/// Make one value safe to drop into a markdown table cell.
///
/// A table row is delimited by `|`, and Rust states bitwise or, patterns and
/// closures with the same character: `pub const MASK: u32 = READ | WRITE;` in a
/// cell opened two new columns and the row rendered as garbage. GitHub's table
/// parser splits on UNESCAPED pipes before any inline markup runs, so `\|` is
/// the escape even inside a code span.
fn escape_table_cell(text: &str) -> std::borrow::Cow<'_, str> {
    if text.contains('|') {
        std::borrow::Cow::Owned(text.replace('|', r"\|"))
    } else {
        std::borrow::Cow::Borrowed(text)
    }
}

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
                let _ = writeln!(
                    md,
                    "| {} | `{}` | {} |",
                    escape_table_cell(&f.file),
                    escape_table_cell(&f.line),
                    symbol_type
                );
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
                let _ = writeln!(
                    md,
                    "| {} | `{}` | {} |",
                    escape_table_cell(&f.file),
                    escape_table_cell(&f.line),
                    symbol_type
                );
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
        // (file, symbol kind, name), render one row, and note the variant count
        // (BUG-4 / TOOLING-15). The kind is part of the key because non-fn
        // declarations also land here: `pub struct Limit` and `pub const Limit`
        // live in different namespaces, so sharing an identifier must not
        // collapse them into a single row.
        let mut order: Vec<ChangedSignatureKey> = Vec::new();
        let mut groups: std::collections::HashMap<ChangedSignatureKey, Vec<(&String, &String)>> =
            std::collections::HashMap::new();
        for f in &changed {
            if let BreakingKind::ChangedSignature { before, after } = &f.kind {
                let name = extract_fn_name(before)
                    .or_else(|| extract_fn_name(after))
                    .or_else(|| symbol_name(before))
                    .unwrap_or_else(|| before.clone());
                let kind = symbol_kind(before)
                    .or_else(|| symbol_kind(after))
                    .unwrap_or("");
                let key = (f.file.clone(), kind, name);
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
                    escape_table_cell(&key.0),
                    escape_table_cell(before),
                    escape_table_cell(after),
                    variants.len() - 1,
                    if variants.len() - 1 == 1 { "" } else { "s" }
                );
            } else {
                let _ = writeln!(
                    md,
                    "| {} | `{}` | `{}` |",
                    escape_table_cell(&key.0),
                    escape_table_cell(before),
                    escape_table_cell(after)
                );
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
                let _ = writeln!(
                    md,
                    "| {} | `{}` |",
                    escape_table_cell(&f.file),
                    escape_table_cell(variable)
                );
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
    fn a_pipe_in_a_declaration_does_not_break_the_table_columns() {
        // Declaration text goes into a markdown table, and Rust states bitwise
        // or, patterns and closures with `|`. Written verbatim it opened new
        // columns, so every row carrying one rendered as garbage — the report
        // stopped being readable exactly where the declaration was interesting.
        let findings = analyze_all_breaking_changes(&[one_file_patch(
            "src/limits.rs",
            &[
                "-pub const MASK: u32 = READ | WRITE;",
                "+pub const MASK: u32 = READ | WRITE | EXEC;",
            ],
        )]);

        let md = format_breaking_changes(&findings);
        let row = md
            .lines()
            .find(|l| l.contains("MASK"))
            .expect("the changed constant is reported");
        assert!(
            row.contains(r"\|"),
            "the pipe must be escaped for the table: {row}"
        );
        // A GitHub table row is `| a | b | c |`: splitting on UNESCAPED pipes
        // leaves the two empty ends plus one field per column.
        let cells = row.replace(r"\|", "\u{0}").split('|').count();
        assert_eq!(cells, 5, "three columns, two empty ends: {row}");
    }

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
    fn duplicate_declarations_pair_one_to_one() {
        // cfg-gated variants share (file, kind, name). Pairing scanned the added
        // side without consuming the match, so every removal cancelled against
        // the SAME unchanged addition: the real `u32 -> u64` change paired with
        // nothing and vanished, and the widening went unreported.
        let findings = analyze_all_breaking_changes(&[one_file_patch(
            "src/model.rs",
            &[
                "-pub type Value = u32;",
                "-pub type Value = u32;",
                "+pub type Value = u32;",
                "+pub type Value = u64;",
            ],
        )]);

        let changes: Vec<(&String, &String)> = findings
            .iter()
            .filter_map(|f| match &f.kind {
                BreakingKind::ChangedSignature { before, after } => Some((before, after)),
                _ => None,
            })
            .collect();
        assert_eq!(
            changes.len(),
            1,
            "the second removal must pair with the leftover addition: {:?}",
            findings.iter().map(|f| &f.kind).collect::<Vec<_>>()
        );
        assert_eq!(changes[0].0, "pub type Value = u32;");
        assert_eq!(changes[0].1, "pub type Value = u64;");
    }

    #[test]
    fn a_const_block_in_a_generic_argument_is_not_the_body_opener() {
        // `pub type Alias = Buffer<{` opens a const argument, not an item body.
        // Finalizing there truncated BOTH sides to the same prefix, they paired
        // as an unchanged re-add, and the changed const expression below —
        // a different public type — produced no finding at all.
        let findings = analyze_all_breaking_changes(&[one_file_patch(
            "src/model.rs",
            &[
                "-pub type Alias = Buffer<{",
                "-    LIMIT * 2",
                "-}>;",
                "+pub type Alias = Buffer<{",
                "+    LIMIT * 3",
                "+}>;",
            ],
        )]);

        let changes: Vec<(&String, &String)> = findings
            .iter()
            .filter_map(|f| match &f.kind {
                BreakingKind::ChangedSignature { before, after } => Some((before, after)),
                _ => None,
            })
            .collect();
        assert_eq!(
            changes.len(),
            1,
            "a changed const argument is a changed public type: {:?}",
            findings.iter().map(|f| &f.kind).collect::<Vec<_>>()
        );
        assert!(changes[0].0.contains("LIMIT * 2"), "{}", changes[0].0);
        assert!(changes[0].1.contains("LIMIT * 3"), "{}", changes[0].1);
    }

    #[test]
    fn a_const_block_in_a_later_generic_argument_is_not_the_body_opener_either() {
        // The same construct one argument along: `Buffer<1, {` opens a const
        // argument whose `{` follows a comma, not a `<`. A const generic is
        // rarely the FIRST argument, so this is the shape the previous rule's
        // exact `<{` sequence missed while covering the rarer one.
        let findings = analyze_all_breaking_changes(&[one_file_patch(
            "src/model.rs",
            &[
                "-pub type Alias = Buffer<u8, {",
                "-    LIMIT * 2",
                "-}>;",
                "+pub type Alias = Buffer<u8, {",
                "+    LIMIT * 3",
                "+}>;",
            ],
        )]);

        let changes: Vec<(&String, &String)> = findings
            .iter()
            .filter_map(|f| match &f.kind {
                BreakingKind::ChangedSignature { before, after } => Some((before, after)),
                _ => None,
            })
            .collect();
        assert_eq!(
            changes.len(),
            1,
            "a changed const argument is a changed public type: {:?}",
            findings.iter().map(|f| &f.kind).collect::<Vec<_>>()
        );
        assert!(changes[0].0.contains("LIMIT * 2"), "{}", changes[0].0);
        assert!(changes[0].1.contains("LIMIT * 3"), "{}", changes[0].1);
    }

    #[test]
    fn a_block_initializer_is_not_the_body_opener() {
        // `pub const LIMIT: usize = {` opens an initializer, not an item body:
        // the declaration runs to the `;` after the block's `}`. Finalizing at
        // the `{` truncated both diff sides to the same first line, they paired
        // as an unchanged re-add, and the changed expression inside the block
        // produced no finding at all.
        let findings = analyze_all_breaking_changes(&[one_file_patch(
            "src/limits.rs",
            &[
                "-pub const LIMIT: usize = {",
                "-    let base = 2;",
                "-    base * 3",
                "-};",
                "+pub const LIMIT: usize = {",
                "+    let base = 2;",
                "+    base * 4",
                "+};",
            ],
        )]);

        let changes: Vec<(&String, &String)> = findings
            .iter()
            .filter_map(|f| match &f.kind {
                BreakingKind::ChangedSignature { before, after } => Some((before, after)),
                _ => None,
            })
            .collect();
        assert_eq!(
            changes.len(),
            1,
            "a changed block-valued constant is a changed public value: {:?}",
            findings.iter().map(|f| &f.kind).collect::<Vec<_>>()
        );
        assert!(changes[0].0.contains("base * 3"), "{}", changes[0].0);
        assert!(changes[0].1.contains("base * 4"), "{}", changes[0].1);
    }

    #[test]
    fn a_body_brace_after_a_const_argument_still_ends_the_declaration() {
        // Guard against over-reach: the const block closes on the same line and
        // the NEXT brace is the real body. Swallowing it would run the
        // accumulator into the body and turn a body-only rewrite into a phantom
        // signature change.
        let findings = analyze_all_breaking_changes(&[one_file_patch(
            "src/model.rs",
            &[
                "-pub fn to_hex(&self) -> ArrayString<{ 2 * OUT_LEN }> {",
                "-    self.old_body()",
                "-}",
                "+pub fn to_hex(&self) -> ArrayString<{ 2 * OUT_LEN }> {",
                "+    self.new_body()",
                "+}",
            ],
        )]);

        assert!(
            !findings.iter().any(|f| matches!(
                &f.kind,
                BreakingKind::RemovedSymbol { .. } | BreakingKind::ChangedSignature { .. }
            )),
            "a body-only rewrite is not a signature change: {:?}",
            findings.iter().map(|f| &f.kind).collect::<Vec<_>>()
        );
    }

    #[test]
    fn a_shifted_constant_still_terminates_at_its_semicolon() {
        // `1 << 3` is why the argument-list tracker consumes `<<` whole: 4,666
        // public `const`/`static` declarations in the local registry state a
        // shift on their own line, and a `<` counted as an opener there would
        // leave every one of them accumulating past its `;`.
        let findings = analyze_all_breaking_changes(&[one_file_patch(
            "src/model.rs",
            &[
                "-pub const MASK: u32 = 1 << 3;",
                "-pub struct Keep;",
                "+pub const MASK: u32 = 1 << 4;",
                "+pub struct Keep;",
            ],
        )]);

        let changes: Vec<(&String, &String)> = findings
            .iter()
            .filter_map(|f| match &f.kind {
                BreakingKind::ChangedSignature { before, after } => Some((before, after)),
                _ => None,
            })
            .collect();
        assert_eq!(
            changes.len(),
            1,
            "the constant is one declaration, the struct below another: {:?}",
            findings.iter().map(|f| &f.kind).collect::<Vec<_>>()
        );
        assert_eq!(changes[0].0, "pub const MASK: u32 = 1 << 3;");
        assert_eq!(changes[0].1, "pub const MASK: u32 = 1 << 4;");
    }

    #[test]
    fn unpaired_duplicate_removal_stays_a_removal() {
        // Two removals, one addition: one removal is genuinely gone. Consuming
        // the addition must leave the second removal reported, not silently
        // cancelled by an addition already spent on the first.
        let findings = analyze_all_breaking_changes(&[one_file_patch(
            "src/model.rs",
            &[
                "-pub type Value = u32;",
                "-pub type Value = u16;",
                "+pub type Value = u32;",
            ],
        )]);

        assert_eq!(
            removed_symbol_types(&findings),
            vec!["type alias".to_string()],
            "exactly one removal survives: {:?}",
            findings.iter().map(|f| &f.kind).collect::<Vec<_>>()
        );
        assert!(
            !findings
                .iter()
                .any(|f| matches!(&f.kind, BreakingKind::ChangedSignature { .. })),
            "the identical pair is a no-op, not a signature change: {:?}",
            findings.iter().map(|f| &f.kind).collect::<Vec<_>>()
        );
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

    #[test]
    fn changed_signatures_of_different_kinds_do_not_collapse() {
        // Once non-fn declarations produce ChangedSignature findings, a name can
        // repeat across namespaces in one file (`pub struct Limit` +
        // `pub const Limit`). Grouping by (file, name) alone rendered one of
        // them as a "feature-gated variant" of the other and dropped its row.
        let patch = one_file_patch(
            "src/model.rs",
            &[
                "-pub struct Limit {",
                "+pub struct Limit<T> {",
                "-pub const Limit: usize = 8;",
                "+pub const Limit: usize = 16;",
            ],
        );

        let findings = analyze_all_breaking_changes(&[patch]);
        let md = format_breaking_changes(&findings);

        assert!(
            md.contains("pub struct Limit<T> {"),
            "struct change must have its own row, got:\n{md}"
        );
        assert!(
            md.contains("pub const Limit: usize = 16;"),
            "const change must not be collapsed into the struct row, got:\n{md}"
        );
        assert!(
            !md.contains("variant"),
            "two different symbol kinds are not feature-gated variants, got:\n{md}"
        );
    }

    #[test]
    fn same_name_in_different_inline_modules_is_not_a_no_op_pair() {
        // `a::Config` is deleted while a same-named `b::Config` is added in the
        // same file. Pairing on (file, kind, name) alone cancelled a genuine
        // removal: the `a::Config` path is gone for every downstream consumer.
        let patch = one_file_patch(
            "src/model.rs",
            &[
                " pub mod a {",
                "-    pub struct Config {",
                "-        pub x: u32,",
                "-    }",
                " }",
                " pub mod b {",
                "+    pub struct Config {",
                "+        pub x: u32,",
                "+    }",
                " }",
            ],
        );

        let findings = analyze_all_breaking_changes(&[patch]);
        assert!(
            removed_symbol_types(&findings).contains(&"struct".to_string()),
            "removal from mod a must survive an unrelated add in mod b, got: {:?}",
            findings.iter().map(|f| &f.kind).collect::<Vec<_>>()
        );
    }

    #[test]
    fn raw_identifier_modules_are_different_scopes() {
        // A module may be named with a keyword through a raw identifier. The
        // scope parser stopped at the `#`, so `r#type` and `r#match` were both
        // recorded as `r`: two different namespaces looked like one, and the
        // removal of `r#type::Config` was cancelled by the unrelated addition of
        // `r#match::Config`.
        let patch = one_file_patch(
            "src/model.rs",
            &[
                " pub mod r#type {",
                "-    pub struct Config {",
                "-        pub x: u32,",
                "-    }",
                " }",
                " pub mod r#match {",
                "+    pub struct Config {",
                "+        pub x: u32,",
                "+    }",
                " }",
            ],
        );

        let findings = analyze_all_breaking_changes(&[patch]);
        assert!(
            removed_symbol_types(&findings).contains(&"struct".to_string()),
            "removal from mod r#type must survive an add in mod r#match, got: {:?}",
            findings.iter().map(|f| &f.kind).collect::<Vec<_>>()
        );
    }

    #[test]
    fn literal_and_comment_braces_do_not_pop_the_module_scope() {
        // A brace inside a literal or a comment is data. Counting it popped
        // `mod a` early, so the removal of `a::Config` carried an unknown scope
        // and paired with the unrelated addition of `b::Config` — the real API
        // removal disappeared from the report.
        let patch = one_file_patch(
            "src/model.rs",
            &[
                " pub mod a {",
                " const CLOSE: &str = \"}\";",
                " // trailing brace in a comment }",
                "-    pub struct Config {",
                "-        pub x: u32,",
                "-    }",
                " }",
                " pub mod b {",
                "+    pub struct Config {",
                "+        pub x: u32,",
                "+    }",
                " }",
            ],
        );

        let findings = analyze_all_breaking_changes(&[patch]);
        assert!(
            removed_symbol_types(&findings).contains(&"struct".to_string()),
            "a brace in a literal or comment must not merge two module scopes, got: {:?}",
            findings.iter().map(|f| &f.kind).collect::<Vec<_>>()
        );
    }

    #[test]
    fn a_literal_spanning_lines_does_not_pop_the_module_scope() {
        // Same defect, one line further on: the literal OPENS on one line and
        // closes on the next, so the tail of its body reached the tracker as
        // code and its `}` popped `mod a`. The removal of `a::Config` then
        // carried an unknown scope, paired with the unrelated `b::Config`
        // addition, and the real API removal vanished from the report. Multi-
        // line literals are not exotic here: 241 of them live in this tree and
        // 168 carry a brace in their body.
        let patch = one_file_patch(
            "src/model.rs",
            &[
                " pub mod a {",
                " const TEMPLATE: &str = \"opens {",
                " closes } here\";",
                "-    pub struct Config {",
                "-        pub x: u32,",
                "-    }",
                " }",
                " pub mod b {",
                "+    pub struct Config {",
                "+        pub x: u32,",
                "+    }",
                " }",
            ],
        );

        let findings = analyze_all_breaking_changes(&[patch]);
        assert!(
            removed_symbol_types(&findings).contains(&"struct".to_string()),
            "a brace inside a multi-line literal must not merge two module scopes, got: {:?}",
            findings.iter().map(|f| &f.kind).collect::<Vec<_>>()
        );
    }

    #[test]
    fn literal_brace_does_not_invent_a_module_scope() {
        // The mirror direction: an unmatched `{` in a literal used to deepen the
        // tracked scope, so a later removal and addition in the SAME module
        // looked like two different namespaces and a plain no-op re-add was
        // reported as a breaking removal.
        let patch = one_file_patch(
            "src/model.rs",
            &[
                " pub mod a {",
                " const OPEN: &str = \"{\";",
                "-    pub struct Config {",
                "+    pub struct Config {",
                " }",
            ],
        );

        let findings = analyze_all_breaking_changes(&[patch]);
        assert!(
            !findings.iter().any(|f| matches!(
                &f.kind,
                BreakingKind::RemovedSymbol { .. } | BreakingKind::ChangedSignature { .. }
            )),
            "an identical re-add in one module is not breaking, got: {:?}",
            findings.iter().map(|f| &f.kind).collect::<Vec<_>>()
        );
    }

    #[test]
    fn a_literal_delimiter_does_not_end_a_declaration_early() {
        // `pub const T: &str = r#"{` carries a `{` inside the literal it opens.
        // Reading it as the declaration's body opener finalized a TRUNCATED
        // declaration on both sides, so the two truncations matched exactly,
        // the removal was cancelled, and the changed literal — the whole point
        // of the diff — produced no finding at all.
        let patch = one_file_patch(
            "src/model.rs",
            &[
                "-pub const TEMPLATE: &str = r#\"{",
                "-  \"kind\": \"old\"",
                "-}\"#;",
                "+pub const TEMPLATE: &str = r#\"{",
                "+  \"kind\": \"new\"",
                "+}\"#;",
            ],
        );

        let findings = analyze_all_breaking_changes(&[patch]);
        assert!(
            findings
                .iter()
                .any(|f| matches!(&f.kind, BreakingKind::ChangedSignature { .. })),
            "a changed multi-line const body must be reported, got: {:?}",
            findings.iter().map(|f| &f.kind).collect::<Vec<_>>()
        );
    }

    #[test]
    fn a_declaration_ending_inside_a_literal_still_completes_at_the_real_end() {
        // Guard the other direction: blanking literals must not make an
        // ordinary single-line declaration look unfinished and swallow the
        // lines after it as continuations.
        let patch = one_file_patch(
            "src/model.rs",
            &[
                "-pub const SEP: &str = \";\";",
                "+pub const SEP: &str = \",\";",
                " pub fn untouched() {}",
            ],
        );

        let findings = analyze_all_breaking_changes(&[patch]);
        let changed: Vec<_> = findings
            .iter()
            .filter_map(|f| match &f.kind {
                BreakingKind::ChangedSignature { before, after } => Some((before, after)),
                _ => None,
            })
            .collect();
        assert_eq!(
            changed.len(),
            1,
            "exactly one const changed, got: {:?}",
            findings.iter().map(|f| &f.kind).collect::<Vec<_>>()
        );
        assert_eq!(changed[0].0, "pub const SEP: &str = \";\";");
        assert_eq!(changed[0].1, "pub const SEP: &str = \",\";");
    }

    #[test]
    fn a_removal_is_not_cancelled_by_a_re_add_under_a_different_cfg() {
        // `#[cfg(feature = "a")] pub struct Config;` replaced by the same
        // struct under feature `b` is an exact text match, so the pairing
        // dropped the removal — but `Config` really did disappear for anyone
        // building with feature `a`.
        let patch = one_file_patch(
            "src/model.rs",
            &[
                "-#[cfg(feature = \"a\")]",
                "-pub struct Config;",
                "+#[cfg(feature = \"b\")]",
                "+pub struct Config;",
            ],
        );

        let findings = analyze_all_breaking_changes(&[patch]);
        assert!(
            removed_symbol_types(&findings).contains(&"struct".to_string()),
            "a re-add under a different cfg must not cancel the removal, got: {:?}",
            findings.iter().map(|f| &f.kind).collect::<Vec<_>>()
        );
    }

    #[test]
    fn a_re_add_under_the_same_cfg_still_cancels() {
        // Guard against over-reach: the same guard on both sides is the no-op
        // remove+re-add the pairing exists for. Spelling differences in the
        // attribute are formatting, not a different predicate.
        let patch = one_file_patch(
            "src/model.rs",
            &[
                "-#[cfg(feature = \"a\")]",
                "-pub struct Config;",
                "+#[cfg(feature=\"a\")]",
                "+pub struct Config;",
            ],
        );

        let findings = analyze_all_breaking_changes(&[patch]);
        assert!(
            !findings.iter().any(|f| matches!(
                &f.kind,
                BreakingKind::RemovedSymbol { .. } | BreakingKind::ChangedSignature { .. }
            )),
            "an identical re-add under the same cfg is not breaking, got: {:?}",
            findings.iter().map(|f| &f.kind).collect::<Vec<_>>()
        );
    }

    #[test]
    fn a_stack_of_cfg_attributes_is_one_guard() {
        // Stacked attributes are Rust's AND. Keeping only the last one made
        // these two sides compare equal on the shared `feature = "x"` alone, so
        // a struct that really disappeared for Unix builds paired with its
        // Windows-only re-add and left no finding.
        let patch = one_file_patch(
            "src/model.rs",
            &[
                "-#[cfg(unix)]",
                "-#[cfg(feature = \"x\")]",
                "-pub struct Config;",
                "+#[cfg(windows)]",
                "+#[cfg(feature = \"x\")]",
                "+pub struct Config;",
            ],
        );

        let findings = analyze_all_breaking_changes(&[patch]);
        assert!(
            removed_symbol_types(&findings).contains(&"struct".to_string()),
            "a re-add under a different cfg stack must not cancel the removal, got: {:?}",
            findings.iter().map(|f| &f.kind).collect::<Vec<_>>()
        );
    }

    #[test]
    fn a_reordered_cfg_stack_is_the_same_guard() {
        // Guard the other direction: the conjunction is commutative, so moving
        // one attribute above another is formatting, not an API change.
        let patch = one_file_patch(
            "src/model.rs",
            &[
                "-#[cfg(unix)]",
                "-#[cfg(feature = \"x\")]",
                "-pub struct Config;",
                "+#[cfg(feature = \"x\")]",
                "+#[cfg(unix)]",
                "+pub struct Config;",
            ],
        );

        let findings = analyze_all_breaking_changes(&[patch]);
        assert!(
            !findings.iter().any(|f| matches!(
                &f.kind,
                BreakingKind::RemovedSymbol { .. } | BreakingKind::ChangedSignature { .. }
            )),
            "reordering a cfg stack is not breaking, got: {:?}",
            findings.iter().map(|f| &f.kind).collect::<Vec<_>>()
        );
    }

    #[test]
    fn a_multiline_cfg_predicate_still_guards_its_declaration() {
        // A predicate wrapped across lines used to record only its opener, and
        // the first continuation line then cleared the guard entirely. Both
        // sides read as unguarded, the identical struct text paired, and a
        // struct that really disappeared for `feature = "b"` builds left no
        // finding at all.
        let patch = one_file_patch(
            "src/model.rs",
            &[
                "-#[cfg(any(",
                "-    feature = \"a\",",
                "-    feature = \"b\"",
                "-))]",
                "-pub struct Config;",
                "+#[cfg(any(",
                "+    feature = \"a\",",
                "+    feature = \"c\"",
                "+))]",
                "+pub struct Config;",
            ],
        );

        let findings = analyze_all_breaking_changes(&[patch]);
        assert!(
            removed_symbol_types(&findings).contains(&"struct".to_string()),
            "a re-add under a different multiline cfg must not cancel the removal, got: {:?}",
            findings.iter().map(|f| &f.kind).collect::<Vec<_>>()
        );
    }

    #[test]
    fn a_rewrapped_cfg_predicate_is_the_same_guard() {
        // Guard the other direction: wrapping one predicate across lines is
        // formatting, exactly like the spacing inside it. The accumulated
        // attribute must compare equal to its single-line spelling, or every
        // `rustfmt` rewrap would report a removal that never happened.
        let patch = one_file_patch(
            "src/model.rs",
            &[
                "-#[cfg(any(feature = \"a\", feature = \"b\"))]",
                "-pub struct Config;",
                "+#[cfg(any(",
                "+    feature = \"a\",",
                "+    feature = \"b\"",
                "+))]",
                "+pub struct Config;",
            ],
        );

        let findings = analyze_all_breaking_changes(&[patch]);
        assert!(
            !findings.iter().any(|f| matches!(
                &f.kind,
                BreakingKind::RemovedSymbol { .. } | BreakingKind::ChangedSignature { .. }
            )),
            "rewrapping a cfg predicate is not breaking, got: {:?}",
            findings.iter().map(|f| &f.kind).collect::<Vec<_>>()
        );
    }

    #[test]
    fn a_multiline_attribute_does_not_drop_the_cfg_above_it() {
        // The same wrapping applies to any attribute standing between the
        // `cfg` and its item: a wrapped `#[derive(…)]` used to break the
        // attribute run on its continuation line and take the guard with it.
        let patch = one_file_patch(
            "src/model.rs",
            &[
                "-#[cfg(unix)]",
                "-#[derive(",
                "-    Debug,",
                "-)]",
                "-pub struct Config;",
                "+#[cfg(windows)]",
                "+#[derive(",
                "+    Debug,",
                "+)]",
                "+pub struct Config;",
            ],
        );

        let findings = analyze_all_breaking_changes(&[patch]);
        assert!(
            removed_symbol_types(&findings).contains(&"struct".to_string()),
            "a wrapped attribute must not drop the cfg above it, got: {:?}",
            findings.iter().map(|f| &f.kind).collect::<Vec<_>>()
        );
    }

    #[test]
    fn a_cfg_attr_that_applies_a_cfg_is_part_of_the_guard() {
        // `#[cfg_attr(feature = "a", cfg(unix))]` gates the item exactly like a
        // `#[cfg(…)]` does — it just decides, per feature, whether to apply one.
        // Recognizing only the literal `#[cfg(` spelling discarded both sides'
        // guards, so the identical struct text paired and the struct that really
        // disappeared from Unix builds with feature `a` left no finding.
        let patch = one_file_patch(
            "src/model.rs",
            &[
                "-#[cfg_attr(feature = \"a\", cfg(unix))]",
                "-pub struct Config;",
                "+#[cfg_attr(feature = \"a\", cfg(windows))]",
                "+pub struct Config;",
            ],
        );

        let findings = analyze_all_breaking_changes(&[patch]);
        assert!(
            removed_symbol_types(&findings).contains(&"struct".to_string()),
            "a re-add under a different cfg_attr guard must not cancel the removal, got: {:?}",
            findings.iter().map(|f| &f.kind).collect::<Vec<_>>()
        );
    }

    #[test]
    fn a_cfg_attr_that_applies_no_cfg_is_not_a_guard() {
        // Guard the other direction: `#[cfg_attr(unix, derive(Debug))]` decides
        // a derive, not whether the item exists. Reading it as a gate would
        // split an ordinary re-add into a phantom removal.
        let patch = one_file_patch(
            "src/model.rs",
            &[
                "-#[cfg_attr(unix, derive(Debug))]",
                "-pub struct Config;",
                "+#[cfg_attr(windows, derive(Debug))]",
                "+pub struct Config;",
            ],
        );

        let findings = analyze_all_breaking_changes(&[patch]);
        assert!(
            !findings.iter().any(|f| matches!(
                &f.kind,
                BreakingKind::RemovedSymbol { .. } | BreakingKind::ChangedSignature { .. }
            )),
            "a cfg_attr applying a derive is not a gate, got: {:?}",
            findings.iter().map(|f| &f.kind).collect::<Vec<_>>()
        );
    }

    #[test]
    fn an_unseen_cfg_guard_pairs_as_before() {
        // The attribute may sit on a context line the hunk never re-emitted on
        // one side. Unknown must pair with anything, exactly as an unseen
        // module opener does — inventing a mismatch there would turn every
        // ordinary re-add into a phantom removal.
        let patch = one_file_patch(
            "src/model.rs",
            &[
                " #[cfg(feature = \"a\")]",
                "-pub struct Config;",
                "+pub struct Config;",
            ],
        );

        let findings = analyze_all_breaking_changes(&[patch]);
        assert!(
            !findings.iter().any(|f| matches!(
                &f.kind,
                BreakingKind::RemovedSymbol { .. } | BreakingKind::ChangedSignature { .. }
            )),
            "an unchanged guard on a context line must not split the pair, got: {:?}",
            findings.iter().map(|f| &f.kind).collect::<Vec<_>>()
        );
    }

    #[test]
    fn a_block_comment_between_the_guard_and_its_item_does_not_drop_the_guard() {
        // `/** … */` is the block form of `///`, and it sits between a `cfg`
        // and the item it guards exactly as the line form does. Reading it as a
        // new item cleared BOTH sides' guards, the identical declaration text
        // then paired, and a struct that really left the `a` build produced no
        // finding at all.
        let patch = one_file_patch(
            "src/model.rs",
            &[
                "-#[cfg(feature = \"a\")]",
                "-/** Configuration for the a build. */",
                "-pub struct Config;",
                "+#[cfg(feature = \"b\")]",
                "+/** Configuration for the b build. */",
                "+pub struct Config;",
            ],
        );

        let findings = analyze_all_breaking_changes(&[patch]);
        assert!(
            removed_symbol_types(&findings).contains(&"struct".to_string()),
            "a block comment must not break the attribute run, got: {:?}",
            findings.iter().map(|f| &f.kind).collect::<Vec<_>>()
        );
    }

    #[test]
    fn a_multiline_block_comment_between_the_guard_and_its_item_does_not_drop_the_guard() {
        // The close arrives on a later line, so tolerating the OPENER alone
        // would leave the body and the `*/` line reading as new items — a fix
        // that looks complete and still drops the guard.
        let patch = one_file_patch(
            "src/model.rs",
            &[
                "-#[cfg(feature = \"a\")]",
                "-/**",
                "- * Configuration for the a build.",
                "- */",
                "-pub struct Config;",
                "+#[cfg(feature = \"b\")]",
                "+/**",
                "+ * Configuration for the b build.",
                "+ */",
                "+pub struct Config;",
            ],
        );

        let findings = analyze_all_breaking_changes(&[patch]);
        assert!(
            removed_symbol_types(&findings).contains(&"struct".to_string()),
            "a wrapped block comment must not break the attribute run, got: {:?}",
            findings.iter().map(|f| &f.kind).collect::<Vec<_>>()
        );
    }

    #[test]
    fn a_block_comment_inside_a_cfg_predicate_is_not_counted_as_syntax() {
        // The delimiter counter used to read `/* ) */` as a real closer, so the
        // attribute balanced early, its real continuation read as a new item,
        // and the guard was gone by the time the declaration arrived.
        let patch = one_file_patch(
            "src/model.rs",
            &[
                "-#[cfg(any(/* ))) */",
                "-    feature = \"a\",",
                "-    feature = \"c\"))]",
                "-pub struct Config;",
                "+#[cfg(any(/* ))) */",
                "+    feature = \"b\",",
                "+    feature = \"c\"))]",
                "+pub struct Config;",
            ],
        );

        let findings = analyze_all_breaking_changes(&[patch]);
        assert!(
            removed_symbol_types(&findings).contains(&"struct".to_string()),
            "a comment inside the predicate must not balance it early, got: {:?}",
            findings.iter().map(|f| &f.kind).collect::<Vec<_>>()
        );
    }

    #[test]
    fn same_name_in_the_same_inline_module_still_pairs() {
        // Guard against the module tracker over-reaching: a remove+re-add inside
        // ONE module is still the phantom-removal case it always was.
        let patch = one_file_patch(
            "src/model.rs",
            &[
                " pub mod a {",
                "-    pub struct Config {",
                "-        pub x: u32,",
                "+    pub struct Config {",
                "+        pub x: u64,",
                "     }",
                " }",
            ],
        );

        let findings = analyze_all_breaking_changes(&[patch]);
        assert!(
            !findings.iter().any(|f| matches!(
                &f.kind,
                BreakingKind::RemovedSymbol { .. } | BreakingKind::ChangedSignature { .. }
            )),
            "same-module remove+re-add must stay a no-op, got: {:?}",
            findings.iter().map(|f| &f.kind).collect::<Vec<_>>()
        );
    }

    #[test]
    fn multiline_non_fn_declaration_change_is_not_swallowed() {
        // Pairing compared only the opening line, so a bound change on a
        // continuation line vanished behind an identical `pub struct Config<`.
        let patch = one_file_patch(
            "src/model.rs",
            &[
                "-pub struct Config<",
                "-    T: Clone,",
                "-> {",
                "+pub struct Config<",
                "+    T: Clone + Send,",
                "+> {",
            ],
        );

        let findings = analyze_all_breaking_changes(&[patch]);
        assert!(
            findings.iter().any(|f| matches!(
                &f.kind,
                BreakingKind::ChangedSignature { before, after }
                    if before.contains("T: Clone,") && after.contains("T: Clone + Send,")
            )),
            "a changed bound on a continuation line must surface, got: {:?}",
            findings.iter().map(|f| &f.kind).collect::<Vec<_>>()
        );
        assert!(
            removed_symbol_types(&findings).is_empty(),
            "the change must not also report a removal, got: {:?}",
            removed_symbol_types(&findings)
        );
    }

    #[test]
    fn multiline_non_fn_declaration_reemitted_unchanged_is_not_breaking() {
        // Same accumulation, opposite direction: an unchanged multi-line
        // declaration re-emitted by the diff stays a no-op.
        let patch = one_file_patch(
            "src/model.rs",
            &[
                "-pub struct Config<",
                "-    T: Clone,",
                "-> {",
                "-    old_detail: u8,",
                "+pub struct Config<",
                "+    T: Clone,",
                "+> {",
                "+    new_detail: u8,",
            ],
        );

        let findings = analyze_all_breaking_changes(&[patch]);
        assert!(
            !findings.iter().any(|f| matches!(
                &f.kind,
                BreakingKind::RemovedSymbol { .. } | BreakingKind::ChangedSignature { .. }
            )),
            "identical multi-line remove+re-add must produce no finding, got: {:?}",
            findings.iter().map(|f| &f.kind).collect::<Vec<_>>()
        );
    }

    #[test]
    fn a_long_signature_change_below_the_old_cap_is_not_swallowed() {
        // Accumulation used to stop after eight continuation lines. Two long
        // declarations that agree on the opener and those eight lines then
        // finalized to the SAME truncated text, so the exact-match pass paired
        // them, consumed the addition and dropped the removal — the changed
        // return type on the tenth line produced no finding at all.
        let mut body = Vec::new();
        for (side, ret) in [("-", "u8"), ("+", "u16")] {
            body.push(format!("{side}pub fn build("));
            for name in ["a", "b", "c", "d", "e", "f", "g", "h"] {
                body.push(format!("{side}    {name}: u8,"));
            }
            body.push(format!("{side}) -> {ret} {{"));
        }
        let refs: Vec<&str> = body.iter().map(String::as_str).collect();
        let patch = one_file_patch("src/model.rs", &refs);

        let findings = analyze_all_breaking_changes(&[patch]);
        assert!(
            findings.iter().any(|f| matches!(
                &f.kind,
                BreakingKind::ChangedSignature { before, after }
                    if before.contains("-> u8") && after.contains("-> u16")
            )),
            "a return type changed past the eighth continuation line must surface, got: {:?}",
            findings.iter().map(|f| &f.kind).collect::<Vec<_>>()
        );
        assert!(
            removed_symbol_types(&findings).is_empty(),
            "the change must not also report a removal, got: {:?}",
            removed_symbol_types(&findings)
        );
    }

    #[test]
    fn a_trailing_comment_does_not_swallow_the_rest_of_a_declaration() {
        // Continuation lines are joined with a space, so a `//` on one of them
        // commented out everything appended after it: `declaration_complete`
        // never saw the closing `)` or the body `{`, the accumulator ran on
        // into the body, and a body-only rewrite came out as a phantom
        // signature change.
        let patch = one_file_patch(
            "src/model.rs",
            &[
                "-pub fn build(",
                "-    a: u8, // how many",
                "-    b: u8,",
                "-) -> u8 {",
                "-    old_body();",
                "+pub fn build(",
                "+    a: u8, // how many",
                "+    b: u8,",
                "+) -> u8 {",
                "+    new_body();",
            ],
        );

        let findings = analyze_all_breaking_changes(&[patch]);
        assert!(
            !findings.iter().any(|f| matches!(
                &f.kind,
                BreakingKind::RemovedSymbol { .. } | BreakingKind::ChangedSignature { .. }
            )),
            "a body-only rewrite under a commented signature is not breaking, got: {:?}",
            findings.iter().map(|f| &f.kind).collect::<Vec<_>>()
        );
    }

    #[test]
    fn a_real_change_below_a_trailing_comment_still_surfaces() {
        // The other direction: ending the comment at its own line must not cost
        // the change that follows it.
        let patch = one_file_patch(
            "src/model.rs",
            &[
                "-pub fn build(",
                "-    a: u8, // how many",
                "-    b: u8,",
                "-) -> u8 {",
                "+pub fn build(",
                "+    a: u8, // how many",
                "+    b: u16,",
                "+) -> u8 {",
            ],
        );

        let findings = analyze_all_breaking_changes(&[patch]);
        assert!(
            findings.iter().any(|f| matches!(
                &f.kind,
                BreakingKind::ChangedSignature { before, after }
                    if before.contains("b: u8") && after.contains("b: u16")
            )),
            "a parameter change below a trailing comment must surface, got: {:?}",
            findings.iter().map(|f| &f.kind).collect::<Vec<_>>()
        );
    }

    #[test]
    fn rewording_a_comment_inside_a_declaration_is_not_a_signature_change() {
        // The Rust API here is byte-identical; only a note to the next reader
        // changed. Comparing the verbatim join made that a `ChangedSignature`,
        // which is a breaking-change claim about text no consumer can observe.
        let patch = one_file_patch(
            "src/model.rs",
            &[
                "-pub fn build(",
                "-    a: u8, // how many",
                "-    b: u8,",
                "-) -> u8 {",
                "+pub fn build(",
                "+    a: u8, // how many of them",
                "+    b: u8,",
                "+) -> u8 {",
            ],
        );

        let findings = analyze_all_breaking_changes(&[patch]);
        assert!(
            !findings.iter().any(|f| matches!(
                &f.kind,
                BreakingKind::RemovedSymbol { .. } | BreakingKind::ChangedSignature { .. }
            )),
            "rewording a comment is not an API change, got: {:?}",
            findings.iter().map(|f| &f.kind).collect::<Vec<_>>()
        );
    }

    #[test]
    fn a_changed_multiline_array_constant_surfaces() {
        // `[u8; 2]` states a length with a `;`, inside the TYPE. Accepting that
        // `;` as the declaration's terminator finalized both sides at their
        // identical opener, the exact-match pass paired them, and the changed
        // values below vanished.
        let patch = one_file_patch(
            "src/model.rs",
            &[
                "-pub const TABLE: [u8; 2] = [",
                "-    1, 2,",
                "-];",
                "+pub const TABLE: [u8; 2] = [",
                "+    3, 4,",
                "+];",
            ],
        );

        let findings = analyze_all_breaking_changes(&[patch]);
        assert!(
            findings.iter().any(|f| matches!(
                &f.kind,
                BreakingKind::ChangedSignature { before, after }
                    if before.contains("1, 2") && after.contains("3, 4")
            )),
            "a changed multiline array constant must surface, got: {:?}",
            findings.iter().map(|f| &f.kind).collect::<Vec<_>>()
        );
    }

    #[test]
    fn a_reemitted_multiline_array_constant_is_still_a_no_op() {
        // The tolerant direction: reading the whole initializer must not turn a
        // verbatim re-emission into a removal.
        let patch = one_file_patch(
            "src/model.rs",
            &[
                "-pub const TABLE: [u8; 2] = [",
                "-    1, 2,",
                "-];",
                "+pub const TABLE: [u8; 2] = [",
                "+    1, 2,",
                "+];",
            ],
        );

        let findings = analyze_all_breaking_changes(&[patch]);
        assert!(
            !findings.iter().any(|f| matches!(
                &f.kind,
                BreakingKind::RemovedSymbol { .. } | BreakingKind::ChangedSignature { .. }
            )),
            "an unchanged re-emission is not breaking, got: {:?}",
            findings.iter().map(|f| &f.kind).collect::<Vec<_>>()
        );
    }

    #[test]
    fn a_newline_inside_a_literal_is_not_the_same_value_as_a_space() {
        // The identity joined physical lines with a space, INCLUDING the ones a
        // literal spans. A constant written across two lines therefore compared
        // equal to the same constant rewritten with a space, and the exact-match
        // pass consumed the addition: a changed public value left no finding.
        let patch = one_file_patch(
            "src/model.rs",
            &[
                "-pub const BANNER: &str = \"a",
                "-b\";",
                "+pub const BANNER: &str = \"a b\";",
            ],
        );

        let findings = analyze_all_breaking_changes(&[patch]);
        assert!(
            findings.iter().any(|f| matches!(
                &f.kind,
                BreakingKind::RemovedSymbol { .. } | BreakingKind::ChangedSignature { .. }
            )),
            "collapsing a literal's newline into a space changes the value, got: {:?}",
            findings.iter().map(|f| &f.kind).collect::<Vec<_>>()
        );
    }

    #[test]
    fn reflowing_a_declaration_across_lines_is_not_a_signature_change() {
        // The line break is preserved because a literal may span it. Preserving
        // it unconditionally made a purely cosmetic reflow — the same alias
        // rewritten onto one line — read as two different declarations, and the
        // pairing pass reported a signature change for an API that did not move.
        let patch = one_file_patch(
            "src/model.rs",
            &["-pub type Alias =", "-    u32;", "+pub type Alias = u32;"],
        );

        let findings = analyze_all_breaking_changes(&[patch]);
        assert!(
            !findings.iter().any(|f| matches!(
                &f.kind,
                BreakingKind::RemovedSymbol { .. } | BreakingKind::ChangedSignature { .. }
            )),
            "a reflow states the same API, got: {:?}",
            findings.iter().map(|f| &f.kind).collect::<Vec<_>>()
        );
    }

    #[test]
    fn reflowing_a_declaration_onto_more_lines_is_not_a_signature_change_either() {
        // The same no-op in the other direction, so the rule is not a one-way
        // tolerance that merely happens to hold for the shape above.
        let patch = one_file_patch(
            "src/model.rs",
            &["-pub type Alias = u32;", "+pub type Alias =", "+    u32;"],
        );

        let findings = analyze_all_breaking_changes(&[patch]);
        assert!(
            !findings.iter().any(|f| matches!(
                &f.kind,
                BreakingKind::RemovedSymbol { .. } | BreakingKind::ChangedSignature { .. }
            )),
            "a reflow states the same API, got: {:?}",
            findings.iter().map(|f| &f.kind).collect::<Vec<_>>()
        );
    }

    #[test]
    fn a_blank_line_inside_a_literal_is_part_of_the_value() {
        // The other half of the same rule: a line contributing no code is
        // dropped from the identity, but inside a literal an empty line IS the
        // value. Dropping it made a constant with a blank line compare equal to
        // the same constant without one.
        let patch = one_file_patch(
            "src/model.rs",
            &[
                "-pub const BANNER: &str = \"a",
                "-",
                "-b\";",
                "+pub const BANNER: &str = \"a",
                "+b\";",
            ],
        );

        let findings = analyze_all_breaking_changes(&[patch]);
        assert!(
            findings.iter().any(|f| matches!(
                &f.kind,
                BreakingKind::RemovedSymbol { .. } | BreakingKind::ChangedSignature { .. }
            )),
            "dropping a literal's blank line changes the value, got: {:?}",
            findings.iter().map(|f| &f.kind).collect::<Vec<_>>()
        );
    }

    #[test]
    fn a_multiline_literal_reemitted_unchanged_is_still_a_no_op() {
        // The tolerant direction: the same constant re-emitted across the same
        // physical lines must stay a no-op.
        let patch = one_file_patch(
            "src/model.rs",
            &[
                "-pub const BANNER: &str = \"a",
                "-b\";",
                "+pub const BANNER: &str = \"a",
                "+b\";",
            ],
        );

        let findings = analyze_all_breaking_changes(&[patch]);
        assert!(
            !findings.iter().any(|f| matches!(
                &f.kind,
                BreakingKind::RemovedSymbol { .. } | BreakingKind::ChangedSignature { .. }
            )),
            "an unchanged multiline literal is not breaking, got: {:?}",
            findings.iter().map(|f| &f.kind).collect::<Vec<_>>()
        );
    }

    #[test]
    fn a_changed_string_literal_is_still_a_signature_change() {
        // The direction the comment-free identity must NOT buy: a literal is
        // code. Comparing declarations on a view that drops literal bodies would
        // pair a real value change away as an unchanged re-add.
        let patch = one_file_patch(
            "src/model.rs",
            &[
                "-pub const GREETING: &str = \"hello\";",
                "+pub const GREETING: &str = \"goodbye\";",
            ],
        );

        let findings = analyze_all_breaking_changes(&[patch]);
        assert!(
            findings.iter().any(|f| matches!(
                &f.kind,
                BreakingKind::ChangedSignature { before, after }
                    if before.contains("hello") && after.contains("goodbye")
            )),
            "a changed public constant must still surface, got: {:?}",
            findings.iter().map(|f| &f.kind).collect::<Vec<_>>()
        );
    }

    #[test]
    fn a_long_declaration_reemitted_unchanged_is_still_a_no_op() {
        // The tolerant direction of the same accumulation: a long signature the
        // diff re-emits verbatim must stay a no-op, not become a removal.
        let mut body = Vec::new();
        for side in ["-", "+"] {
            body.push(format!("{side}pub fn build("));
            for name in ["a", "b", "c", "d", "e", "f", "g", "h", "i", "j"] {
                body.push(format!("{side}    {name}: u8,"));
            }
            body.push(format!("{side}) -> u8 {{"));
        }
        let refs: Vec<&str> = body.iter().map(String::as_str).collect();
        let patch = one_file_patch("src/model.rs", &refs);

        let findings = analyze_all_breaking_changes(&[patch]);
        assert!(
            !findings.iter().any(|f| matches!(
                &f.kind,
                BreakingKind::RemovedSymbol { .. } | BreakingKind::ChangedSignature { .. }
            )),
            "an unchanged long declaration must produce no finding, got: {:?}",
            findings.iter().map(|f| &f.kind).collect::<Vec<_>>()
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
