//! Public API Diff — heuristic scan for public API surface changes.

use super::api_delta::{
    ApiArtifactView, ApiDeltaConfidence, ApiDeltaFinding, ApiDeltaKind, REPO_BACKED_RUST_API_SOURCE,
};
use super::common::{
    ReviewFileCategory, RustLexState, classify_review_file, js_ts_patch_sections,
    strip_rust_non_code,
};
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub analysis_source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rust_api_delta: Option<ApiArtifactView>,
}

#[cfg(test)]
pub(crate) mod api_surface_corpus_contract {
    use crate::artifacts::signal::breaking::historical_scenarios::{
        HistoricalFactKind, HistoricalTestId,
    };
    use serde::Deserialize;
    use std::collections::BTreeMap;

    #[derive(Debug, Deserialize, PartialEq, Eq)]
    #[serde(rename_all = "snake_case")]
    pub(crate) enum CorpusExpectation {
        Positive,
        Negative,
        AcceptedZero,
    }

    #[derive(Debug, Clone, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
    #[serde(rename_all = "snake_case")]
    pub(crate) enum ApiDeltaKind {
        Added,
        Removed,
        Changed,
        Relocated,
        VisibilityChanged,
        Unknown,
    }

    #[derive(Debug, Deserialize, PartialEq, Eq)]
    #[serde(rename_all = "snake_case")]
    pub(crate) enum ApiConfidence {
        Confirmed,
        Probable,
        Unknown,
    }

    #[derive(Debug, Deserialize)]
    pub(crate) struct ApiSourceProvenance {
        pub(crate) base_revision: String,
        pub(crate) target_revision: String,
        pub(crate) source_kind: String,
    }

    #[derive(Debug, Deserialize)]
    pub(crate) struct ExpectedApiDelta {
        pub(crate) kind: ApiDeltaKind,
        pub(crate) symbol: String,
        pub(crate) namespace: String,
        pub(crate) before: Option<String>,
        pub(crate) after: Option<String>,
        pub(crate) provenance: ApiSourceProvenance,
        pub(crate) confidence: ApiConfidence,
        pub(crate) unknown_reason: Option<String>,
    }

    #[derive(Debug, Deserialize)]
    pub(crate) struct CorpusExpected {
        pub(crate) schema: String,
        pub(crate) cell: String,
        pub(crate) family: String,
        pub(crate) legacy_expectation: CorpusExpectation,
        pub(crate) legacy_positive_sibling: Option<String>,
        #[serde(default)]
        pub(crate) legacy_delta_rationale: Option<String>,
        pub(crate) repo_backed_records: Vec<ExpectedApiDelta>,
    }

    #[derive(Debug, Deserialize)]
    pub(crate) struct CorpusManifestCell {
        pub(crate) id: String,
        pub(crate) family: String,
        pub(crate) legacy_expectation: CorpusExpectation,
        pub(crate) legacy_positive_sibling: Option<String>,
        #[serde(default)]
        pub(crate) legacy_delta_rationale: Option<String>,
        #[serde(default)]
        pub(crate) historical_test_id: Option<HistoricalTestId>,
        #[serde(default)]
        pub(crate) historical_expected_breaking_kinds: Vec<HistoricalFactKind>,
        #[serde(default)]
        pub(crate) recommended_disposition: Option<String>,
        #[serde(default)]
        pub(crate) phase_b_operator_effect: Option<String>,
    }

    #[derive(Debug, Deserialize, PartialEq, Eq)]
    #[serde(rename_all = "snake_case")]
    pub(crate) enum CorpusPolarity {
        Positive,
        Negative,
    }

    #[derive(Debug, Deserialize)]
    pub(crate) struct HistoricalRegressionMapping {
        pub(crate) cell: String,
        pub(crate) expected_polarity: CorpusPolarity,
        pub(crate) expected_delta_kinds: Vec<ApiDeltaKind>,
    }

