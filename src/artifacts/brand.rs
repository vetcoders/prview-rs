//! Single source of truth for the prview visual identity.
//!
//! Both HTML generators — the interactive dashboard ([`super::dashboard`]) and
//! the portable standard review ([`super::review_html`]) — build their CSS
//! custom-property palette from here, so the two artifacts render with byte-
//! identical brand tokens. The module exposes:
//!
//! * the palette anchors, AA-cleared status colors (dark + light) and font
//!   stacks as `&str` constants, and
//! * [`root_css`], which emits the shared `:root { … }` block (plus its
//!   `prefers-color-scheme: light` override) consumed verbatim by both files.
//!
//! Keeping the tokens in one place means a palette change lands in both
//! artifacts at once and can never drift between them.

use crate::mdrender::Theme;

// --- palette anchors -------------------------------------------------------

/// Near-black brand ink; page background in the dark theme.
pub(crate) const INK: &str = "#0D0D0D";
/// Mid graphite; neutral accent in the light theme.
pub(crate) const GRAPHITE: &str = "#2A2A2A";
/// Warm off-white paper; page background in the light theme.
pub(crate) const PAPER: &str = "#F5F5F0";
/// Lime signal — reserved for tiny, surprising accents (never large fills).
pub(crate) const SIGNAL: &str = "#B8FF00";

// --- neutral text / accent -------------------------------------------------

/// De-emphasized text on the dark theme.
pub(crate) const MUTED_DARK: &str = "#a6a69c";
/// De-emphasized text on the light theme.
pub(crate) const MUTED_LIGHT: &str = "#52514e";
/// Neutral accent (dir names, links) on the dark theme.
pub(crate) const ACCENT_DARK: &str = "#cfcfc6";

// --- statuses: dark theme (AA on ink) --------------------------------------

/// PASS / success status on the dark theme.
pub(crate) const PASS_DARK: &str = "#3fb950";
/// WARN status on the dark theme.
pub(crate) const WARN_DARK: &str = "#d29922";
/// HOLD status on the dark theme.
pub(crate) const HOLD_DARK: &str = "#e0a92e";
/// BLOCK / error status on the dark theme.
pub(crate) const BLOCK_DARK: &str = "#f0655e";

// --- statuses: light theme (AA on paper) -----------------------------------

/// PASS / success status on the light theme.
pub(crate) const PASS_LIGHT: &str = "#0a7d2e";
/// WARN status on the light theme.
pub(crate) const WARN_LIGHT: &str = "#8a6300";
/// HOLD status on the light theme.
pub(crate) const HOLD_LIGHT: &str = "#8a6300";
/// BLOCK / error status on the light theme.
pub(crate) const BLOCK_LIGHT: &str = "#c0342e";

// --- typography ------------------------------------------------------------

/// Display font stack for headings and the wordmark.
pub(crate) const FONT_HEADING: &str =
    "'Space Grotesk', system-ui, -apple-system, 'Segoe UI', sans-serif";
/// Monospace stack for code, metrics and technical chrome.
pub(crate) const FONT_MONO: &str =
    "'JetBrains Mono', ui-monospace, SFMono-Regular, Menlo, Consolas, monospace";

/// Emit the shared `:root` custom-property block (dark defaults +
/// `prefers-color-scheme: light` override).
///
/// This is the canonical brand palette both HTML artifacts inject verbatim.
/// Variable names are stable and shared: `--bg`, `--surface`, `--fg`,
/// `--muted`, `--line`, `--accent`, `--signal`, `--pass`, `--warn`, `--hold`,
/// `--block`, `--font-heading`, `--mono` (plus the `--ink/--graphite/--paper`
/// anchors and the `--veil` elevation base).
pub(crate) fn root_css() -> String {
    format!(
        r#":root {{
  color-scheme: dark;
  --ink:{ink}; --graphite:{graphite}; --paper:{paper}; --signal:{signal};
  --veil:255,255,255;
  --bg:{ink}; --surface:rgba(var(--veil),0.05); --fg:{paper};
  --muted:{muted_d}; --line:rgba(var(--veil),0.12); --accent:{accent_d};
  --pass:{pass_d}; --warn:{warn_d}; --hold:{hold_d}; --block:{block_d};
  --font-heading:{font_heading};
  --mono:{font_mono};
}}
@media (prefers-color-scheme: light) {{ :root {{
  color-scheme: light;
  --veil:13,13,13;
  --bg:{paper}; --surface:#ffffff; --fg:{ink};
  --muted:{muted_l}; --line:rgba(var(--veil),0.12); --accent:{graphite};
  --pass:{pass_l}; --warn:{warn_l}; --hold:{hold_l}; --block:{block_l};
}} }}
"#,
        ink = INK,
        graphite = GRAPHITE,
        paper = PAPER,
        signal = SIGNAL,
        muted_d = MUTED_DARK,
        muted_l = MUTED_LIGHT,
        accent_d = ACCENT_DARK,
        pass_d = PASS_DARK,
        warn_d = WARN_DARK,
        hold_d = HOLD_DARK,
        block_d = BLOCK_DARK,
        pass_l = PASS_LIGHT,
        warn_l = WARN_LIGHT,
        hold_l = HOLD_LIGHT,
        block_l = BLOCK_LIGHT,
        font_heading = FONT_HEADING,
        font_mono = FONT_MONO,
    )
}

