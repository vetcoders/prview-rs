//! Unsafe Audit — diff-first scan for unsafe code blocks.

use super::common::{
    ReviewFileCategory, RustLexState, classify_review_file, parse_patch_new_start,
    strip_rust_non_code,
};
use crate::checks::{CheckResult, CheckStatus};
use crate::git::{Diff, Repository};
use anyhow::Result;
use std::fmt::Write as FmtWrite;
use std::fs;
use std::path::Path;

#[derive(Debug, Clone, serde::Serialize)]
pub struct UnsafeAudit {
    pub findings: Vec<UnsafeFinding>,
    pub test_only_count: usize,
    pub production_or_unknown_count: usize,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct UnsafeFinding {
    pub file: String,
    pub line: usize,
    pub content: String,
    pub has_safety_comment: bool,
    pub scope: UnsafeScope,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UnsafeScope {
    TestOnly,
    ProductionOrUnknown,
}

pub fn generate_unsafe_audit(
    dir: &Path,
    diffs: &[Diff],
    repo: &Repository,
) -> Result<Option<CheckResult>> {
    let mut findings = Vec::new();

    for diff in diffs {
        for file in &diff.files {
            if matches!(
                classify_review_file(&file.path),
                ReviewFileCategory::Code | ReviewFileCategory::Test
            ) && file.path.ends_with(".rs")
                && let Ok(patch) =
                    repo.file_diff(&diff.base_commit_id, &diff.target_commit_id, &file.path)
            {
                let mut added = scan_for_unsafe(&file.path, &patch);
                if !added.is_empty()
                    && repo
                        .regular_file_at_commit(&diff.target_commit_id, &file.path)
                        .unwrap_or(false)
                    && let Ok(source) = repo.file_at_commit(&diff.target_commit_id, &file.path)
                {
                    let test_lines = test_only_unsafe_lines(&source);
                    for finding in &mut added {
                        if test_lines.contains(&finding.line) {
                            finding.scope = UnsafeScope::TestOnly;
                        }
                    }
                }
                findings.extend(added);
            }
        }
    }

    if findings.is_empty() {
        return Ok(None);
    }

    let test_only_count = findings
        .iter()
        .filter(|f| f.scope == UnsafeScope::TestOnly)
        .count();
    let production_or_unknown_count = findings.len() - test_only_count;
    let audit = UnsafeAudit {
        findings,
        test_only_count,
        production_or_unknown_count,
    };

    fs::create_dir_all(dir)?;
    fs::write(
        dir.join("UNSAFE_AUDIT.json"),
        serde_json::to_string_pretty(&audit)?,
    )?;

    let md = format_unsafe_audit(&audit);
    fs::write(dir.join("UNSAFE_AUDIT.md"), md)?;

    let msg = format!(
        "[line heuristic] {} new unsafe line(s): {} test-only (informational), {} production or unknown (review)",
        audit.findings.len(),
        audit.test_only_count,
        audit.production_or_unknown_count
    );

    Ok(Some(CheckResult {
        name: "unsafe_audit".to_string(),
        status: if audit.production_or_unknown_count == 0 {
            CheckStatus::Passed
        } else {
            CheckStatus::Warnings
        },
        duration: std::time::Duration::ZERO,
        output: msg,
        cached: false,
        provenance: None,
    }))
}

/// Lines whose every `unsafe` token is enclosed by a proven test-only scope.
/// Full target-source spans prevent hunk boundaries and same-line mixed scopes
/// from granting test-only status to production code.
fn test_only_unsafe_lines(source: &str) -> std::collections::BTreeSet<usize> {
    use proc_macro2::{TokenStream, TokenTree};
    use syn::visit::Visit;

    if source.len() > 2 * 1024 * 1024 {
        return Default::default();
    }
    let Ok(file) = syn::parse_file(source) else {
        return Default::default();
    };
    let Ok(tokens) = source.parse::<TokenStream>() else {
        return Default::default();
    };
    let mut scopes = TestScopes::default();
    scopes.visit_file(&file);
    let mut lines = std::collections::BTreeMap::new();
    let mut pending = vec![tokens];
    while let Some(tokens) = pending.pop() {
        for token in tokens {
            match token {
                TokenTree::Group(group) => pending.push(group.stream()),
                TokenTree::Ident(ident) if ident == "unsafe" => {
                    let at = ident.span().start();
                    let inside = scopes.0.iter().any(|span| {
                        let start = span.start();
                        let end = span.end();
                        (start.line, start.column) <= (at.line, at.column)
                            && (at.line, at.column) < (end.line, end.column)
                    });
                    *lines.entry(at.line).or_insert(true) &= inside;
                }
                _ => {}
            }
        }
    }
    lines
        .into_iter()
        .filter_map(|(line, only_test)| only_test.then_some(line))
        .collect()
}

#[derive(Default)]
struct TestScopes(Vec<proc_macro2::Span>);

impl TestScopes {
    fn record(&mut self, attrs: &[syn::Attribute], span: proc_macro2::Span) {
        if attrs.iter().any(attribute_requires_test) {
            self.0.push(span);
        }
    }
}

macro_rules! visit_test_scopes {
    ($($method:ident: $node:ident),* $(,)?) => {
        $(fn $method(&mut self, node: &'ast syn::$node) {
            use syn::spanned::Spanned;
            self.record(&node.attrs, node.span());
            syn::visit::$method(self, node);
        })*
    };
}

impl<'ast> syn::visit::Visit<'ast> for TestScopes {
    visit_test_scopes! {
        visit_file: File,
        visit_item_const: ItemConst,
        visit_item_enum: ItemEnum,
        visit_item_extern_crate: ItemExternCrate,
        visit_item_fn: ItemFn,
        visit_item_foreign_mod: ItemForeignMod,
        visit_item_impl: ItemImpl,
        visit_item_macro: ItemMacro,
        visit_item_mod: ItemMod,
        visit_item_static: ItemStatic,
        visit_item_struct: ItemStruct,
        visit_item_trait: ItemTrait,
        visit_item_trait_alias: ItemTraitAlias,
        visit_item_type: ItemType,
        visit_item_union: ItemUnion,
        visit_item_use: ItemUse,
        visit_foreign_item_fn: ForeignItemFn,
        visit_foreign_item_static: ForeignItemStatic,
        visit_foreign_item_type: ForeignItemType,
        visit_foreign_item_macro: ForeignItemMacro,
        visit_trait_item_const: TraitItemConst,
        visit_trait_item_fn: TraitItemFn,
        visit_trait_item_type: TraitItemType,
        visit_trait_item_macro: TraitItemMacro,
        visit_impl_item_const: ImplItemConst,
        visit_impl_item_fn: ImplItemFn,
        visit_impl_item_type: ImplItemType,
        visit_impl_item_macro: ImplItemMacro,
        visit_receiver: Receiver,
        visit_variadic: Variadic,
        visit_expr_array: ExprArray,
        visit_expr_assign: ExprAssign,
        visit_expr_async: ExprAsync,
        visit_expr_await: ExprAwait,
        visit_expr_binary: ExprBinary,
        visit_expr_block: ExprBlock,
        visit_expr_break: ExprBreak,
        visit_expr_call: ExprCall,
        visit_expr_cast: ExprCast,
        visit_expr_closure: ExprClosure,
        visit_expr_const: ExprConst,
        visit_expr_continue: ExprContinue,
        visit_expr_field: ExprField,
        visit_expr_for_loop: ExprForLoop,
        visit_expr_group: ExprGroup,
        visit_expr_if: ExprIf,
        visit_expr_index: ExprIndex,
        visit_expr_infer: ExprInfer,
        visit_expr_let: ExprLet,
        visit_expr_lit: ExprLit,
        visit_expr_loop: ExprLoop,
        visit_expr_macro: ExprMacro,
        visit_expr_match: ExprMatch,
        visit_expr_method_call: ExprMethodCall,
        visit_expr_paren: ExprParen,
        visit_expr_path: ExprPath,
        visit_expr_range: ExprRange,
        visit_expr_raw_addr: ExprRawAddr,
        visit_expr_reference: ExprReference,
        visit_expr_repeat: ExprRepeat,
        visit_expr_return: ExprReturn,
        visit_expr_struct: ExprStruct,
        visit_expr_try: ExprTry,
        visit_expr_try_block: ExprTryBlock,
        visit_expr_tuple: ExprTuple,
        visit_expr_unary: ExprUnary,
        visit_expr_unsafe: ExprUnsafe,
        visit_expr_while: ExprWhile,
        visit_expr_yield: ExprYield,
        visit_field_value: FieldValue,
        visit_arm: Arm,
        visit_variant: Variant,
        visit_field: Field,
        visit_lifetime_param: LifetimeParam,
        visit_type_param: TypeParam,
        visit_const_param: ConstParam,
        visit_pat_ident: PatIdent,
        visit_pat_or: PatOr,
        visit_pat_paren: PatParen,
        visit_pat_reference: PatReference,
        visit_pat_rest: PatRest,
        visit_pat_slice: PatSlice,
        visit_pat_struct: PatStruct,
        visit_pat_tuple: PatTuple,
        visit_pat_tuple_struct: PatTupleStruct,
        visit_pat_type: PatType,
        visit_pat_wild: PatWild,
        visit_field_pat: FieldPat,
        visit_local: Local,
        visit_stmt_macro: StmtMacro,
    }
}

fn attribute_requires_test(attr: &syn::Attribute) -> bool {
    if !attr.path().is_ident("cfg") {
        return false;
    }
    let Ok(meta) = attr.parse_args::<syn::Meta>() else {
        return false;
    };
    let (active_in_production, mentions_test) = cfg_without_test(&meta, 0);
    mentions_test && active_in_production == Some(false)
}

/// Evaluate only the built-in `test` predicate. Other target/feature predicates
/// are unknown; `all(test, unknown)` is excluded, `any(test, unknown)` is not.
fn cfg_without_test(meta: &syn::Meta, depth: usize) -> (Option<bool>, bool) {
    use syn::{Meta, Token, punctuated::Punctuated};
    if depth > 64 {
        return (None, false);
    }
    if let Meta::Path(path) = meta {
        return if path.is_ident("test") {
            (Some(false), true)
        } else {
            (None, false)
        };
    }
    let Meta::List(list) = meta else {
        return (None, false);
    };
    let Ok(children) = list.parse_args_with(Punctuated::<Meta, Token![,]>::parse_terminated) else {
        return (None, false);
    };
    let values: Vec<_> = children
        .iter()
        .map(|meta| cfg_without_test(meta, depth + 1))
        .collect();
    let mentions_test = values.iter().any(|(_, test)| *test);
    let value = if list.path.is_ident("all") {
        if values.iter().any(|(v, _)| *v == Some(false)) {
            Some(false)
        } else if values.iter().all(|(v, _)| *v == Some(true)) {
            Some(true)
        } else {
            None
        }
    } else if list.path.is_ident("any") {
        if values.iter().any(|(v, _)| *v == Some(true)) {
            Some(true)
        } else if values.iter().all(|(v, _)| *v == Some(false)) {
            Some(false)
        } else {
            None
        }
    } else if list.path.is_ident("not") && values.len() == 1 {
        values[0].0.map(|v| !v)
    } else {
        None
    };
    (value, mentions_test)
}

fn scan_for_unsafe(file_path: &str, patch: &str) -> Vec<UnsafeFinding> {
    let mut findings = Vec::new();
    let mut current_line = 0;
    let mut rust_state = RustLexState::default();

    // A single function-level SAFETY comment commonly covers a short matrix of
    // adjacent unsafe blocks in tests. Keep the nearby comment as context
    // instead of consuming it after the first block.
    let mut recent_safety_comment_line: Option<usize> = None;

    for line in patch.lines() {
        if line.starts_with("@@") {
            if let Some(start) = parse_patch_new_start(line) {
                current_line = start;
            }
            rust_state = RustLexState::default(); // hunks are not contiguous source
            continue;
        }

        if line.starts_with("---") || line.starts_with("+++") || line.starts_with('\\') {
            continue;
        }

        let is_added = line.starts_with('+');
        let is_context = line.starts_with(' ');

        if !is_added && !is_context {
            continue;
        }

        let content = &line[1..];
        let trimmed = content.trim();

        // Lex once per line so block-comment / raw-string state always advances,
        // and so the `SAFETY:` credit is read from the COMMENT portion only — a
        // string literal that merely contains "SAFETY:" must not credit an
        // adjacent unsafe block.
        let stripped = strip_rust_non_code(content, &mut rust_state, true);

        let trimmed_lower = trimmed.to_ascii_lowercase();
        let clears_credit = trimmed_lower.contains("no safety comment");
        let same_line_safety = !clears_credit && comment_credits_safety(&stripped.comment);

        // Update the rolling SAFETY-comment window.
        if clears_credit {
            recent_safety_comment_line = None;
        } else if same_line_safety {
            recent_safety_comment_line = Some(current_line);
        }

        // Detect an added unsafe block INDEPENDENTLY of the comment handling: a
        // line carrying both `unsafe { … }` and a trailing `// SAFETY:` must
        // still be recorded (credited), not swallowed by the comment branch.
        if is_added && code_has_unsafe(&stripped.code) {
            let has_safety_comment = same_line_safety
                || recent_safety_comment_line
                    .is_some_and(|safety_line| current_line.saturating_sub(safety_line) <= 50);
            findings.push(UnsafeFinding {
                file: file_path.to_string(),
                line: current_line,
                content: trimmed.chars().take(120).collect(),
                has_safety_comment,
                scope: UnsafeScope::ProductionOrUnknown,
            });
        } else if !same_line_safety
            && !clears_credit
            && stripped
                .code
                .chars()
                .any(|c| c.is_alphanumeric() || c == '_')
        {
            // Real intervening code breaks SAFETY comment credit; structural
            // braces alone keep the window open for unsafe fn bodies.
            recent_safety_comment_line = None;
        }

        current_line += 1;
    }

    findings
}

/// Whether stripped code introduces a new unsafe block / fn / trait / impl.
fn code_has_unsafe(code: &str) -> bool {
    code.contains("unsafe {")
        || code.contains("unsafe fn")
        || code.contains("unsafe trait")
        || code.contains("unsafe impl")
}

/// Whether a captured comment carries a `SAFETY:` justification. Read from the
/// comment portion only, so a string literal containing the text "SAFETY:"
/// never credits an adjacent unsafe block.
fn comment_credits_safety(comment: &str) -> bool {
    comment.contains("SAFETY:") || comment.contains("Safety:") || comment.contains("safety:")
}

fn format_unsafe_audit(audit: &UnsafeAudit) -> String {
    let mut md = String::new();
    let _ = writeln!(md, "# Unsafe Audit\n");
    let _ = writeln!(
        md,
        "> **Line heuristic:** string literals, raw strings and comments are excluded. Safety macros may be missed. Test-only classification describes code excluded from non-test builds, not proof that tests execute it. \n"
    );
    let _ = writeln!(
        md,
        "Detected {} new `unsafe` line(s): {} test-only (informational), {} production or unknown (review).\n",
        audit.findings.len(),
        audit.test_only_count,
        audit.production_or_unknown_count
    );

    let _ = writeln!(md, "| File | Line | Snippet | Safety Comment? | Scope |");
    let _ = writeln!(md, "|------|------|---------|-----------------|-------|");

    for f in &audit.findings {
        let comment_mark = if f.has_safety_comment {
            "✅ Yes"
        } else {
            "❌ Missing"
        };
        let _ = writeln!(
            md,
            "| `{}` | `{}` | `{}` | {} | {} |",
            f.file,
            f.line,
            f.content,
            comment_mark,
            if f.scope == UnsafeScope::TestOnly {
                "Test-only (info)"
            } else {
                "Production or unknown (review)"
            }
        );
    }

    md
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::artifacts::signal::test_helpers::{
        make_diff_with_ids, make_test_repo, mock_file_change,
    };
    use crate::git::FileStatus;

    #[test]
    fn proven_test_only_unsafe_is_preserved_without_warning() {
        let old = "#[cfg(test)]\nmod tests {\n    const _: () = ();\n    const _: () = ();\n    const _: () = ();\n    const _: () = ();\n    const _: () = ();\n    fn example() {\n        let value = 1;\n    }\n}\n";
        let new = old.replace(
            "let value = 1;",
            "let value = unsafe { 1 }; // SAFETY: literal only",
        );
        let (tmp, repo, base, target) = make_test_repo(&[("src/lib.rs", old, &new)]);
        assert!(
            !repo
                .file_diff(&base, &target, "src/lib.rs")
                .unwrap()
                .contains("cfg(test)")
        );
        let diff = make_diff_with_ids(
            base,
            target,
            vec![mock_file_change("src/lib.rs", FileStatus::Modified, 1, 1)],
        );
        // The operator's file deliberately contradicts the reviewed revision.
        fs::write(
            tmp.path().join("src/lib.rs"),
            "fn production() { unsafe { 1 }; }\n",
        )
        .unwrap();
        let out = tmp.path().join("pack");
        let check = generate_unsafe_audit(&out, &[diff], &repo)
            .unwrap()
            .unwrap();
        assert_eq!(check.status, CheckStatus::Passed);
        let json: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(out.join("UNSAFE_AUDIT.json")).unwrap())
                .unwrap();
        assert_eq!(json["findings"].as_array().unwrap().len(), 1);
        assert_eq!(json["findings"][0]["scope"], "test_only");
        assert_eq!(json["test_only_count"], 1);
        assert_eq!(json["production_or_unknown_count"], 0);
    }

    #[test]
    fn unsafe_scope_requires_proof_from_complete_rust_syntax() {
        let cases = [
            ("#[cfg(test)] mod t { fn f() { unsafe { 1 }; } }", true),
            ("#![cfg(test)]\nfn f() { unsafe { 1 }; }", true),
            ("#[test] fn f() { unsafe { 1 }; }", false),
            ("#[cfg(test)] struct Packet([u8; unsafe { 1 }]);", true),
            (
                "struct Packet { #[cfg(test)] bytes: [u8; unsafe { 1 }] }",
                true,
            ),
            ("enum Packet { #[cfg(test)] A = unsafe { 1 }, B = 2 }", true),
            (
                "fn f() { match 1 { #[cfg(test)] 1 => unsafe { 1 }, _ => 0 }; }",
                true,
            ),
            ("fn f() { #[cfg(test)] consume!(unsafe { 1 }); }", true),
            ("#[cfg(all(test, unix))] fn f() { unsafe { 1 }; }", true),
            ("#[cfg(not(not(test)))] fn f() { unsafe { 1 }; }", true),
            ("#[cfg(test)] const V: i32 = unsafe { 1 };", true),
            ("fn f() { #[cfg(test)] let v = unsafe { 1 }; }", true),
            (
                "#[cfg(test)] mod t { fn f() { unsafe { 1 }; unsafe { 2 }; } }",
                true,
            ),
            ("mod tests { fn f() { unsafe { 1 }; } }", false),
            ("#[cfg(not(test))] fn f() { unsafe { 1 }; }", false),
            (
                "#[cfg(any(test, feature = \"x\"))] fn f() { unsafe { 1 }; }",
                false,
            ),
            ("#[cfg_attr(test, inline)] fn f() { unsafe { 1 }; }", false),
            ("#[tokio::test] async fn f() { unsafe { 1 }; }", false),
            (
                "#[cfg(test)] fn t() { unsafe { 1 }; } fn p() { unsafe { 2 }; }",
                false,
            ),
            ("// #[cfg(test)]\nfn f() { unsafe { 1 }; }", false),
            ("#[cfg(test)] mod t { fn f() { unsafe {", false),
        ];
        for (source, expected) in cases {
            assert_eq!(
                !test_only_unsafe_lines(source).is_empty(),
                expected,
                "{source}"
            );
        }
    }

    #[test]
    fn a_shadowed_test_attribute_is_not_test_only_proof() {
        let source =
            "use donor::test;\n#[test]\nfn production() -> u8 { unsafe { std::ptr::read(&7) } }\n";
        assert!(test_only_unsafe_lines(source).is_empty());
    }

    #[test]
    fn unsafe_test_file_path_preserves_unproven_helpers_as_warnings() {
        let source = "#[cfg(test)]\n#[test]\nfn example() { unsafe { 1 }; }\nfn helper() { unsafe { 2 }; }\n";
        let (tmp, repo, base, target) = make_test_repo(&[("tests/example.rs", "", source)]);
        let diff = make_diff_with_ids(
            base,
            target,
            vec![mock_file_change(
                "tests/example.rs",
                FileStatus::Modified,
                4,
                0,
            )],
        );
        let out = tmp.path().join("pack");
        let check = generate_unsafe_audit(&out, &[diff], &repo)
            .unwrap()
            .unwrap();
        assert_eq!(check.status, CheckStatus::Warnings);
        let json: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(out.join("UNSAFE_AUDIT.json")).unwrap())
                .unwrap();
        assert_eq!(json["findings"].as_array().unwrap().len(), 2);
        assert_eq!(json["test_only_count"], 1);
        assert_eq!(json["production_or_unknown_count"], 1);
        let md = fs::read_to_string(out.join("UNSAFE_AUDIT.md")).unwrap();
        assert!(md.contains("Test-only (info)"));
        assert!(md.contains("Production or unknown (review)"));
    }

