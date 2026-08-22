//! Reading Rust source out of a unified diff, one line at a time.
//!
//! Several trackers walk diff text counting delimiters to decide something:
//! the perf tracker (is this line inside inline `#[cfg(test)]` context?), the
//! breaking-change tracker (which inline `mod` is this line in?) and the
//! declaration accumulator (has this public declaration ended?). All three are
//! fooled by the same thing — a delimiter that is not syntax — and all three
//! need the same answer, so the scanner lives in one place rather than being
//! reimplemented per consumer.

use std::borrow::Cow;

/// The code part of `line`: comments dropped, literal contents blanked.
///
/// This is what a delimiter tracker should walk. `const CLOSE: &str = "}";`,
/// `// closes with }` and `/* } */` all reduce to text carrying no brace.
///
/// Stateless, so a `/*` left open at the end of `line` simply ends the code on
/// that line. Use [`SourceScanner`] to carry an open block comment across
/// consecutive lines.
pub(crate) fn code_only(line: &str) -> Cow<'_, str> {
    let mut block_depth = 0;
    scan(line, &mut block_depth)
}

/// Line-by-line source reader that remembers a `/* … */` left open.
///
/// A block comment is the one construct a per-line scanner cannot resolve on
/// its own: `/* } */` spread over three lines hides a brace that never reaches
/// the tracker as syntax. Consumers that walk a hunk in order keep one scanner
/// for that walk and [`reset`](Self::reset) it at boundaries where the text is
/// no longer contiguous.
#[derive(Default)]
pub(crate) struct SourceScanner {
    block_comment_depth: u32,
}

impl SourceScanner {
    /// The code part of `line`, continuing any block comment still open.
    pub(crate) fn code_only<'a>(&mut self, line: &'a str) -> Cow<'a, str> {
        scan(line, &mut self.block_comment_depth)
    }

    /// Forget a block comment left open: the next line is not contiguous with
    /// the last one (a new hunk, a new file).
    pub(crate) fn reset(&mut self) {
        self.block_comment_depth = 0;
    }
}

/// One pass over `line`, dropping comments and blanking literal contents.
///
/// `block_depth` is the `/* … */` nesting carried in from earlier lines (Rust
/// block comments nest) and is updated in place.
///
/// Comments and literals are resolved in the SAME pass, which is what keeps a
/// delimiter from being read in the wrong language: `"http://x"` is a string,
/// not a comment, and `format!("{}/*.{}", dir, ext)` is a glob pattern, not a
/// block comment swallowing the rest of the file. Normal strings (with `\`
/// escapes), raw strings (`r"…"`, `r#"…"#`, `br##"…"##`) and char literals
/// (including `'\u{7b}'`) are recognised; a `'` that does not close as a char
/// literal is a lifetime and is left alone.
///
/// Best-effort in one respect: a *string* literal spanning several lines is not
/// tracked across them, so its tail is read as code on the following line. That
/// residue can only affect delimiter counting inside a literal body, which is
/// rarer than the single-line case this handles.
fn scan<'a>(line: &'a str, block_depth: &mut u32) -> Cow<'a, str> {
    let bytes = line.as_bytes();
    if *block_depth == 0
        && !bytes.iter().any(|b| matches!(b, b'"' | b'\''))
        && !line.contains("//")
        && !line.contains("/*")
    {
        return Cow::Borrowed(line);
    }

    let mut out = String::with_capacity(line.len());
    let mut i = 0;
    while i < bytes.len() {
        if *block_depth > 0 {
            if bytes[i] == b'*' && bytes.get(i + 1) == Some(&b'/') {
                *block_depth -= 1;
                i += 2;
            } else if bytes[i] == b'/' && bytes.get(i + 1) == Some(&b'*') {
                *block_depth += 1;
                i += 2;
            } else {
                i += next_char_len(line, i);
            }
            continue;
        }

        if let Some(end) = raw_string_end(line, i) {
            i = end;
            continue;
        }

        match bytes[i] {
            b'"' => i = normal_string_end(line, i),
            b'\'' => match char_literal_end(line, i) {
                Some(end) => i = end,
                None => {
                    out.push('\'');
                    i += 1;
                }
            },
            // The rest of the line is a `//` comment: nothing after it is code.
            b'/' if bytes.get(i + 1) == Some(&b'/') => return Cow::Owned(out),
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                *block_depth += 1;
                i += 2;
            }
            _ => {
                let ch = line[i..]
                    .chars()
                    .next()
                    .expect("index sits on a char boundary");
                out.push(ch);
                i += ch.len_utf8();
            }
        }
    }
    Cow::Owned(out)
}

/// Byte length of the char starting at `i`.
fn next_char_len(line: &str, i: usize) -> usize {
    line[i..].chars().next().map_or(1, char::len_utf8)
}

