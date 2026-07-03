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
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct UnsafeFinding {
    pub file: String,
    pub line: usize,
    pub content: String,
    pub has_safety_comment: bool,
}

pub fn generate_unsafe_audit(
    dir: &Path,
    diffs: &[Diff],
    repo: &Repository,
) -> Result<Option<CheckResult>> {
    let mut findings = Vec::new();

    for diff in diffs {
        for file in &diff.files {
            if matches!(classify_review_file(&file.path), ReviewFileCategory::Code)
                && file.path.ends_with(".rs")
                && let Ok(patch) =
                    repo.file_diff(&diff.base_commit_id, &diff.target_commit_id, &file.path)
            {
                findings.extend(scan_for_unsafe(&file.path, &patch));
            }
        }
    }

    if findings.is_empty() {
        return Ok(None);
    }

    let audit = UnsafeAudit { findings };

    fs::create_dir_all(dir)?;
    fs::write(
        dir.join("UNSAFE_AUDIT.json"),
        serde_json::to_string_pretty(&audit)?,
    )?;

    let md = format_unsafe_audit(&audit);
    fs::write(dir.join("UNSAFE_AUDIT.md"), md)?;

    let msg = format!(
        "[⚠️ line heuristic] Found {} new `unsafe` block(s)",
        audit.findings.len()
    );

    Ok(Some(CheckResult {
        name: "unsafe_audit".to_string(),
        status: CheckStatus::Warnings,
        duration: std::time::Duration::ZERO,
        output: msg,
        cached: false,
        provenance: None,
    }))
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
        "> ⚠️ **NEEDS VERIFICATION**: *Generated by a text heuristic (string literals, raw strings and comments are excluded). Safety macros may be missed.* \n"
    );
    let _ = writeln!(
        md,
        "Detected {} new `unsafe` block(s) in this PR.\n",
        audit.findings.len()
    );

    let _ = writeln!(md, "| File | Line | Snippet | Safety Comment? |");
    let _ = writeln!(md, "|------|------|---------|-----------------|");

    for f in &audit.findings {
        let comment_mark = if f.has_safety_comment {
            "✅ Yes"
        } else {
            "❌ Missing"
        };
        let _ = writeln!(
            md,
            "| `{}` | `{}` | `{}` | {} |",
            f.file, f.line, f.content, comment_mark
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
