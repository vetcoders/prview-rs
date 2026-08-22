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
/// Stateless, so a `/*` or a `"` left open at the end of `line` simply ends the
/// code on that line. Use [`SourceScanner`] to carry an open construct across
/// consecutive lines.
///
/// Text that is itself multi-line (an accumulated declaration) may be passed
/// whole: the scan runs over it in one piece, so a literal spanning its lines
/// closes where it really closes.
pub(crate) fn code_only(line: &str) -> Cow<'_, str> {
    let mut state = ScanState::default();
    scan(line, &mut state)
}

/// Line-by-line source reader that remembers a construct left open.
///
/// Block comments and string literals are the two things a per-line scanner
/// cannot resolve on its own: `/* } */` spread over three lines hides a brace
/// that never reaches the tracker as syntax, and so does
/// `const T: &str = "{\n}";`. Consumers that walk a hunk in order keep one
/// scanner for that walk and [`reset`](Self::reset) it at boundaries where the
/// text is no longer contiguous.
#[derive(Default)]
pub(crate) struct SourceScanner {
    state: ScanState,
}

impl SourceScanner {
    /// The code part of `line`, continuing any construct still open.
    pub(crate) fn code_only<'a>(&mut self, line: &'a str) -> Cow<'a, str> {
        scan(line, &mut self.state)
    }

    /// Forget a comment or literal left open: the next line is not contiguous
    /// with the last one (a new hunk, a new file).
    ///
    /// This is also the boundary at which carrying stops being sound. A hunk
    /// may START in the middle of a literal, and then its closing delimiter
    /// reads as an opener — measured at 1 hunk in 872 over this repo's history,
    /// against 29 hunk sides in the same history whose brace counting the
    /// carrying fixes. The residue never outlives the hunk.
    pub(crate) fn reset(&mut self) {
        self.state = ScanState::default();
    }
}

/// What an earlier line left open.
#[derive(Default)]
struct ScanState {
    /// `/* … */` nesting carried in (Rust block comments nest).
    block_comment_depth: u32,
    /// A string literal whose closing delimiter has not been seen yet.
    open_literal: Option<OpenLiteral>,
}

/// A string literal still waiting for its closing delimiter.
#[derive(Clone, Copy)]
enum OpenLiteral {
    /// `"…` — closed by the first unescaped `"`.
    Normal,
    /// `r#"…` / `br##"…` — closed by `"` plus exactly this many `#`. No escapes.
    Raw { hashes: usize },
}

