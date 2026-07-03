//! Visual tokens consumed by [`render`](crate::mdrender::render) and
//! [`stylesheet`](crate::mdrender::stylesheet).
//!
//! Every color field is an opaque CSS color *string*. It may be a literal
//! (`"#0D0D0D"`, `"rgb(13 13 13)"`) or a reference to a custom property defined
//! by the surrounding document (`"var(--fg)"`). The renderer never inspects the
//! value, so a host application can wire the tokens to its own theming system —
//! including OS light/dark flips driven by `prefers-color-scheme` — simply by
//! passing `var(...)` references whose values change per media query.

/// A palette + typography + naming bundle for the Markdown renderer.
///
/// Construct via [`Theme::default`] for a self-contained neutral look, or build
/// one explicitly to bind the tokens to an existing design system. All fields
/// are plain strings so the renderer stays agnostic of how colors are resolved.
#[derive(Debug, Clone)]
pub struct Theme {
    /// Background color of the rendered block.
    pub bg: String,
    /// Slightly raised surface color: inline-code chips and table header fill.
    pub surface: String,
    /// Primary body and heading text color.
    pub text: String,
    /// De-emphasized text color (captions, table body borders).
    pub muted: String,
    /// Accent color for links and small highlights.
    pub accent: String,
    /// Hairline border color for tables, code blocks and chips.
    pub border: String,
    /// Callout accent for `[!NOTE]` alerts.
    pub status_note: String,
    /// Callout accent for `[!TIP]` alerts.
    pub status_tip: String,
    /// Callout accent for `[!IMPORTANT]` alerts.
    pub status_important: String,
    /// Callout accent for `[!WARNING]` alerts.
    pub status_warning: String,
    /// Callout accent for `[!CAUTION]` alerts.
    pub status_caution: String,
    /// Font stack applied to headings.
    pub font_heading: String,
    /// Font stack applied to body text.
    pub font_body: String,
    /// Monospace font stack for inline code and code blocks.
    pub font_mono: String,
    /// CSS class the rendered fragment is wrapped in; also the selector scope
    /// emitted by [`stylesheet`](crate::mdrender::stylesheet). Keep it a valid,
    /// trusted CSS identifier — it is not derived from untrusted input.
    pub root_class: String,
    /// Name of a bundled syntect theme used for fenced-code highlighting.
    ///
    /// Must be one of the syntect default themes; an unknown name falls back to
    /// a known-good dark theme rather than panicking.
    pub code_theme: String,
}

impl Default for Theme {
    fn default() -> Self {
        Theme {
            bg: "#0D0D0D".to_string(),
            surface: "rgba(127,127,127,0.10)".to_string(),
            text: "#F5F5F0".to_string(),
            muted: "#a6a69c".to_string(),
            accent: "#B8FF00".to_string(),
            border: "rgba(127,127,127,0.28)".to_string(),
            status_note: "#4c8dff".to_string(),
            status_tip: "#3fb950".to_string(),
            status_important: "#a371f7".to_string(),
            status_warning: "#d29922".to_string(),
            status_caution: "#f0655e".to_string(),
            font_heading: "'Space Grotesk', system-ui, -apple-system, 'Segoe UI', sans-serif"
                .to_string(),
            font_body: "system-ui, -apple-system, 'Segoe UI', sans-serif".to_string(),
            font_mono: "'JetBrains Mono', ui-monospace, SFMono-Regular, Menlo, Consolas, monospace"
                .to_string(),
            root_class: "mdr".to_string(),
            code_theme: "base16-ocean.dark".to_string(),
        }
    }
}
