//! Reading one line of Rust source out of a unified diff.
//!
//! Two independent trackers walk diff text counting braces to decide scope: the
//! perf tracker (inline `#[cfg(test)]` context) and the breaking-change tracker
//! (inline `mod` nesting). Both were fooled by the same thing — a brace that is
//! not syntax — and both need the same answer, so the scanner lives in one
//! place rather than being reimplemented per consumer.

use std::borrow::Cow;

/// The code part of `line`: comment dropped, literal contents blanked.
///
/// This is what a brace tracker should walk. `const CLOSE: &str = "}";` and
/// `// closes with }` both reduce to text carrying no brace at all.
pub(crate) fn code_only(line: &str) -> Cow<'_, str> {
    blank_literals(strip_line_comment(line))
}

/// Return the code part of `line`, dropping a `//` comment wherever it starts.
///
/// Comments are not code: a marker mentioned in one must not open test context,
/// and braces typed in one must not move the scope depth. Only FULL-LINE `//`
/// used to be recognised, which left every trailing comment live.
///
/// String literals are respected so a `https://` URL is not mistaken for a
/// comment — truncating there would drop whatever braces follow it and corrupt
/// the depth in the other direction. Char literals are deliberately NOT tracked:
/// a char literal cannot contain `//`, and tracking `'` would misread Rust
/// lifetimes (`&'a str`). A stray `"` inside a char literal only suppresses
/// stripping for that line, which is the pre-existing behavior.
pub(crate) fn strip_line_comment(line: &str) -> &str {
    let bytes = line.as_bytes();
    let mut in_string = false;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            // Skip the escaped character so `\"` does not close the string.
            b'\\' if in_string => i += 1,
            b'"' => in_string = !in_string,
            b'/' if !in_string && bytes.get(i + 1) == Some(&b'/') => return &line[..i],
            _ => {}
        }
        i += 1;
    }
    line
}

/// Return `code` with the contents of string and char literals removed.
///
/// A brace typed inside a literal is data, not syntax: `const CLOSE: &str = "}"`
/// in a test module used to close the test scope, so every later hit in that
/// module was classified as production; an unmatched `{` in a literal held the
/// scope open the other way and muted real production hits. Blanking literals
/// also stops a marker quoted in a string from opening test context at all.
///
/// Normal strings (with `\` escapes), raw strings (`r"…"`, `r#"…"#`, `br##"…"##`)
/// and char literals (including `'\u{7b}'`) are recognised. A `'` that does not
/// close as a char literal is a lifetime and is left alone.
///
/// Best-effort, deliberately: this is a per-line scanner over diff text, so a
/// literal spanning several lines (or cut in half by a hunk boundary) is not
/// tracked across lines — its tail is read as code on the following line. That
/// residue can only affect brace depth inside a literal body, which is rarer
/// than the single-line case this fixes.
pub(crate) fn blank_literals(code: &str) -> Cow<'_, str> {
    let bytes = code.as_bytes();
    if !bytes.iter().any(|b| matches!(b, b'"' | b'\'')) {
        return Cow::Borrowed(code);
    }

    let mut out = String::with_capacity(code.len());
    let mut i = 0;
    while i < bytes.len() {
        if let Some(end) = raw_string_end(code, i) {
            i = end;
            continue;
        }
        match bytes[i] {
            b'"' => i = normal_string_end(code, i),
            b'\'' => match char_literal_end(code, i) {
                Some(end) => i = end,
                None => {
                    out.push('\'');
                    i += 1;
                }
            },
            _ => {
                let ch = code[i..]
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
    fn test_double_slash_inside_string_literal_is_not_a_comment() {
        // Stripping `//` blindly would truncate a URL and drop the brace that
        // follows it, corrupting the scope depth in the other direction.
        assert_eq!(
            strip_line_comment("let url = \"https://example.com\"; // note"),
            "let url = \"https://example.com\"; "
        );
        assert_eq!(strip_line_comment("let x = 1;"), "let x = 1;");
        assert_eq!(strip_line_comment("// whole line"), "");
    }

    #[test]
    fn test_blank_literals_removes_literal_contents_only() {
        assert_eq!(blank_literals("let s = \"} {\";"), "let s = ;");
        assert_eq!(blank_literals("let c = '}';"), "let c = ;");
        assert_eq!(blank_literals("let e = '\\u{7b}';"), "let e = ;");
        assert_eq!(blank_literals("let q = \"\\\"}\";"), "let q = ;");
        assert_eq!(blank_literals("let r = r#\"}\"#;"), "let r = ;");
        assert_eq!(blank_literals("let b = br##\"}\"##;"), "let b = ;");
        // A lifetime is not a char literal and must survive untouched.
        assert_eq!(
            blank_literals("fn f<'a>(x: &'a str) {"),
            "fn f<'a>(x: &'a str) {"
        );
        assert_eq!(blank_literals("if depth > 0 {"), "if depth > 0 {");
    }
}
