use super::{Theme, render, stylesheet};

const FIXTURE: &str = r#"# Heading

Some **bold** and `inline code` and a [link](https://example.com).

- [x] done task
- [ ] pending task

| Check | Status |
|-------|--------|
| Build | Passed |

> [!WARNING]
> Careful here.

```rust
fn main() {
    println!("hi");
}
```
"#;

#[test]
fn wraps_output_in_root_class() {
    let out = render("hello", &Theme::default());
    assert!(out.starts_with("<div class=\"mdr\">"), "got: {out}");
    assert!(out.ends_with("</div>"));
}

#[test]
fn renders_gfm_table() {
    let out = render(FIXTURE, &Theme::default());
    assert!(out.contains("<table>"), "table missing: {out}");
    assert!(out.contains("<th>Check</th>"));
    assert!(out.contains("<td>Build</td>"));
}

#[test]
fn renders_tasklist_with_classes() {
    let out = render(FIXTURE, &Theme::default());
    assert!(
        out.contains("contains-task-list"),
        "task-list class missing"
    );
    assert!(out.contains("task-list-item-checkbox"));
    assert!(
        out.contains("type=\"checkbox\""),
        "checkbox input missing: {out}"
    );
    assert!(out.contains("checked"), "checked state missing");
}

#[test]
fn renders_alert_callout() {
    let out = render(FIXTURE, &Theme::default());
    assert!(
        out.contains("markdown-alert markdown-alert-warning"),
        "warning callout classes missing: {out}"
    );
    assert!(out.contains("markdown-alert-title"));
}

#[test]
fn highlights_fenced_code_with_inline_spans() {
    let out = render(FIXTURE, &Theme::default());
    // syntect emits a themed pre background and per-token color spans inline.
    assert!(
        out.contains("<pre style=\"background-color:#"),
        "syntect pre background missing: {out}"
    );
    assert!(
        out.contains("<span style=\"color:#"),
        "syntect inline highlight spans missing: {out}"
    );
}

#[test]
fn strikethrough_and_autolink() {
    let out = render("~~gone~~ and https://example.com", &Theme::default());
    assert!(out.contains("<del>gone</del>"));
    assert!(out.contains("<a href=\"https://example.com\""));
}

#[test]
fn sanitizes_script_tags() {
    let out = render("Hi <script>alert('xss')</script> there", &Theme::default());
    assert!(!out.contains("<script"), "script tag leaked: {out}");
    assert!(!out.contains("alert('xss')"), "script body leaked: {out}");
}

#[test]
fn strips_javascript_url_scheme() {
    // comrak passes markdown links through with any scheme; ammonia must drop
    // the dangerous href while keeping the link text.
    let out = render("[click](javascript:alert(1))", &Theme::default());
    assert!(!out.contains("javascript:"), "js url leaked: {out}");
    assert!(out.contains("click"));
}

#[test]
fn preserves_trusted_details_html() {
    let out = render(
        "<details><summary>More</summary>body</details>",
        &Theme::default(),
    );
    assert!(out.contains("<details>"), "details stripped: {out}");
    assert!(out.contains("<summary>More</summary>"));
}

#[test]
fn stylesheet_scoped_to_root_class() {
    let css = stylesheet(&Theme::default());
    assert!(css.contains(".mdr code"), "inline code rule missing");
    assert!(css.contains(".mdr .markdown-alert-warning"));
    assert!(css.contains(".mdr table"));
    assert!(css.contains(".mdr .task-list-item-checkbox"));
}

#[test]
fn custom_root_class_flows_through() {
    let theme = Theme {
        root_class: "prose".to_string(),
        ..Theme::default()
    };
    assert!(render("x", &theme).starts_with("<div class=\"prose\">"));
    assert!(stylesheet(&theme).contains(".prose code"));
}

#[test]
fn unknown_code_theme_falls_back_without_panic() {
    let theme = Theme {
        code_theme: "no-such-theme-xyz".to_string(),
        ..Theme::default()
    };
    let out = render("```rust\nfn a() {}\n```", &theme);
    assert!(out.contains("<pre style=\"background-color:#"));
}