    #[test]
    fn unsafe_audit_finds_unsafe_blocks() {
        let patch = "@@ -10,0 +10,4 @@\n\
                     + // SAFETY: trust me bro\n\
                     + unsafe fn do_bad_things() {\n\
                     + }\n\
                     + let x = unsafe { 42 };";
        let findings = scan_for_unsafe("src/lib.rs", patch);
        assert_eq!(findings.len(), 2);
        assert!(findings[0].has_safety_comment); // For unsafe fn
        assert!(findings[1].has_safety_comment); // Nearby function-level SAFETY context applies
    }

    #[test]
    fn test_scan_no_unsafe_returns_empty() {
        let patch = "@@ -1,0 +1,3 @@\n\
                     +fn safe_function() {\n\
                     +    let x = 42;\n\
                     +}";
        let findings = scan_for_unsafe("src/lib.rs", patch);
        assert!(
            findings.is_empty(),
            "no unsafe keyword means empty findings"
        );
    }

    #[test]
    fn test_scan_unsafe_with_safety_comment() {
        let patch = "@@ -1,0 +1,4 @@\n\
                     + // SAFETY: this pointer is valid and aligned\n\
                     + let val = unsafe { *raw_ptr };\n\
                     + // no safety comment here\n\
                     + let val2 = unsafe { *another_ptr };";
        let findings = scan_for_unsafe("src/core.rs", patch);
        assert_eq!(findings.len(), 2);
        assert!(
            findings[0].has_safety_comment,
            "first unsafe has SAFETY comment"
        );
        assert!(
            !findings[1].has_safety_comment,
            "second unsafe lacks SAFETY comment"
        );
    }