/// Brand-aligned [`Theme`] for the portable standard-review export.
///
/// Color tokens reference the shared `:root` variables (see [`root_css`]), so
/// rendered markdown flips with the same OS light/dark system as the rest of
/// the artifact. `--signal` stays reserved for tiny accents: links use the
/// foreground color and callouts reuse the AA-cleared status variables rather
/// than the bright signal.
pub(crate) fn review_theme() -> Theme {
    Theme {
        bg: "var(--bg)".to_string(),
        surface: "var(--surface)".to_string(),
        text: "var(--fg)".to_string(),
        muted: "var(--muted)".to_string(),
        accent: "var(--fg)".to_string(),
        border: "var(--line)".to_string(),
        status_note: "var(--muted)".to_string(),
        status_tip: "var(--pass)".to_string(),
        status_important: "var(--hold)".to_string(),
        status_warning: "var(--warn)".to_string(),
        status_caution: "var(--block)".to_string(),
        font_heading: "var(--font-heading)".to_string(),
        font_body: "inherit".to_string(),
        font_mono: "var(--mono)".to_string(),
        root_class: "mdr".to_string(),
        code_theme: "base16-ocean.dark".to_string(),
    }
}

/// Emit the discreet shared brand footer used at the bottom of both artifacts.
///
/// Renders the `pull request · rust cli · diff intelligence` tagline (mono,
/// muted, small) with lime signal dots as separators, and a fainter
/// `powered by prview-rs` line beneath. Self-contained inline styles reference
/// only the shared brand tokens (`--line`, `--mono`, `--muted`, `--signal`),
/// so the fragment renders identically in the dashboard and the standard
/// review without extra CSS wiring.
pub(crate) fn mini_footer_html() -> String {
    let sep = r#"<span style="color:var(--signal)">&nbsp;&middot;&nbsp;</span>"#;
    format!(
        r#"<footer style="margin:44px 0 8px;padding-top:18px;border-top:1px solid var(--line);text-align:center;font-family:var(--mono);font-size:11px;line-height:1.7">
<div style="color:var(--muted);letter-spacing:0.04em">pull request{sep}rust cli{sep}diff intelligence</div>
<div style="color:var(--muted);opacity:0.7;margin-top:4px">powered by prview-rs</div>
</footer>"#
    )
}

/// Brand-aligned [`Theme`] for the dashboard narrative section.
///
/// Same shared tokens as [`review_theme`]; the narrative uses the neutral
/// accent for emphasis callouts and reserves `--signal` for chrome accents.
pub(crate) fn dashboard_narrative_theme() -> Theme {
    Theme {
        bg: "var(--bg)".to_string(),
        surface: "var(--surface-2)".to_string(),
        text: "var(--fg)".to_string(),
        muted: "var(--muted)".to_string(),
        accent: "var(--accent)".to_string(),
        border: "var(--line)".to_string(),
        status_note: "var(--muted)".to_string(),
        status_tip: "var(--pass)".to_string(),
        status_important: "var(--accent)".to_string(),
        status_warning: "var(--warn)".to_string(),
        status_caution: "var(--block)".to_string(),
        font_heading: "var(--font-heading)".to_string(),
        font_body: "inherit".to_string(),
        font_mono: "var(--mono)".to_string(),
        root_class: "mdr".to_string(),
        code_theme: "base16-ocean.dark".to_string(),
    }
}
