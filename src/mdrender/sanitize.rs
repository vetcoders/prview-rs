//! Output sanitization.
//!
//! comrak runs with raw-HTML passthrough enabled so that legitimate embedded
//! HTML (for example `<details>`/`<summary>` folds produced by trusted tooling)
//! survives. That makes the sanitizer the real security boundary: every render
//! is passed through an [`ammonia`] allowlist that keeps the tags/attributes the
//! renderer intentionally emits and strips everything else — `<script>`, event
//! handlers, `javascript:` URLs, and stray `style` properties.

use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

use ammonia::Builder;

/// Configured sanitizer, built once and reused across every [`clean`] call.
///
/// `ammonia::Builder::clean` takes `&self` and is designed for reuse, so we pay
/// the allowlist/collection construction cost a single time instead of on every
/// render.
static SANITIZER: LazyLock<Builder<'static>> = LazyLock::new(|| {
    let style_props: HashSet<&str> = [
        "color",
        "background-color",
        "font-weight",
        "font-style",
        "text-decoration",
        "text-decoration-line",
        "text-align",
    ]
    .into_iter()
    .collect();

    let allowed_classes: HashMap<&str, HashSet<&str>> = [
        (
            "div",
            [
                "markdown-alert",
                "markdown-alert-note",
                "markdown-alert-tip",
                "markdown-alert-important",
                "markdown-alert-warning",
                "markdown-alert-caution",
            ]
            .into_iter()
            .collect(),
        ),
        ("p", ["markdown-alert-title"].into_iter().collect()),
        ("ul", ["contains-task-list"].into_iter().collect()),
        ("li", ["task-list-item"].into_iter().collect()),
        ("input", ["task-list-item-checkbox"].into_iter().collect()),
    ]
    .into_iter()
    .collect();

    let mut builder = Builder::default();
    builder
        .add_tags(["input"])
        .add_tag_attributes("input", ["type", "checked", "disabled"])
        .add_tag_attributes("span", ["style"])
        .add_tag_attributes("pre", ["style"])
        .add_tag_attributes("code", ["style"])
        .add_tag_attributes("th", ["align"])
        .add_tag_attributes("td", ["align"])
        .filter_style_properties(style_props)
        .allowed_classes(allowed_classes);
    builder
});

/// Sanitize renderer output against a strict allowlist.
///
/// Preserves:
/// - GitHub-alert callout wrappers (`div.markdown-alert*`, `p.markdown-alert-title`),
/// - task-list markup (`ul.contains-task-list`, `li.task-list-item`,
///   `input.task-list-item-checkbox` with `type`/`checked`/`disabled`),
/// - syntect inline highlighting (`style` on `span`/`pre`/`code`, filtered to
///   color/background/weight/style/decoration/alignment properties),
/// - table cell alignment.
pub(crate) fn clean(html: &str) -> String {
    SANITIZER.clean(html).to_string()
}