    #[test]
    fn same_line_safety_comment_still_records_the_unsafe_block() {
        // A line carrying both the unsafe block and its SAFETY justification
        // must be recorded (credited), not swallowed by the comment branch.
        let patch = "@@ -1,0 +1,1 @@\n\
                     + let val = unsafe { *raw_ptr }; // SAFETY: valid and aligned";
        let findings = scan_for_unsafe("src/core.rs", patch);
        assert_eq!(findings.len(), 1, "same-line unsafe must not be dropped");
        assert!(
            findings[0].has_safety_comment,
            "same-line SAFETY comment must credit the block"
        );
    }

    #[test]
    fn test_scan_ignores_unsafe_inside_strings_and_comments() {
        let patch = "@@ -1,0 +1,6 @@\n\
                     +let doc = \"unsafe { not code }\";\n\
                     +// unsafe { not code }\n\
                     +let fixture = r#\"\n\
                     +unsafe { also not code }\n\
                     +\"#;\n\
                     +let val = unsafe { *raw_ptr };";
        let findings = scan_for_unsafe("src/core.rs", patch);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].line, 6);
    }

    #[test]
    fn safety_inside_string_does_not_credit_following_unsafe() {
        // A string literal containing the text "SAFETY:" is not a justification
        // comment and must not credit the next unsafe block.
        let patch = "@@ -1,0 +1,2 @@\n\
                     + let s = \"// SAFETY: not really\";\n\
                     + let v = unsafe { *p };";
        let findings = scan_for_unsafe("src/lib.rs", patch);
        assert_eq!(findings.len(), 1);
        assert!(
            !findings[0].has_safety_comment,
            "SAFETY: living inside a string literal must not credit a following unsafe block"
        );
    }

    #[test]
    fn trailing_safety_comment_on_code_line_credits_next_block() {
        // A `// SAFETY:` trailing real code still arms the adjacent unsafe run.
        let patch = "@@ -1,0 +1,2 @@\n\
                     + let p = ptr(); // SAFETY: p is valid\n\
                     + let v = unsafe { *p };";
        let findings = scan_for_unsafe("src/lib.rs", patch);
        assert_eq!(findings.len(), 1);
        assert!(
            findings[0].has_safety_comment,
            "a trailing // SAFETY: comment must credit the next unsafe block"
        );
    }

    #[test]
    fn test_generate_unsafe_audit_wrapper() {
        // Create a repo with a .rs file that introduces unsafe code
        let old_content = "fn safe() { let x = 1; }\n";
        let new_content = "fn safe() { let x = 1; }\n\
                           // SAFETY: needed for FFI\n\
                           unsafe fn ffi_call() {}\n";
        let files = &[("src/lib.rs", old_content, new_content)];
        let (tmp, repo, base_id, target_id) = make_test_repo(files);

        let diff = make_diff_with_ids(
            base_id,
            target_id,
            vec![mock_file_change("src/lib.rs", FileStatus::Modified, 2, 0)],
        );

        let out_dir = tmp.path().join("30_context");
        let result = generate_unsafe_audit(&out_dir, &[diff], &repo).unwrap();
        assert!(result.is_some(), "should detect unsafe fn");

        let cr = result.unwrap();
        assert_eq!(cr.name, "unsafe_audit");
        assert!(cr.output.contains("1"));

        // Verify output files
        assert!(out_dir.join("UNSAFE_AUDIT.json").exists());
        assert!(out_dir.join("UNSAFE_AUDIT.md").exists());
    }

    #[test]
    fn ignores_unsafe_inside_string_literal() {
        // `description = "...unsafe { ... }"` is a string, not an unsafe block.
        let patch = "@@ -1,0 +1,2 @@\n\
                     +    let description = \"surfaces every unsafe { ... } block\";\n\
                     +    let x = 1;";
        let findings = scan_for_unsafe("src/main.rs", patch);
        assert!(
            findings.is_empty(),
            "unsafe inside a string literal must not be counted"
        );
    }

    #[test]
    fn ignores_unsafe_inside_multiline_raw_string() {
        // MCP tool descriptions use `r#"..."#` blocks that mention `unsafe { }`.
        let patch = "@@ -1,0 +1,4 @@\n\
                     +    let doc = r#\"\n\
                     +      suppressions: Rust unsafe { ... } and friends\n\
                     +    \"#;\n\
                     +    let y = 2;";
        let findings = scan_for_unsafe("src/main.rs", patch);
        assert!(
            findings.is_empty(),
            "unsafe inside a multi-line raw string must not be counted"
        );
    }

    #[test]
    fn shared_safety_comment_credits_adjacent_blocks() {
        let patch = "@@ -1,0 +1,4 @@\n\
                     + // SAFETY: env mutation isolated to this test\n\
                     + unsafe { std::env::set_var(\"K\", \"v\"); }\n\
                     + unsafe { std::env::set_var(\"K\", \"w\"); }\n\
                     + unsafe { std::env::remove_var(\"K\"); }";
        let findings = scan_for_unsafe("src/lib.rs", patch);
        assert_eq!(findings.len(), 3);
        assert!(
            findings.iter().all(|f| f.has_safety_comment),
            "one SAFETY comment credits the adjacent unsafe run"
        );
    }

    #[test]
    fn safety_credit_broken_by_intervening_code() {
        let patch = "@@ -1,0 +1,4 @@\n\
                     + // SAFETY: covers only the first block\n\
                     + unsafe { a(); }\n\
                     + let x = 1;\n\
                     + unsafe { b(); }";
        let findings = scan_for_unsafe("src/lib.rs", patch);
        assert_eq!(findings.len(), 2);
        assert!(findings[0].has_safety_comment);
        assert!(
            !findings[1].has_safety_comment,
            "intervening real code ends the safety run"
        );
    }
}