    #[derive(Debug, Deserialize)]
    pub(crate) struct CorpusManifest {
        pub(crate) schema: String,
        pub(crate) required_families: Vec<String>,
        pub(crate) cells: Vec<CorpusManifestCell>,
        pub(crate) historical_regressions: BTreeMap<String, HistoricalRegressionMapping>,
    }
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
#[cfg(test)]
pub fn generate_public_api_diff(dir: &Path, patch_texts: &[String]) -> Result<Option<CheckResult>> {
    let diff = analyze_public_api_diff(patch_texts);
    write_public_api_diff(dir, diff, None)
}

/// Analyze only JS/TS diff sections with the legacy backend. Rust patch lines
/// are structurally absent from this input after the Phase B takeover.
pub fn analyze_js_ts_public_api_diff(patch_texts: &[String]) -> PublicApiDiff {
    let patches = patch_texts
        .iter()
        .map(|patch| js_ts_patch_sections(patch))
        .filter(|patch| !patch.is_empty())
        .collect::<Vec<_>>();
    analyze_public_api_diff(&patches)
}

fn analyze_public_api_diff(patch_texts: &[String]) -> PublicApiDiff {
    let mut added_findings = Vec::new();
    let mut removed_findings = Vec::new();
    let mut changed_findings = Vec::new();

    for patch in patch_texts {
        let (add, rm, ch) = analyze_patch_for_api_diff(patch);
        added_findings.extend(add);
        removed_findings.extend(rm);
        changed_findings.extend(ch);
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

    PublicApiDiff {
        added: added_findings,
        removed: removed_findings,
        changed: changed_findings,
        analysis_source: None,
        rust_api_delta: None,
    }
}

/// Write the compatibility projection plus the full canonical Rust view.
/// Existing readers retain `added`/`removed`/`changed`; additive readers use
/// `rust_api_delta` for stable IDs, confidence, evidence, and provenance.
pub fn write_public_api_diff(
    dir: &Path,
    mut diff: PublicApiDiff,
    rust_view: Option<&ApiArtifactView>,
) -> Result<Option<CheckResult>> {
    let had_legacy_js_ts_facts =
        !diff.added.is_empty() || !diff.removed.is_empty() || !diff.changed.is_empty();
    let legacy_js_breaking = !diff.removed.is_empty() || !diff.changed.is_empty();
    if let Some(view) = rust_view {
        for finding in &view.findings {
            project_rust_finding_for_legacy_fields(&mut diff, finding);
        }
        diff.analysis_source = Some(if had_legacy_js_ts_facts {
            "repo_backed_rust_api+legacy_js_ts_diff".to_owned()
        } else {
            REPO_BACKED_RUST_API_SOURCE.to_owned()
        });
        diff.rust_api_delta = Some(view.clone());
    }

    if diff.added.is_empty()
        && diff.removed.is_empty()
        && diff.changed.is_empty()
        && diff
            .rust_api_delta
            .as_ref()
            .is_none_or(|view| view.findings.is_empty())
    {
        return Ok(None);
    }

    diff.added
        .sort_by(|a, b| a.file.cmp(&b.file).then(a.signature.cmp(&b.signature)));
    diff.removed
        .sort_by(|a, b| a.file.cmp(&b.file).then(a.signature.cmp(&b.signature)));
    diff.changed
        .sort_by(|a, b| a.file.cmp(&b.file).then(a.before.cmp(&b.before)));
    dedupe_api_findings(&mut diff.added);
    dedupe_api_findings(&mut diff.removed);
    dedupe_signature_changes(&mut diff.changed);

    fs::create_dir_all(dir)?;
    fs::write(
        dir.join("PUBLIC_API_DIFF.json"),
        serde_json::to_string_pretty(&diff)?,
    )?;

    let md = format_public_api_diff(&diff);
    fs::write(dir.join("PUBLIC_API_DIFF.md"), md)?;

    let rust_breaking_or_unknown = diff.rust_api_delta.as_ref().is_some_and(|view| {
        view.findings.iter().any(|finding| {
            finding.confidence == ApiDeltaConfidence::Unknown
                || matches!(
                    finding.kind,
                    ApiDeltaKind::Removed
                        | ApiDeltaKind::Changed
                        | ApiDeltaKind::Relocated
                        | ApiDeltaKind::VisibilityChanged
                )
        })
    });
    let msg = format!(
        "Public API changed: {} new, {} removed, {} modified",
        diff.added.len(),
        diff.removed.len(),
        diff.changed.len()
    );

    Ok(Some(CheckResult {
        name: "public_api_diff".to_string(),
        status: if rust_breaking_or_unknown || legacy_js_breaking {
            CheckStatus::Warnings
        } else {
            CheckStatus::Passed
        },
        duration: std::time::Duration::ZERO,
        output: msg,
        cached: false,
        provenance: None,
    }))
}

fn project_rust_finding_for_legacy_fields(diff: &mut PublicApiDiff, finding: &ApiDeltaFinding) {
    match finding.kind {
        ApiDeltaKind::Added => {
            if let Some(after) = &finding.after {
                diff.added.push(ApiFinding {
                    file: after.source_path.clone(),
                    symbol_type: finding.identity.namespace.clone(),
                    signature: after.contract.clone(),
                });
            }
        }
        ApiDeltaKind::Removed => {
            if let Some(before) = &finding.before {
                diff.removed.push(ApiFinding {
                    file: before.source_path.clone(),
                    symbol_type: finding.identity.namespace.clone(),
                    signature: before.contract.clone(),
                });
            }
        }
        ApiDeltaKind::Changed | ApiDeltaKind::Relocated => {
            if let (Some(before), Some(after)) = (&finding.before, &finding.after) {
                diff.changed.push(ApiSignatureChange {
                    file: after.source_path.clone(),
                    symbol_type: finding.identity.namespace.clone(),
                    before: before.contract.clone(),
                    after: after.contract.clone(),
                });
            }
        }
        ApiDeltaKind::VisibilityChanged => match (&finding.before, &finding.after) {
            (Some(before), Some(after)) if before.declared_public && !after.declared_public => {
                diff.removed.push(ApiFinding {
                    file: before.source_path.clone(),
                    symbol_type: finding.identity.namespace.clone(),
                    signature: before.contract.clone(),
                });
            }
            (Some(_), Some(after)) => diff.added.push(ApiFinding {
                file: after.source_path.clone(),
                symbol_type: finding.identity.namespace.clone(),
                signature: after.contract.clone(),
            }),
            _ => {}
        },
        ApiDeltaKind::Unknown => {}
    }
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
    if diff.rust_api_delta.is_some() {
        let _ = writeln!(
            md,
            "> Rust facts are derived from exact revision-backed repository trees. JavaScript/TypeScript exports remain a bounded diff heuristic. Unknown Rust regions are preserved explicitly rather than inferred as removals.\n"
        );
    } else {
        let _ = writeln!(
            md,
            "> ⚠️ **NEEDS VERIFICATION**: *Generated by a fast text heuristic. It may miss AST details or raise false positives (e.g. macros). `export ...` is only scanned in JS/TS files; `pub use` is labelled as a re-export.* \n"
        );
    }

    if let Some(view) = &diff.rust_api_delta {
        let _ = writeln!(
            md,
            "- Rust analysis source: `{}`\n- Rust base revision: `{}`\n- Rust target revision: `{}`\n- Rust counts: added={}, removed={}, changed={}, relocated={}, visibility_changed={}, unknown={}\n",
            view.analysis_source,
            view.base_revision,
            view.target_revision,
            view.counts.added,
            view.counts.removed,
            view.counts.changed,
            view.counts.relocated,
            view.counts.visibility_changed,
            view.counts.unknown,
        );

        if !view.findings.is_empty() {
            let _ = writeln!(md, "## Canonical Rust API findings\n");
            for finding in &view.findings {
                let kind = match finding.kind {
                    ApiDeltaKind::Added => "Added",
                    ApiDeltaKind::Removed => "Removed",
                    ApiDeltaKind::Changed => "Changed",
                    ApiDeltaKind::Relocated => "Relocated",
                    ApiDeltaKind::VisibilityChanged => "VisibilityChanged",
                    ApiDeltaKind::Unknown => "Unknown",
                };
                let confidence = match finding.confidence {
                    ApiDeltaConfidence::Confirmed => "confirmed",
                    ApiDeltaConfidence::Unknown => "unknown",
                };
                let _ = writeln!(
                    md,
                    "- **{} `{}`** — `{}` in `{}` ({})",
                    kind,
                    finding.identity.name,
                    finding.identity.namespace,
                    finding.identity.external_path(),
                    confidence,
                );
                if let Some(before) = &finding.before {
                    let _ = writeln!(
                        md,
                        "  - Before: `{}` — `{}` (`{}`)",
                        before.contract, before.source_path, before.provenance,
                    );
                }
                if let Some(after) = &finding.after {
                    let _ = writeln!(
                        md,
                        "  - After: `{}` — `{}` (`{}`)",
                        after.contract, after.source_path, after.provenance,
                    );
                }
                if let Some(source) = &finding.unknown_source {
                    let side = match source.side {
                        super::api_delta::ApiSnapshotSide::Base => "base",
                        super::api_delta::ApiSnapshotSide::Target => "target",
                    };
                    let _ = writeln!(
                        md,
                        "  - Unknown source: `{side}` `{}` (`{}`)",
                        source.source_path, source.provenance,
                    );
                }
                if let Some(reason) = &finding.unknown_reason {
                    let _ = writeln!(md, "  - Unknown reason: {reason}");
                }
                let _ = writeln!(
                    md,
                    "  - Evidence: {}\n  - Finding ID: `{}`",
                    finding.evidence.join("; "),
                    finding.id,
                );
            }
            let _ = writeln!(md);
        }
    }

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
    fn js_ts_legacy_public_api_path_never_observes_rust_patch_lines() {
        let patch = "diff --git a/src/lib.rs b/src/lib.rs\n--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1 +0,0 @@\n-pub fn rust_only() {}\ndiff --git a/src/utils.ts b/src/utils.ts\n--- a/src/utils.ts\n+++ b/src/utils.ts\n@@ -1 +0,0 @@\n-export function js_only() {}\n";
        let diff = analyze_js_ts_public_api_diff(&[patch.to_owned()]);
        assert_eq!(diff.removed.len(), 1);
        assert_eq!(diff.removed[0].file, "src/utils.ts");
        assert!(diff.removed[0].signature.contains("js_only"));
        assert!(
            diff.removed
                .iter()
                .all(|finding| !finding.signature.contains("rust_only"))
        );
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