/// End index of a raw string starting at `start`, or `None` if none starts there.
fn raw_string_end(code: &str, start: usize) -> Option<usize> {
    let bytes = code.as_bytes();
    // The prefix must be a token start, otherwise `bar"` would look like `b` + `"`.
    if start > 0 && (bytes[start - 1].is_ascii_alphanumeric() || bytes[start - 1] == b'_') {
        return None;
    }

    let mut i = start;
    if bytes.get(i) == Some(&b'b') {
        i += 1;
    }
    if bytes.get(i) != Some(&b'r') {
        return None;
    }
    i += 1;

    let hash_start = i;
    while bytes.get(i) == Some(&b'#') {
        i += 1;
    }
    let hashes = i - hash_start;
    if bytes.get(i) != Some(&b'"') {
        return None;
    }
    i += 1;

    // Closing delimiter: a quote followed by exactly as many hashes.
    while i < bytes.len() {
        if bytes[i] == b'"' && bytes[i + 1..].iter().take(hashes).all(|b| *b == b'#') {
            let close = i + 1 + hashes;
            if close <= bytes.len() {
                return Some(close);
            }
        }
        i += 1;
    }
    // Unterminated on this line: the rest of the line is literal body.
    Some(bytes.len())
}

/// End index of the normal string literal opening at `start`.
fn normal_string_end(code: &str, start: usize) -> usize {
    let bytes = code.as_bytes();
    let mut i = start + 1;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 2,
            b'"' => return i + 1,
            _ => i += 1,
        }
    }
    bytes.len()
}

/// End index of the char literal opening at `start`, or `None` for a lifetime.
fn char_literal_end(code: &str, start: usize) -> Option<usize> {
    let bytes = code.as_bytes();
    if bytes.get(start + 1) == Some(&b'\\') {
        // `'\''`, `'\\'`, `'\u{7b}'`: skip the escaped byte, then find the close.
        // Bounded so a stray backslash cannot swallow the rest of the line.
        let mut i = start + 3;
        let limit = (start + 12).min(bytes.len());
        while i < limit {
            if bytes[i] == b'\'' {
                return Some(i + 1);
            }
            i += 1;
        }
        return None;
    }

    let ch = code.get(start + 1..)?.chars().next()?;
    let close = start + 1 + ch.len_utf8();
    (bytes.get(close) == Some(&b'\'')).then_some(close + 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_comments_are_not_code_but_a_url_is_not_a_comment() {
        assert_eq!(code_only("// whole line {"), "");
        assert_eq!(code_only("let x = 1; // trailing {"), "let x = 1; ");
        // Truncating at the `//` of a URL would drop the brace after it and
        // corrupt the depth in the other direction.
        assert_eq!(
            code_only("let url = \"https://example.com\"; {"),
            "let url = ; {"
        );
        assert_eq!(code_only("let x = 1;"), "let x = 1;");
    }

    #[test]
    fn literal_contents_are_blanked_but_lifetimes_survive() {
        assert_eq!(code_only("let s = \"} {\";"), "let s = ;");
        assert_eq!(code_only("let c = '}';"), "let c = ;");
        assert_eq!(code_only("let e = '\\u{7b}';"), "let e = ;");
        assert_eq!(code_only("let q = \"\\\"}\";"), "let q = ;");
        assert_eq!(code_only("let r = r#\"}\"#;"), "let r = ;");
        assert_eq!(code_only("let b = br##\"}\"##;"), "let b = ;");
        // A lifetime is not a char literal and must survive untouched.
        assert_eq!(
            code_only("fn f<'a>(x: &'a str) {"),
            "fn f<'a>(x: &'a str) {"
        );
        assert_eq!(code_only("if depth > 0 {"), "if depth > 0 {");
    }

    #[test]
    fn block_comments_are_not_code() {
        assert_eq!(code_only("let a = 5 /* } */ ;"), "let a = 5  ;");
        // Rust block comments nest.
        assert_eq!(code_only("a /* x /* } */ y */ b"), "a  b");
        assert_eq!(code_only("/** doc { */ fn f() {"), " fn f() {");
    }

    #[test]
    fn a_block_comment_opener_inside_a_string_is_data() {
        // Glob patterns carry `/*` far more often than Rust code carries a
        // block comment. Reading one as a comment opener would swallow the
        // rest of the line — and, with a scanner, the rest of the hunk.
        assert_eq!(
            code_only("let p = format!(\"{}/*.{}\", dir, ext);"),
            "let p = format!(, dir, ext);"
        );
        assert_eq!(code_only("let g = \"**/\"; {"), "let g = ; {");
    }

    #[test]
    fn a_block_comment_stays_open_across_lines() {
        let mut scanner = SourceScanner::default();
        assert_eq!(scanner.code_only("mod tests { /* start"), "mod tests { ");
        assert_eq!(scanner.code_only("    } still commented"), "");
        assert_eq!(
            scanner.code_only("    end */ let x = 1; {"),
            " let x = 1; {"
        );
        // Nothing is open any more, so the next line is ordinary code.
        assert_eq!(scanner.code_only("}"), "}");
    }

    #[test]
    fn reset_forgets_a_comment_left_open() {
        let mut scanner = SourceScanner::default();
        assert_eq!(scanner.code_only("/* opened and never closed"), "");
        scanner.reset();
        assert_eq!(
            scanner.code_only("pub struct Config {"),
            "pub struct Config {"
        );
    }
}