/// One pass over `line`, dropping comments and blanking literal contents.
///
/// `state` is what earlier lines left open — a nested block comment, a string
/// literal — and is updated in place.
///
/// Comments and literals are resolved in the SAME pass, which is what keeps a
/// delimiter from being read in the wrong language: `"http://x"` is a string,
/// not a comment, and `format!("{}/*.{}", dir, ext)` is a glob pattern, not a
/// block comment swallowing the rest of the file. Normal strings (with `\`
/// escapes), raw strings (`r"…"`, `r#"…"#`, `br##"…"##`) and char literals
/// (including `'\u{7b}'`) are recognised; a `'` that does not close as a char
/// literal is a lifetime and is left alone. A char literal cannot span lines,
/// so only strings are carried.
fn scan<'a>(line: &'a str, state: &mut ScanState) -> Cow<'a, str> {
    let bytes = line.as_bytes();
    if state.block_comment_depth == 0
        && state.open_literal.is_none()
        && !bytes.iter().any(|b| matches!(b, b'"' | b'\''))
        && !line.contains("//")
        && !line.contains("/*")
    {
        return Cow::Borrowed(line);
    }

    let mut out = String::with_capacity(line.len());
    let mut i = 0;
    // A literal opened on an earlier line owns the start of this one.
    if let Some(open) = state.open_literal {
        match literal_close(line, 0, open) {
            Some(end) => {
                state.open_literal = None;
                i = end;
            }
            None => return Cow::Owned(out),
        }
    }

    while i < bytes.len() {
        if state.block_comment_depth > 0 {
            if bytes[i] == b'*' && bytes.get(i + 1) == Some(&b'/') {
                state.block_comment_depth -= 1;
                i += 2;
            } else if bytes[i] == b'/' && bytes.get(i + 1) == Some(&b'*') {
                state.block_comment_depth += 1;
                i += 2;
            } else {
                i += next_char_len(line, i);
            }
            continue;
        }

        if let Some(raw) = raw_string_start(line, i) {
            match literal_close(line, raw.body_start, raw.open) {
                Some(end) => i = end,
                None => {
                    state.open_literal = Some(raw.open);
                    return Cow::Owned(out);
                }
            }
            continue;
        }

        match bytes[i] {
            b'"' => match literal_close(line, i + 1, OpenLiteral::Normal) {
                Some(end) => i = end,
                None => {
                    state.open_literal = Some(OpenLiteral::Normal);
                    return Cow::Owned(out);
                }
            },
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
                state.block_comment_depth += 1;
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

/// A raw string opener found in the text.
struct RawStringStart {
    /// Index just past the opening `"`, where the literal body begins.
    body_start: usize,
    open: OpenLiteral,
}

/// The raw string opening at `start`, or `None` if none opens there.
fn raw_string_start(code: &str, start: usize) -> Option<RawStringStart> {
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
    Some(RawStringStart {
        body_start: i + 1,
        open: OpenLiteral::Raw { hashes },
    })
}

/// Index just past the closing delimiter of `open`, searching from `from`, or
/// `None` when the literal runs past the end of `code`.
///
/// `None` is the whole point of carrying literal state: it says the literal is
/// still open, so the NEXT line's leading text is body, not code.
fn literal_close(code: &str, from: usize, open: OpenLiteral) -> Option<usize> {
    let bytes = code.as_bytes();
    let mut i = from;
    match open {
        OpenLiteral::Normal => {
            while i < bytes.len() {
                match bytes[i] {
                    b'\\' => i += 2,
                    b'"' => return Some(i + 1),
                    _ => i += 1,
                }
            }
            None
        }
        // Raw strings have no escapes: the only terminator is a quote followed
        // by exactly as many hashes as the opener carried.
        OpenLiteral::Raw { hashes } => {
            while i < bytes.len() {
                if bytes[i] == b'"'
                    && bytes.len() - (i + 1) >= hashes
                    && bytes[i + 1..].iter().take(hashes).all(|b| *b == b'#')
                {
                    return Some(i + 1 + hashes);
                }
                i += 1;
            }
            None
        }
    }
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
    fn a_raw_string_holding_a_quote_and_a_slash_slash_stays_one_literal() {
        // The reason comments and literals are resolved in ONE pass: a
        // comment-stripping pass that only knows `"…"` sees the interior quote
        // of `r#"a " b // c"#` as closing the string, then reads the `//` as a
        // real comment and truncates the line — dropping the `{` after it and
        // corrupting the depth for both the perf and module-scope trackers.
        assert_eq!(code_only("let s = r#\"a \" b // c\"#; {"), "let s = ; {");
        assert_eq!(code_only("let b = br##\"x \" // y\"##; }"), "let b = ; }");
        // A `//` genuinely after the literal still ends the code.
        assert_eq!(code_only("let s = r#\"a\"#; // trailing {"), "let s = ; ");
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

    #[test]
    fn a_normal_string_stays_open_across_lines() {
        // A string literal spans lines exactly like a block comment does, and
        // its body is data on every one of them. Reading the tail as code made
        // the closing `"` look like an OPENER and the `}` in front of it look
        // like syntax — a brace that pops `mod inner` one level early, after
        // which a removed `inner::Config` carries an unknown scope and pairs
        // with any addition, hiding a real API removal.
        let mut scanner = SourceScanner::default();
        assert_eq!(scanner.code_only("mod inner {"), "mod inner {");
        assert_eq!(
            scanner.code_only("    const T: &str = \"{"),
            "    const T: &str = "
        );
        assert_eq!(scanner.code_only("}\";"), ";");
        assert_eq!(scanner.code_only("}"), "}");
    }

    #[test]
    fn a_raw_string_stays_open_across_lines_until_its_own_delimiter() {
        // Multi-line raw strings are how JSON fixtures are written, so their
        // bodies are full of braces. The closing delimiter is `"` plus exactly
        // as many hashes as the opener carried: an interior `"#` with the wrong
        // hash count does not end it.
        let mut scanner = SourceScanner::default();
        assert_eq!(scanner.code_only("let j = br##\"{"), "let j = ");
        assert_eq!(scanner.code_only("  \"a\": \"x\"#,"), "");
        assert_eq!(scanner.code_only("}\"##; {"), "; {");
    }

    #[test]
    fn an_escaped_quote_does_not_close_a_carried_string() {
        let mut scanner = SourceScanner::default();
        assert_eq!(scanner.code_only("let s = \"one {"), "let s = ");
        assert_eq!(scanner.code_only("two \\\" still inside {"), "");
        assert_eq!(scanner.code_only("three\"; }"), "; }");
    }

    #[test]
    fn reset_forgets_a_literal_left_open() {
        // The hunk boundary is where carrying stops being deterministic: the
        // next hunk may start anywhere, including outside the literal. Every
        // consumer resets there, and the reset must clear the literal for the
        // same reason it clears the comment.
        let mut scanner = SourceScanner::default();
        assert_eq!(
            scanner.code_only("let s = \"opened and never closed"),
            "let s = "
        );
        scanner.reset();
        assert_eq!(
            scanner.code_only("pub struct Config {"),
            "pub struct Config {"
        );
    }
}
