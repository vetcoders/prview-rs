//! Self-contained Markdown-to-HTML rendering for static artifacts.
//!
//! `mdrender` turns a Markdown string into a sanitized HTML fragment plus a
//! matching stylesheet, with no external runtime assets (no CDN, no JavaScript,
//! no web fonts of its own). It is deliberately independent of the rest of the
//! crate — it imports nothing from its host and communicates purely through
//! [`Theme`] tokens in and `String` HTML/CSS out — so it can later be lifted
//! into a standalone crate unchanged.
//!
//! # Pipeline
//!
//! 1. [`comrak`] parses GitHub-Flavored Markdown (tables, task lists,
//!    strikethrough, autolinks, and `[!NOTE]`-style alert callouts) with raw
//!    HTML passthrough enabled.
//! 2. [`syntect`](comrak::plugins::syntect) highlights fenced code into
//!    self-contained inline `<span style>` runs.
//! 3. [`ammonia`] sanitizes the result against a strict allowlist, which is the
//!    security boundary: `<script>`, event handlers and `javascript:` URLs are
//!    stripped while the intentional markup survives.
//!
//! # Example
//!
//! ```
//! use prview::mdrender::{render, stylesheet, Theme};
//!
//! let theme = Theme::default();
//! let css = stylesheet(&theme);
//! let html = render("# Title\n\n- [x] done\n", &theme);
//! assert!(html.starts_with("<div class=\"mdr\">"));
//! assert!(css.contains(".mdr"));
//! ```

mod css;
mod highlight;
mod sanitize;
mod theme;

#[cfg(test)]
mod tests;

pub use theme::Theme;

use comrak::Options;
use comrak::options::Plugins;

/// Render a Markdown string to a sanitized, self-contained HTML fragment.
///
/// The output is wrapped in a single `<div class="{root_class}">` (see
/// [`Theme::root_class`]) so the companion [`stylesheet`] can scope every rule.
/// Pair one `stylesheet` call (injected once into the host `<style>`) with any
/// number of `render` calls sharing the same [`Theme`].
pub fn render(markdown: &str, theme: &Theme) -> String {
    let mut options = Options::default();
    options.extension.strikethrough = true;
    options.extension.table = true;
    options.extension.autolink = true;
    options.extension.tasklist = true;
    options.extension.alerts = true;
    // Emit stable task-list classes so the stylesheet can target checkboxes.
    options.render.tasklist_classes = true;
    // Pass raw HTML through comrak; `sanitize::clean` is the trusted boundary.
    options.render.r#unsafe = true;

    let adapter = highlight::adapter_for(&theme.code_theme);
    let mut plugins = Plugins::default();
    plugins.render.codefence_syntax_highlighter = Some(adapter.as_ref());

    let raw = comrak::markdown_to_html_with_plugins(markdown, &options, &plugins);
    let clean = sanitize::clean(&raw);

    format!("<div class=\"{}\">{}</div>", theme.root_class, clean)
}

/// Build the CSS for a [`Theme`], scoped under `.{root_class}`.
///
/// Inject the returned block once into the host document's `<style>`; it styles
/// every construct [`render`] can produce.
pub fn stylesheet(theme: &Theme) -> String {
    css::stylesheet(theme)
}
