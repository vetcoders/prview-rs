//! Stylesheet generation for the renderer's output classes.

use super::theme::Theme;

/// Build the CSS for a [`Theme`], scoped under `.{root_class}`.
///
/// Inject the result once into a host document's `<style>`; it styles every
/// construct [`render`](crate::mdrender::render) can emit. Fenced-code colors
/// come from syntect's inline styles, so this sheet only supplies the code
/// block chrome (border, radius, scroll) and leaves the palette to the inline
/// spans.
pub(crate) fn stylesheet(theme: &Theme) -> String {
    let r = format!(".{}", theme.root_class);
    format!(
        r#"{r} {{ color:{text}; font-family:{body}; line-height:1.6; }}
{r} h1, {r} h2, {r} h3, {r} h4, {r} h5, {r} h6 {{ font-family:{heading}; letter-spacing:-0.02em; line-height:1.25; margin:1.4em 0 0.5em; }}
{r} h1 {{ font-size:1.7em; }}
{r} h2 {{ font-size:1.4em; }}
{r} h3 {{ font-size:1.18em; }}
{r} h4 {{ font-size:1.02em; }}
{r} p {{ margin:0.6em 0; }}
{r} a {{ color:{accent}; text-decoration:none; border-bottom:1px solid {border}; }}
{r} a:hover {{ border-bottom-color:{accent}; }}
{r} ul, {r} ol {{ margin:0.5em 0; padding-left:1.5em; }}
{r} li {{ margin:0.2em 0; }}
{r} blockquote {{ margin:0.8em 0; padding:0.2em 0 0.2em 1em; border-left:3px solid {border}; color:{muted}; }}
{r} hr {{ border:0; border-top:1px solid {border}; margin:1.6em 0; }}
{r} del {{ text-decoration:line-through; color:{muted}; }}
{r} code {{ font-family:{mono}; font-size:0.9em; background:{surface}; border:1px solid {border}; border-radius:5px; padding:0.12em 0.38em; }}
{r} pre {{ font-family:{mono}; overflow-x:auto; border:1px solid {border}; border-radius:8px; padding:12px 14px; margin:0.9em 0; line-height:1.45; }}
{r} pre code {{ background:none; border:0; border-radius:0; padding:0; font-size:0.86em; }}
{r} table {{ border-collapse:collapse; width:100%; margin:0.9em 0; font-size:0.94em; }}
{r} th, {r} td {{ border:1px solid {border}; padding:6px 10px; text-align:left; }}
{r} thead th {{ background:{surface}; font-weight:650; }}
{r} .contains-task-list {{ list-style:none; padding-left:0.2em; }}
{r} .task-list-item {{ margin:0.25em 0; }}
{r} .task-list-item-checkbox {{ accent-color:{accent}; margin-right:0.5em; vertical-align:middle; }}
{r} .markdown-alert {{ margin:1em 0; padding:0.6em 0.9em 0.6em 1em; border-left:3px solid {muted}; border-radius:0 8px 8px 0; background:{surface}; }}
{r} .markdown-alert > :first-child {{ margin-top:0; }}
{r} .markdown-alert > :last-child {{ margin-bottom:0; }}
{r} .markdown-alert-title {{ display:flex; align-items:center; gap:0.5em; font-weight:700; text-transform:none; margin-bottom:0.4em; }}
{r} .markdown-alert-title::before {{ content:""; display:inline-block; width:0.62em; height:0.62em; border-radius:50%; background:currentColor; }}
{r} .markdown-alert-note {{ border-left-color:{note}; background:color-mix(in srgb, {note} 10%, transparent); }}
{r} .markdown-alert-note .markdown-alert-title {{ color:{note}; }}
{r} .markdown-alert-tip {{ border-left-color:{tip}; background:color-mix(in srgb, {tip} 10%, transparent); }}
{r} .markdown-alert-tip .markdown-alert-title {{ color:{tip}; }}
{r} .markdown-alert-important {{ border-left-color:{important}; background:color-mix(in srgb, {important} 10%, transparent); }}
{r} .markdown-alert-important .markdown-alert-title {{ color:{important}; }}
{r} .markdown-alert-warning {{ border-left-color:{warning}; background:color-mix(in srgb, {warning} 10%, transparent); }}
{r} .markdown-alert-warning .markdown-alert-title {{ color:{warning}; }}
{r} .markdown-alert-caution {{ border-left-color:{caution}; background:color-mix(in srgb, {caution} 10%, transparent); }}
{r} .markdown-alert-caution .markdown-alert-title {{ color:{caution}; }}
"#,
        r = r,
        text = theme.text,
        body = theme.font_body,
        heading = theme.font_heading,
        mono = theme.font_mono,
        accent = theme.accent,
        border = theme.border,
        muted = theme.muted,
        surface = theme.surface,
        note = theme.status_note,
        tip = theme.status_tip,
        important = theme.status_important,
        warning = theme.status_warning,
        caution = theme.status_caution,
    )
}
