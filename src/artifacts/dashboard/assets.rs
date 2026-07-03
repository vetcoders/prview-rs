//! Static CSS and JavaScript assets embedded in the HTML dashboard.

// ---------------------------------------------------------------------------
// CSS
// ---------------------------------------------------------------------------

pub(super) fn css() -> String {
    format!("{}{}", crate::artifacts::brand::root_css(), STATIC_CSS)
}

/// Dashboard styling: dashboard-only supplementary tokens (layered on the
/// shared brand `:root` from [`crate::artifacts::brand::root_css`]) followed by
/// every dashboard rule.
const STATIC_CSS: &str = r#"
/* Supplementary tokens specific to the dashboard chrome. The shared brand
   palette (--bg/--surface/--fg/--muted/--line/--accent/--signal/--pass/--warn/
   --hold/--block/--font-heading/--mono plus --ink/--graphite/--paper/--veil)
   is injected ahead of this block by brand::root_css(). */
:root {
    --surface-2: rgba(var(--veil),0.06);
    --hover: rgba(var(--veil),0.05);
    --faint: #6b6b63;
    --line-strong: rgba(var(--veil),0.14);
    --accent-dim: rgba(var(--veil),0.08);
    --serious: #f0883e;
    --serious-dim: rgba(240,136,62,0.22);
    --pass-dim: rgba(63,185,80,0.25);
    --warn-dim: rgba(210,153,34,0.25);
    --block-dim: rgba(248,81,73,0.25);
    --added: #238636;
    --added-light: #2ea043;
    --deleted: #da3633;
    --deleted-light: #f0655e;
    --glass-bg: rgba(var(--veil),0.05);
    --glass-border: rgba(var(--veil),0.09);
    --glass-blur: 16px;
    --glass-shadow: 0 10px 40px rgba(0,0,0,0.55);
    --glass-inset: inset 0 1px 0 rgba(var(--veil),0.05);
    --radius: 18px;
    --radius-sm: 8px;
    /* signal accent, used only as small surprising touches (never large fills) */
    --signal-soft: rgba(184,255,0,0.14);
    --signal-line: rgba(184,255,0,0.55);
}

/* === Light theme supplements (paper page / white panels), OS-preference driven === */
@media (prefers-color-scheme: light) {
    :root {
        --surface-2: #ffffff;
        --hover: rgba(var(--veil),0.04);
        --faint: #6b6b64;
        --line-strong: rgba(var(--veil),0.18);
        --serious: #9a4510;
        --serious-dim: rgba(154,69,16,0.12);
        --pass-dim: rgba(10,125,46,0.12);
        --warn-dim: rgba(138,99,0,0.12);
        --block-dim: rgba(192,52,46,0.12);
        --added: #0a7d2e;
        --added-light: #0a7d2e;
        --deleted: #c0342e;
        --deleted-light: #c0342e;
        --glass-bg: #ffffff;
        --glass-border: rgba(var(--veil),0.10);
        --glass-shadow: 0 8px 28px rgba(13,13,13,0.10);
        --glass-inset: inset 0 1px 0 rgba(254,254,254,0.7);
    }
}

*, *::before, *::after { box-sizing: border-box; margin: 0; padding: 0; }

body {
    font-family: system-ui, -apple-system, 'Segoe UI', sans-serif;
    background:
        radial-gradient(circle at 50% 40%, rgba(var(--veil),0.08), transparent 40%),
        radial-gradient(circle at 50% 80%, rgba(var(--veil),0.04), transparent 50%),
        var(--bg);
    color: var(--fg);
    line-height: 1.6;
    padding: 24px;
    min-height: 100vh;
}
body::before {
    content: "";
    position: fixed;
    inset: 0;
    background: radial-gradient(circle at 50% 40%, rgba(var(--veil),0.05), transparent 40%);
    pointer-events: none;
    z-index: 0;
}

.container {
    width: min(100%, 1440px);
    margin: 0 auto;
    position: relative;
    z-index: 1;
}

/* ---- HEADER ---- */
.header {
    display: flex;
    justify-content: space-between;
    align-items: flex-start;
    gap: 16px;
    padding-bottom: 20px;
    border-bottom: 1px solid var(--line);
    margin-bottom: 24px;
    flex-wrap: wrap;
}
.header-left {
    display: flex;
    flex: 1 1 560px;
    min-width: 0;
    flex-direction: column;
    gap: 10px;
}
.header-title {
    font-family: var(--mono);
    font-size: 11px;
    font-weight: 700;
    letter-spacing: 0.12em;
    text-transform: uppercase;
    color: var(--faint);
    display: flex;
    align-items: center;
    gap: 10px;
    flex-wrap: wrap;
}
h1, h2, h3, h4 { font-family: var(--font-heading); letter-spacing: -0.02em; }
code, pre { font-family: var(--mono); }
/* Brand wordmark: shared lockup with review.html (Space Grotesk + signal dot). */
.wordmark {
    font-family: var(--font-heading);
    font-weight: 700;
    font-size: 19px;
    letter-spacing: -0.03em;
    line-height: 1;
    text-transform: none;
    color: var(--fg);
    display: inline-flex;
    align-items: baseline;
    gap: 3px;
}
.wordmark .dot {
    width: 6px;
    height: 6px;
    border-radius: 50%;
    background: var(--signal);
    display: inline-block;
}
.header-title a { color: var(--accent); text-decoration: none; }
.header-title a:hover { color: var(--fg); text-decoration: underline; text-decoration-color: var(--signal); }
.dashboard-context {
    display: flex;
    flex-direction: column;
    gap: 10px;
    min-width: 0;
}
.dashboard-context-repo {
    font-size: 31px;
    line-height: 1.05;
    font-weight: 700;
    letter-spacing: -0.03em;
    color: var(--fg);
    overflow-wrap: anywhere;
}
.dashboard-context-flow {
    display: flex;
    align-items: center;
    gap: 8px;
    flex-wrap: wrap;
}
.dashboard-context-arrow {
    color: var(--faint);
    font-family: var(--mono);
}
.dashboard-context-meta {
    display: flex;
    align-items: center;
    gap: 8px;
    flex-wrap: wrap;
}
.context-pill {
    display: inline-flex;
    align-items: center;
    gap: 8px;
    min-width: 0;
    padding: 6px 10px;
    border-radius: 999px;
    background: rgba(var(--veil),0.04);
    border: 1px solid rgba(var(--veil),0.08);
}
.context-pill-label {
    font-size: 10px;
    font-weight: 700;
    text-transform: uppercase;
    letter-spacing: 0.08em;
    color: var(--faint);
    white-space: nowrap;
}
.context-pill code,
.context-pill a {
    color: var(--fg);
    font-family: var(--mono);
    font-size: 12px;
    text-decoration: none;
}
.context-pill a:hover { text-decoration: underline; }
.header-meta {
    color: var(--muted);
    font-size: 13px;
    display: flex;
    align-items: center;
    gap: 8px;
    flex-wrap: wrap;
}
.header-right {
    display: flex;
    align-items: flex-end;
    justify-content: flex-end;
    gap: 12px;
    flex-wrap: wrap;
    flex: 1 1 420px;
    min-width: 0;
}
.header-actions {
    display: flex;
    align-items: center;
    justify-content: flex-end;
    gap: 10px;
    flex-wrap: wrap;
    width: 100%;
}
.header-profile-badge { flex-shrink: 0; }
.ref-arrow {
    display: inline-flex;
    align-items: center;
    gap: 6px;
    font-family: var(--mono);
    font-size: 13px;
    color: var(--muted);
}
.ref-arrow .ref-name {
    background: var(--surface-2);
    padding: 2px 8px;
    border-radius: var(--radius-sm);
    color: var(--accent);
}

/* ---- BADGES ---- */
.badge {
    display: inline-block;
    padding: 4px 10px;
    border-radius: 8px;
    font-size: 12px;
    font-weight: 500;
    white-space: nowrap;
    background: rgba(var(--veil),0.06);
    border: 1px solid rgba(var(--veil),0.12);
}
.badge-success { background: rgba(var(--veil),0.06); color: var(--muted); border-color: rgba(var(--veil),0.12); }
.badge-warning { background: transparent; color: var(--warn); border-color: var(--warn); }
.badge-error   { background: transparent; color: var(--block); border-color: var(--block); }
.badge-info    { background: rgba(var(--veil),0.06); color: var(--muted); border-color: rgba(var(--veil),0.12); }
.badge-muted   { background: rgba(var(--veil),0.04); color: var(--faint); border-color: rgba(var(--veil),0.08); }
.badge-hotspot { background: transparent; color: var(--serious); border-color: var(--serious); font-size: 10px; padding: 1px 6px; }
.badge-blocking { background: transparent; color: var(--block); border-color: var(--block); font-size: 10px; padding: 2px 7px; }
.badge-nonblocking { background: rgba(var(--veil),0.04); color: var(--muted); border-color: rgba(var(--veil),0.08); font-size: 10px; padding: 2px 7px; }
.badge-confidence-high { background: transparent; color: var(--pass); border-color: var(--pass); font-size: 10px; padding: 2px 7px; }
.badge-confidence-medium { background: transparent; color: var(--warn); border-color: var(--warn); font-size: 10px; padding: 2px 7px; }
.badge-confidence-low { background: rgba(var(--veil),0.04); color: var(--muted); border-color: rgba(var(--veil),0.08); font-size: 10px; padding: 2px 7px; }

/* ---- SECURITY SECTION ---- */
.security-grid {
    display: grid;
    grid-template-columns: repeat(auto-fit, minmax(280px, 1fr));
    gap: 12px;
}
.security-card {
    background: var(--glass-bg);
    backdrop-filter: blur(var(--glass-blur));
    -webkit-backdrop-filter: blur(var(--glass-blur));
    border: 1px solid var(--glass-border);
    border-radius: var(--radius);
    padding: 14px 18px;
    box-shadow: var(--glass-shadow), var(--glass-inset);
    border-left: 4px solid var(--line);
}
.security-card.sec-pass { border-left-color: var(--pass); }
.security-card.sec-warn { border-left-color: var(--warn); }
.security-card.sec-fail { border-left-color: var(--block); }
.security-card.sec-error { border-left-color: var(--faint); }
.security-card-header {
    display: flex;
    align-items: center;
    gap: 8px;
    margin-bottom: 6px;
    font-size: 13px;
    font-weight: 600;
}
.security-card-metric {
    font-size: 12px;
    color: var(--muted);
    font-family: var(--mono);
}
.security-card-link {
    font-size: 11px;
    margin-top: 4px;
}
.security-card-link a { color: var(--accent); text-decoration: none; }
.security-card-link a:hover { text-decoration: underline; }

/* ---- MERGE DECISION ---- */
.merge-decision {
    display: flex;
    align-items: center;
    justify-content: flex-end;
    gap: 12px;
    flex-wrap: wrap;
    min-width: 0;
}
.merge-chip {
    display: inline-flex;
    align-items: center;
    gap: 6px;
    padding: 8px 16px;
    border-radius: 24px;
    font-size: 14px;
    font-weight: 700;
    letter-spacing: 0.5px;
    backdrop-filter: blur(8px);
    -webkit-backdrop-filter: blur(8px);
    border: 1px solid rgba(var(--veil),0.12);
}
.merge-allow { background: transparent; color: var(--pass); border-color: var(--pass); }
.merge-block { background: transparent; color: var(--block); border-color: var(--block); }
.merge-na { background: rgba(var(--veil),0.06); color: var(--muted); }
.merge-policy {
    font-size: 12px;
    color: var(--muted);
    display: flex;
    flex-direction: column;
    gap: 2px;
}
.merge-policy-line {
    display: flex;
    align-items: flex-start;
    justify-content: flex-end;
    gap: 8px;
    flex-wrap: wrap;
}
.merge-policy-label {
    white-space: nowrap;
}
.merge-policy-value {
    text-align: right;
}
/* Review signals: amber is reserved for the small marker only. The compact
   header count reads muted; the full caveat prose in the card reads as --fg. */
.review-signal-compact { color: var(--muted); }
.review-signal-dot {
    width: 7px;
    height: 7px;
    margin-top: 5px;
    border-radius: 50%;
    background: var(--warn);
    flex: 0 0 auto;
}
.review-signal-label { color: var(--warn); }
.review-signal-prose { color: var(--fg); }

/* ---- CARDS ROW ---- */
.cards-row {
    display: grid;
    grid-template-columns: repeat(auto-fit, minmax(160px, 1fr));
    gap: 16px;
    margin-bottom: 20px;
}
.card {
    background: var(--glass-bg);
    backdrop-filter: blur(var(--glass-blur));
    -webkit-backdrop-filter: blur(var(--glass-blur));
    border: 1px solid var(--glass-border);
    border-radius: var(--radius);
    padding: 18px 20px;
    box-shadow: 0 8px 26px rgba(0,0,0,0.35), var(--glass-inset);
}
.card-label {
    font-size: 11px;
    color: var(--muted);
    text-transform: uppercase;
    letter-spacing: 0.5px;
    margin-bottom: 6px;
}
.card-value {
    font-size: 28px;
    font-weight: 700;
    font-variant-numeric: tabular-nums;
    color: var(--fg);
}
.card-value.green { color: var(--pass); }
.card-value.red   { color: var(--block); }

/* ---- ACTION CENTER ---- */
.action-card {
    background: rgba(var(--veil),0.035);
    backdrop-filter: blur(calc(var(--glass-blur) - 4px));
    -webkit-backdrop-filter: blur(calc(var(--glass-blur) - 4px));
    border: 1px solid var(--glass-border);
    border-radius: calc(var(--radius) - 2px);
    padding: 12px 14px;
}
.action-card.alert-error { border-color: rgba(248,81,73,0.28); }
.action-card.alert-warning { border-color: rgba(210,153,34,0.26); }
.action-card.alert-success { border-color: rgba(63,185,80,0.22); }
.action-card.alert-info { border-color: rgba(var(--veil),0.14); }
.action-card-header {
    display: flex;
    align-items: center;
    gap: 8px;
    margin-bottom: 6px;
    font-size: 12px;
    font-weight: 600;
}
.action-card-body {
    font-size: 12px;
    color: var(--muted);
    line-height: 1.45;
}
.action-card-body code {
    font-family: var(--mono);
    background: var(--surface-2);
    padding: 1px 5px;
    border-radius: 3px;
    font-size: 11px;
}

/* ---- SECTIONS ---- */
.section { margin-bottom: 28px; }
.section-header {
    display: flex;
    justify-content: space-between;
    align-items: center;
    margin-bottom: 12px;
    padding-bottom: 8px;
    border-bottom: 1px solid rgba(var(--veil),0.06);
    flex-wrap: wrap;
    gap: 8px;
}
.section-title {
    font-size: 16px;
    font-weight: 500;
    letter-spacing: -0.01em;
}
.section-count {
    font-size: 12px;
    color: var(--muted);
    background: var(--surface-2);
    padding: 2px 8px;
    border-radius: 10px;
}

.tier-one-stack {
    margin-bottom: 28px;
}
.tier-one-grid {
    display: grid;
    grid-template-columns: minmax(0, 2.2fr) minmax(280px, 1fr);
    gap: 16px;
    align-items: start;
    margin-bottom: 14px;
}
.tier-one-rail {
    display: grid;
    gap: 16px;
}
.top-section-anchor {
    scroll-margin-top: 24px;
}
.files-summary-card {
    display: grid;
    gap: 12px;
}
.files-summary-header {
    display: flex;
    justify-content: space-between;
    align-items: flex-start;
    gap: 12px;
}
.files-summary-title {
    font-size: 12px;
    text-transform: uppercase;
    letter-spacing: 0.08em;
    color: var(--muted);
}
.files-summary-total {
    font-family: var(--mono);
    font-size: 28px;
    font-weight: 700;
}
.files-summary-breakdown {
    display: grid;
    grid-template-columns: repeat(3, minmax(0, 1fr));
    gap: 10px;
}
.files-summary-stat {
    padding-top: 10px;
    border-top: 1px solid rgba(var(--veil),0.06);
}
.files-summary-stat strong {
    display: block;
    font-family: var(--mono);
    font-size: 16px;
    color: var(--fg);
}
.files-summary-stat span {
    font-size: 12px;
    color: var(--muted);
}

/* ---- CHECKS TABLE ---- */
.checks-table { width: 100%; border-collapse: collapse; }
.checks-table th {
    text-align: left;
    padding: 10px 12px;
    font-size: 11px;
    color: var(--muted);
    text-transform: uppercase;
    letter-spacing: 0.5px;
    border-bottom: 1px solid var(--line);
}
.checks-table td { padding: 10px 12px; border-bottom: 1px solid var(--line); }
.check-row { cursor: default; transition: background 0.2s ease; }
.check-row.expandable { cursor: pointer; }
.check-row:hover { background: rgba(var(--veil),0.05); }
.check-name-cell {
    display: flex;
    align-items: center;
    gap: 8px;
}
.check-icon {
    width: 22px;
    height: 22px;
    display: inline-flex;
    align-items: center;
    justify-content: center;
    border-radius: 50%;
    font-size: 12px;
}
.check-icon.passed  { background: rgba(63,185,80,0.2); color: var(--pass); }
.check-icon.failed  { background: rgba(248,81,73,0.2); color: var(--block); }
.check-icon.warnings{ background: rgba(210,153,34,0.2); color: var(--warn); }
.check-icon.skipped { background: rgba(var(--veil),0.06); color: var(--muted); }
.check-icon.error   { background: rgba(248,81,73,0.2); color: var(--block); }
.check-expand {
    color: var(--faint);
    font-size: 10px;
    transition: transform 0.2s;
    display: inline-block;
    width: 16px;
    text-align: center;
}
.check-expand.open { transform: rotate(90deg); }
.check-output {
    display: none;
    padding: 0;
}
.check-output.open { display: table-row; }
.check-output td {
    padding: 0;
    border-bottom: 1px solid var(--line);
}
.check-output-inner {
    background: var(--bg);
    padding: 12px 16px;
    max-height: 360px;
    overflow: auto;
    border-left: 3px solid var(--line-strong);
    margin: 0 12px 12px 12px;
    border-radius: 0 0 var(--radius-sm) var(--radius-sm);
}
.check-output-inner pre {
    font-family: var(--mono);
    font-size: 12px;
    color: var(--muted);
    white-space: pre-wrap;
    overflow-wrap: break-word;
    word-break: break-word;
    margin: 0;
}
.check-meta {
    display: flex;
    gap: 8px;
    flex-wrap: wrap;
    align-items: center;
}

/* ---- FILES SECTION ---- */
.file-toggle {
    display: flex;
    gap: 4px;
    margin-bottom: 12px;
    flex-wrap: wrap;
}
.file-toggle .toggle-btn {
    padding: 6px 14px;
    border: 1px solid var(--line);
    border-radius: 20px;
    background: none;
    color: var(--muted);
    font-size: 13px;
    cursor: pointer;
    font-family: inherit;
    transition: all 0.2s;
}
.file-toggle .toggle-btn:hover {
    color: var(--fg);
    border-color: var(--faint);
}
.file-toggle .toggle-btn.active {
    background: rgba(var(--veil),0.10);
    color: var(--fg);
    border-color: var(--line-strong);
}
.files-toolbar {
    display: flex;
    flex-wrap: wrap;
    gap: 10px;
    align-items: center;
    margin-bottom: 12px;
}
.file-search {
    flex: 1;
    min-width: 200px;
    max-width: 400px;
    padding: 8px 12px;
    background: var(--bg);
    border: 1px solid var(--line);
    border-radius: var(--radius);
    color: var(--fg);
    font-size: 13px;
    font-family: var(--mono);
    outline: none;
    transition: border-color 0.2s;
}
.file-search:focus { border-color: var(--accent); }
.file-search::placeholder { color: var(--faint); }
.filter-chips {
    display: flex;
    gap: 6px;
    flex-wrap: wrap;
}
.filter-chip {
    padding: 4px 10px;
    border-radius: 14px;
    font-size: 11px;
    font-weight: 600;
    cursor: pointer;
    user-select: none;
    border: 1px solid var(--line);
    background: var(--surface-2);
    color: var(--muted);
    transition: all 0.15s;
}
.filter-chip:hover { border-color: var(--accent); color: var(--fg); }
.filter-chip.active { background: rgba(var(--veil),0.1); border-color: rgba(var(--veil),0.2); color: var(--fg); }
.sort-select {
    padding: 4px 8px;
    background: var(--surface-2);
    border: 1px solid var(--line);
    border-radius: var(--radius-sm);
    color: var(--fg);
    font-size: 12px;
    outline: none;
}

.dir-group { margin-bottom: 2px; }
.dir-header {
    display: flex;
    align-items: center;
    gap: 8px;
    padding: 8px 12px;
    background: var(--surface-2);
    border-radius: var(--radius-sm);
    cursor: pointer;
    user-select: none;
    transition: background 0.15s;
    font-size: 13px;
}
.dir-header:hover { background: rgba(var(--veil),0.08); }
.dir-chevron {
    font-size: 10px;
    transition: transform 0.2s;
    display: inline-block;
    width: 14px;
    color: var(--faint);
}
.dir-chevron.open { transform: rotate(90deg); }
.dir-name {
    font-family: var(--mono);
    font-weight: 600;
    color: var(--accent);
    flex: 1;
}
.dir-stats {
    font-size: 12px;
    color: var(--muted);
    display: flex;
    gap: 8px;
}
.dir-files-wrap {
    overflow: hidden;
    transition: max-height 0.25s ease;
}
.dir-files-wrap.collapsed { max-height: 0 !important; }

.file-row {
    display: grid;
    grid-template-columns: 32px 1fr 110px 70px 70px;
    align-items: center;
    gap: 8px;
    padding: 6px 12px 6px 36px;
    border-bottom: 1px solid var(--surface-2);
    font-size: 13px;
    transition: background 0.1s;
}
.file-row:hover { background: rgba(var(--veil),0.05); transition: background 0.2s ease; }
.file-row.has-diff { cursor: pointer; }
.file-badge {
    display: inline-flex;
    align-items: center;
    justify-content: center;
    width: 24px;
    height: 20px;
    border-radius: var(--radius-sm);
    font-size: 11px;
    font-weight: 700;
    color: var(--fg);
    background: rgba(var(--veil),0.10);
}
.file-badge.A { background: rgba(var(--veil),0.14); }
.file-badge.M { background: rgba(var(--veil),0.08); }
.file-badge.D { background: rgba(var(--veil),0.14); }
.file-badge.R { background: rgba(var(--veil),0.08); }
.file-badge.C { background: rgba(var(--veil),0.08); }
.file-name {
    font-family: var(--mono);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    display: flex;
    align-items: center;
    gap: 6px;
}
.file-name a { color: var(--fg); text-decoration: none; }
.file-name a:hover { color: var(--accent); text-decoration: underline; }
.diff-bar {
    display: flex;
    height: 8px;
    border-radius: 4px;
    overflow: hidden;
    background: var(--bg);
}
.diff-bar-add { background: rgba(var(--veil),0.45); }
.diff-bar-del { background: rgba(var(--veil),0.22); }
.file-adds { color: var(--muted); text-align: right; font-family: var(--mono); font-size: 12px; }
.file-dels { color: var(--faint);   text-align: right; font-family: var(--mono); font-size: 12px; }

/* ---- DISTRIBUTION ---- */
.dist-chart { display: flex; flex-direction: column; gap: 6px; }
.dist-row {
    display: grid;
    grid-template-columns: 180px 1fr 60px;
    align-items: center;
    gap: 12px;
    font-size: 13px;
}
.dist-label {
    font-family: var(--mono);
    color: var(--accent);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
}
.dist-bar-wrap {
    height: 18px;
    background: var(--bg);
    border-radius: 4px;
    overflow: hidden;
    display: flex;
}
.dist-bar-add { background: rgba(var(--veil),0.45); transition: width 0.4s ease; }
.dist-bar-del { background: rgba(var(--veil),0.22); transition: width 0.4s ease; }
.dist-count { text-align: right; color: var(--muted); font-family: var(--mono); font-size: 12px; }

/* ---- COMMITS ---- */
.commits-list {
    max-height: 420px;
    overflow-y: auto;
    border-left: 2px solid var(--line-strong);
    margin-left: 6px;
    padding-left: 0;
}
.commit-item {
    display: grid;
    grid-template-columns: auto 1fr auto;
    gap: 10px;
    padding: 10px 14px;
    position: relative;
    transition: background 0.1s;
}
.commit-item:hover { background: rgba(var(--veil),0.05); transition: background 0.2s ease; }
.commit-dot {
    width: 10px;
    height: 10px;
    border-radius: 50%;
    background: var(--accent-dim);
    border: 2px solid var(--accent);
    margin-top: 6px;
    margin-left: -7px;
}
.commit-body { display: flex; flex-direction: column; gap: 2px; }
.commit-msg { font-size: 14px; }
.commit-meta {
    font-size: 12px;
    color: var(--muted);
    display: flex;
    gap: 8px;
    flex-wrap: wrap;
}
.commit-hash {
    font-family: var(--mono);
    font-size: 12px;
    color: var(--accent);
    text-decoration: none;
    background: var(--surface-2);
    padding: 2px 6px;
    border-radius: var(--radius-sm);
    align-self: flex-start;
    margin-top: 4px;
}
.commit-hash:hover { text-decoration: underline; }

/* ---- LOCTREE ---- */
.loctree-grid {
    display: grid;
    grid-template-columns: repeat(auto-fit, minmax(380px, 1fr));
    gap: 16px;
}
.lang-bar-wrap { display: flex; flex-direction: column; gap: 4px; }
.lang-row {
    display: grid;
    grid-template-columns: 110px 1fr 80px;
    align-items: center;
    gap: 10px;
    font-size: 13px;
}
.lang-pill {
    display: inline-block;
    padding: 2px 8px;
    border-radius: 10px;
    font-size: 11px;
    font-weight: 600;
    color: #fff;
}
.lang-bar {
    height: 14px;
    border-radius: 4px;
    transition: width 0.4s ease;
}
.lang-count { color: var(--muted); font-family: var(--mono); font-size: 12px; text-align: right; }

.issue-card {
    padding: 10px 14px;
    background: var(--glass-bg);
    border: 1px solid var(--glass-border);
    border-radius: var(--radius-sm);
    margin-bottom: 8px;
    font-size: 13px;
    display: flex;
    align-items: center;
    gap: 10px;
    border-left: 3px solid var(--warn);
}
.issue-card.clean { border-left-color: var(--pass); }
.issue-count {
    font-weight: 700;
    font-size: 16px;
    min-width: 32px;
    text-align: center;
}
.issue-count.warn { color: var(--warn); }
.issue-count.ok   { color: var(--pass); }

/* ---- BREAKING CHANGES TABLE ---- */
.breaking-table { width: 100%; border-collapse: collapse; font-size: 13px; }
.breaking-table th {
    text-align: left;
    padding: 8px 12px;
    font-size: 11px;
    color: var(--muted);
    text-transform: uppercase;
    letter-spacing: 0.5px;
    border-bottom: 1px solid var(--line);
}
.breaking-table td {
    padding: 8px 12px;
    border-bottom: 1px solid var(--line);
    font-family: var(--mono);
    font-size: 12px;
}
.breaking-table tr:hover { background: rgba(var(--veil),0.05); transition: background 0.2s ease; }

/* ---- COVERAGE LIST ---- */
.coverage-summary {
    display: flex;
    align-items: center;
    gap: 16px;
    margin-bottom: 16px;
    flex-wrap: wrap;
}
.coverage-pct {
    font-size: 32px;
    font-weight: 700;
    font-variant-numeric: tabular-nums;
}
.coverage-detail {
    font-size: 13px;
    color: var(--muted);
}
.coverage-file-list {
    font-size: 13px;
    max-height: 300px;
    overflow-y: auto;
}
.coverage-file {
    display: flex;
    gap: 8px;
    padding: 4px 12px;
    font-family: var(--mono);
    font-size: 12px;
    border-bottom: 1px solid var(--surface-2);
}
.coverage-file:hover { background: rgba(var(--veil),0.05); transition: background 0.2s ease; }

/* ---- FINDINGS ---- */
.finding-row {
    display: grid;
    grid-template-columns: 80px 1fr;
    gap: 12px;
    padding: 8px 12px;
    border-bottom: 1px solid var(--line);
    font-size: 13px;
}
.finding-row:hover { background: rgba(var(--veil),0.05); transition: background 0.2s ease; }

/* ---- SIDEBAR NAV ---- */
.layout-wrap { display: flex; gap: 0; position: relative; }
.sidebar-nav {
    position: sticky;
    top: 16px;
    align-self: flex-start;
    width: 180px;
    min-width: 180px;
    padding: 16px 0;
    margin-right: 24px;
    flex-shrink: 0;
    background: var(--glass-bg);
    backdrop-filter: blur(var(--glass-blur));
    -webkit-backdrop-filter: blur(var(--glass-blur));
    border: 1px solid var(--glass-border);
    border-radius: var(--radius);
}
.sidebar-nav a {
    display: block;
    padding: 6px 14px;
    font-size: 12px;
    color: var(--muted);
    text-decoration: none;
    border-left: 2px solid transparent;
    transition: all 0.15s;
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
}
.sidebar-nav a:hover { color: var(--fg); border-left-color: var(--line-strong); }
.sidebar-nav a.active {
    color: var(--fg);
    border-left-color: var(--accent);
    background: rgba(var(--veil),0.06);
    font-weight: 600;
}
.main-content { flex: 1 1 0; min-width: 0; }

/* Deep links */
.section[id] { scroll-margin-top: 24px; }
.cards-row[id] { scroll-margin-top: 24px; }
.header[id] { scroll-margin-top: 24px; }
.file-row[id] { scroll-margin-top: 24px; }
.file-row[id]:target { background: var(--hover); outline: 2px solid var(--accent); outline-offset: -2px; }
.file-row.highlight-flash { animation: flash-highlight 1.5s ease-out; }
@keyframes flash-highlight { 0% { background: color-mix(in srgb, var(--accent) 30%, transparent); outline: 2px solid var(--accent); } 100% { background: transparent; outline-color: transparent; } }

/* ---- DIFF MODAL ---- */
.diff-modal-overlay {
    display: none;
    position: fixed;
    top: 0; left: 0; right: 0; bottom: 0;
    background: rgba(0,0,0,0.75);
    z-index: 1000;
    justify-content: center;
    align-items: center;
    padding: 24px;
}
.diff-modal-overlay.open { display: flex; }
.diff-modal {
    background: rgba(15,15,15,0.95);
    backdrop-filter: blur(24px);
    -webkit-backdrop-filter: blur(24px);
    border: 1px solid var(--glass-border);
    border-radius: var(--radius);
    box-shadow: 0 20px 60px rgba(0,0,0,0.8);
    width: 90vw;
    max-width: 1200px;
    max-height: 85vh;
    display: flex;
    flex-direction: column;
    overflow: hidden;
}
.diff-modal-header {
    display: flex;
    justify-content: space-between;
    align-items: center;
    padding: 12px 16px;
    border-bottom: 1px solid var(--line);
    gap: 12px;
    flex-wrap: wrap;
}
.diff-modal-title {
    font-family: var(--mono);
    font-size: 13px;
    color: var(--accent);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    flex: 1;
}
.diff-modal-actions {
    display: flex;
    gap: 8px;
    align-items: center;
}
.diff-modal-search {
    padding: 4px 8px;
    background: var(--bg);
    border: 1px solid var(--line);
    border-radius: var(--radius-sm);
    color: var(--fg);
    font-size: 12px;
    font-family: var(--mono);
    outline: none;
    width: 200px;
}
.diff-modal-search:focus { border-color: var(--accent); }
.diff-modal-close {
    background: var(--surface-2);
    border: 1px solid var(--line);
    border-radius: var(--radius-sm);
    color: var(--fg);
    cursor: pointer;
    padding: 4px 10px;
    font-size: 13px;
}
.diff-modal-close:hover { border-color: var(--accent); }
.diff-modal-body {
    overflow: auto;
    flex: 1;
    padding: 0;
}
.diff-modal-body pre {
    font-family: var(--mono);
    font-size: 12px;
    line-height: 1.5;
    margin: 0;
    padding: 12px 16px;
    white-space: pre-wrap;
    overflow-wrap: break-word;
    word-break: break-word;
}
.diff-line-add { background: rgba(46,160,67,0.15); color: var(--pass); }
.diff-line-del { background: rgba(248,81,73,0.15); color: var(--block); }
.diff-line-hunk { color: var(--accent); font-weight: 600; }
.diff-line-match { background: rgba(210,153,34,0.25); }

/* ---- BATCH LINKS ---- */
.batch-links {
    display: flex;
    flex-wrap: wrap;
    gap: 8px;
    padding: 12px 16px;
    border-top: 1px solid var(--line);
    align-items: center;
    font-size: 12px;
    color: var(--muted);
}
.batch-links a {
    display: inline-flex;
    padding: 3px 8px;
    background: var(--surface-2);
    border: 1px solid var(--line);
    border-radius: var(--radius-sm);
    font-family: var(--mono);
    font-size: 11px;
    color: var(--accent);
    text-decoration: none;
    transition: border-color 0.15s;
}
.batch-links a:hover { border-color: var(--accent); }

/* ---- NARRATIVE ---- */
.narrative-content {
    font-family: var(--mono);
    font-size: 13px;
    color: var(--muted);
    white-space: pre-wrap;
    overflow-wrap: break-word;
    word-break: break-word;
    line-height: 1.6;
    margin: 0;
    max-height: 600px;
    overflow-y: auto;
    background: var(--bg);
    padding: 12px 16px;
    border-radius: var(--radius-sm);
}
.narrative-rendered { padding: 16px 20px; line-height: 1.6; }
.narrative-rendered > :first-child { margin-top: 0; }
.narrative-rendered h2 { font-size: 16px; margin: 16px 0 8px; color: var(--fg); }
.narrative-rendered h3 { font-size: 14px; margin: 12px 0 6px; color: var(--fg); }
.narrative-rendered h4 { font-size: 13px; margin: 10px 0 4px; color: var(--fg); }
.narrative-rendered p { margin: 0 0 12px; color: var(--muted); }
.narrative-rendered ul { margin: 0 0 12px; padding-left: 20px; color: var(--muted); }
.narrative-rendered li { margin-bottom: 4px; }
.narrative-rendered code { background: var(--surface-2); padding: 2px 6px; border-radius: 4px; font-size: 13px; }
.narrative-rendered pre { background: var(--surface-2); padding: 12px; border-radius: var(--radius); overflow-x: auto; margin: 0 0 12px; }
.narrative-rendered pre code { background: none; padding: 0; }
.narrative-rendered strong { color: var(--fg); }
.narrative-rendered a { color: var(--accent); text-decoration: none; }
.narrative-rendered a:hover { text-decoration: underline; }
.narrative-rendered .md-table { width: 100%; border-collapse: collapse; margin: 0 0 12px; font-size: 13px; }
.narrative-rendered .md-table th, .narrative-rendered .md-table td { padding: 6px 10px; border: 1px solid var(--line); text-align: left; }
.narrative-rendered .md-table th { background: var(--surface-2); font-weight: 600; color: var(--fg); }
.narrative-rendered .md-table td { color: var(--muted); }
.btn-ghost {
    background: none;
    border: 1px solid var(--line);
    border-radius: var(--radius-sm);
    color: var(--muted);
    cursor: pointer;
    padding: 4px 10px;
    font-size: 12px;
    transition: all 0.15s;
}
.btn-ghost:hover { border-color: var(--accent); color: var(--fg); }

/* ---- MERGE DECISION CARD ---- */
.merge-decision-card {
    grid-column: 1 / -1;
}
.merge-decision-badges {
    display: flex;
    gap: 10px;
    flex-wrap: wrap;
    margin-bottom: 8px;
}
.merge-decision-badge {
    display: inline-flex;
    align-items: center;
    gap: 6px;
    padding: 6px 14px;
    border-radius: 20px;
    font-size: 12px;
    font-weight: 700;
    letter-spacing: 0.3px;
    text-transform: uppercase;
}
.merge-decision-badge.mdb-pass { background: rgba(var(--veil),0.06); color: var(--fg); border: 1px solid rgba(var(--veil),0.14); }
.merge-decision-badge.mdb-pass::before { content: "\2713"; color: var(--pass); margin-right: 5px; }
.merge-decision-badge.mdb-fail { background: transparent; color: var(--block); border: 1px solid var(--block); }
.merge-decision-badge.mdb-hold { background: transparent; color: var(--warn); border: 1px solid var(--warn); }
.merge-decision-reason {
    font-size: 13px;
    color: var(--muted);
    line-height: 1.5;
}

/* ---- BLOCKERS SECTION ---- */
.blockers-grid {
    display: grid;
    grid-template-columns: repeat(auto-fit, minmax(300px, 1fr));
    gap: 16px;
}
.blocker-card {
    background: var(--glass-bg);
    backdrop-filter: blur(var(--glass-blur));
    -webkit-backdrop-filter: blur(var(--glass-blur));
    border: 1px solid var(--glass-border);
    border-radius: var(--radius);
    padding: 16px 20px;
    box-shadow: var(--glass-shadow), var(--glass-inset);
    border-left: 4px solid var(--block);
}
.blocker-card.blocker-success {
    border-left-color: var(--pass);
}
.blocker-card-header {
    display: flex;
    align-items: center;
    gap: 8px;
    margin-bottom: 8px;
    font-size: 14px;
    font-weight: 600;
}
.blocker-card-body {
    font-size: 13px;
    color: var(--muted);
    line-height: 1.5;
    margin-bottom: 8px;
}
.blocker-card-body code {
    font-family: var(--mono);
    background: var(--surface-2);
    padding: 1px 5px;
    border-radius: 3px;
    font-size: 12px;
}
.blocker-card-footer {
    display: flex;
    gap: 12px;
    align-items: center;
    flex-wrap: wrap;
    font-size: 12px;
}
.blocker-card-footer a {
    color: var(--accent);
    text-decoration: none;
}
.blocker-card-footer a:hover {
    text-decoration: underline;
}
.blocker-debug-hint {
    font-family: var(--mono);
    font-size: 11px;
    background: var(--surface-2);
    padding: 2px 8px;
    border-radius: var(--radius-sm);
    color: var(--faint);
}

/* ---- TIME BUDGET ---- */
.time-budget-chart {
    display: flex;
    flex-direction: column;
    gap: 6px;
}
.time-budget-row {
    display: grid;
    grid-template-columns: 180px 1fr 80px;
    align-items: center;
    gap: 12px;
    font-size: 13px;
}
.time-budget-name {
    font-family: var(--mono);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    display: flex;
    align-items: center;
    gap: 6px;
}
.time-budget-bar-wrap {
    height: 18px;
    background: var(--bg);
    border-radius: 4px;
    overflow: hidden;
}
.time-budget-bar {
    height: 100%;
    border-radius: 4px;
    transition: width 0.4s ease;
    background: var(--accent-dim);
}
.time-budget-bar.tb-slowest {
    background: rgba(var(--veil),0.30);
}
.time-budget-bar.tb-cached {
    background: var(--surface-2);
}
.time-budget-duration {
    text-align: right;
    color: var(--muted);
    font-family: var(--mono);
    font-size: 12px;
    display: flex;
    align-items: center;
    justify-content: flex-end;
    gap: 6px;
}
.time-budget-total {
    margin-top: 10px;
    padding-top: 10px;
    border-top: 1px solid var(--line);
    font-size: 13px;
    font-weight: 600;
    color: var(--muted);
    text-align: right;
}

/* ---- FOOTER ---- */
.footer {
    margin-top: 40px;
    padding: 16px 0;
    border-top: 1px solid var(--line);
    text-align: center;
    color: var(--faint);
    font-size: 12px;
}
.footer strong { color: var(--muted); }

/* ---- RESPONSIVE ---- */
@media (max-width: 1100px) {
    .sidebar-nav { display: none; }
    .layout-wrap { display: block; }
    .tier-one-grid { grid-template-columns: 1fr; }
}
@media (max-width: 768px) {
    body { padding: 12px; }
    .header { flex-direction: column; }
    .header-right { align-items: stretch; }
    .header-actions, .merge-decision { justify-content: flex-start; }
    .dashboard-context-repo { font-size: 24px; }
    .cards-row { grid-template-columns: repeat(2, 1fr); }
    .files-summary-breakdown { grid-template-columns: 1fr; }
    .signal-cards { grid-template-columns: 1fr; }
    .file-row {
        grid-template-columns: 28px 1fr 50px 50px;
        padding-left: 24px;
    }
    .file-row .diff-bar { display: none; }
    .dist-row { grid-template-columns: 120px 1fr 50px; }
    .loctree-grid { grid-template-columns: 1fr; }
    .commit-item { grid-template-columns: auto 1fr; }
    .commit-hash { display: none; }
    .files-toolbar { flex-direction: column; align-items: stretch; }
    .section-summary { max-width: 100%; text-align: left; }
}

@media (prefers-reduced-motion: reduce) {
    *, *::before, *::after {
        transition-duration: 0.01ms !important;
        animation-duration: 0.01ms !important;
    }
}

/* scrollbar styling */
::-webkit-scrollbar { width: 8px; height: 8px; }
::-webkit-scrollbar-track { background: transparent; }
::-webkit-scrollbar-thumb { background: rgba(var(--veil),0.1); border-radius: 4px; }
::-webkit-scrollbar-thumb:hover { background: rgba(var(--veil),0.2); }

/* ---- VIEW MODE (PRV-105) ---- */
body.author-mode .section-system { display: none; }
body.author-mode .section-noise  { display: none; }
.header-action-btn {
    background: var(--surface-2);
    border: 1px solid var(--line);
    border-radius: var(--radius-sm);
    color: var(--muted);
    cursor: pointer;
    padding: 4px 10px;
    font-size: 12px;
    font-weight: 600;
    transition: all 0.15s;
    flex-shrink: 0;
}
.header-action-btn:hover { border-color: var(--accent); color: var(--fg); }
.view-mode-toggle {
    display: inline-flex;
    align-items: center;
    gap: 4px;
    padding: 4px;
    background: var(--surface-2);
    border: 1px solid var(--line);
    border-radius: 999px;
    flex-wrap: wrap;
}
.lang-toggle {
    display: inline-flex;
    align-items: center;
    gap: 4px;
    padding: 4px;
    background: var(--surface-2);
    border: 1px solid var(--line);
    border-radius: 999px;
}
.view-mode-btn {
    background: transparent;
    border: 1px solid transparent;
    color: var(--muted);
    cursor: pointer;
    padding: 4px 10px;
    font-size: 12px;
    font-weight: 600;
    border-radius: 999px;
    transition: all 0.15s;
}
.view-mode-btn:hover { color: var(--fg); }
.view-mode-btn.active {
    background: rgba(var(--veil),0.12);
    border-color: rgba(var(--veil),0.18);
    color: var(--fg);
}
.lang-btn {
    background: transparent;
    border: 1px solid transparent;
    color: var(--muted);
    cursor: pointer;
    padding: 4px 8px;
    font-size: 11px;
    font-weight: 700;
    letter-spacing: 0.04em;
    border-radius: 999px;
    transition: all 0.15s;
}
.lang-btn:hover { color: var(--fg); }
.lang-btn.active {
    background: rgba(var(--veil),0.12);
    border-color: rgba(var(--veil),0.18);
    color: var(--fg);
}

/* ---- DELTA SECTION (PRV-104) ---- */
.delta-row {
    display: flex;
    flex-wrap: wrap;
    gap: 12px;
    margin-bottom: 28px;
    padding: 14px 20px;
    background: var(--glass-bg);
    backdrop-filter: blur(var(--glass-blur));
    -webkit-backdrop-filter: blur(var(--glass-blur));
    border: 1px solid var(--glass-border);
    border-radius: var(--radius);
    box-shadow: var(--glass-shadow), var(--glass-inset);
    align-items: center;
}
.delta-badge {
    display: inline-flex;
    align-items: center;
    gap: 6px;
    padding: 4px 12px;
    border-radius: 14px;
    font-size: 12px;
    font-weight: 600;
    background: var(--surface-2);
    color: var(--muted);
}
.delta-badge.delta-better { background: transparent; color: var(--pass); border: 1px solid var(--pass); }
.delta-badge.delta-worse  { background: transparent; color: var(--block); border: 1px solid var(--block); }
.delta-badge.delta-same   { background: var(--surface-2); color: var(--faint); }
.delta-label {
    font-size: 11px;
    color: var(--faint);
    text-transform: uppercase;
    letter-spacing: 0.3px;
    margin-right: 4px;
}

/* ---- LINT METRICS SECTION (PRV-205) ---- */
.lint-grid {
    display: grid;
    grid-template-columns: repeat(auto-fit, minmax(300px, 1fr));
    gap: 12px;
}
.lint-card {
    background: var(--glass-bg);
    backdrop-filter: blur(var(--glass-blur));
    -webkit-backdrop-filter: blur(var(--glass-blur));
    border: 1px solid var(--glass-border);
    border-radius: var(--radius);
    padding: 14px 18px;
    box-shadow: var(--glass-shadow), var(--glass-inset);
    border-left: 4px solid var(--line);
}
.lint-card.lint-clean { border-left-color: var(--pass); }
.lint-card.lint-new   { border-left-color: var(--warn); }
.lint-card.lint-mixed  { border-left-color: var(--block); }
.lint-card-header {
    display: flex;
    align-items: center;
    gap: 8px;
    margin-bottom: 8px;
    font-size: 13px;
    font-weight: 600;
}
.lint-card-stats {
    display: flex;
    gap: 16px;
    font-size: 12px;
    font-family: var(--mono);
    color: var(--muted);
    margin-bottom: 6px;
}
.lint-stat-new { color: var(--warn); font-weight: 600; }
.lint-stat-legacy { color: var(--faint); }
.lint-card-files {
    margin-top: 8px;
    padding-top: 8px;
    border-top: 1px solid var(--line);
}
.lint-card-files summary {
    font-size: 11px;
    color: var(--muted);
    cursor: pointer;
    user-select: none;
}
.lint-card-files summary:hover { color: var(--fg); }
.lint-card-files ul {
    list-style: none;
    margin-top: 4px;
    padding-left: 0;
}
.lint-card-files li {
    font-size: 11px;
    font-family: var(--mono);
    color: var(--muted);
    padding: 2px 0;
}
.lint-card-files li::before {
    content: "\2022 ";
    color: var(--faint);
}
.lint-clean-msg {
    background: var(--glass-bg);
    border: 1px solid var(--glass-border);
    border-radius: var(--radius);
    padding: 16px 20px;
    color: var(--pass);
    font-size: 13px;
    border-left: 4px solid var(--pass);
}
.lint-nodata-msg {
    background: var(--glass-bg);
    border: 1px solid var(--glass-border);
    border-radius: var(--radius);
    padding: 16px 20px;
    color: var(--faint);
    font-size: 13px;
    font-style: italic;
}

/* ---- TRENDS SECTION (PRV-201) ---- */
.trends-table-wrap { overflow-x: auto; }
.trends-table {
    width: 100%;
    border-collapse: collapse;
    font-size: 13px;
    font-family: var(--mono);
}
.trends-table th {
    text-align: left;
    padding: 8px 12px;
    border-bottom: 2px solid var(--line);
    color: var(--muted);
    font-weight: 600;
    font-size: 11px;
    text-transform: uppercase;
    letter-spacing: 0.5px;
}
.trends-table td {
    padding: 6px 12px;
    border-bottom: 1px solid var(--line);
    vertical-align: middle;
    white-space: nowrap;
}
.trends-row-latest td {
    background: rgba(var(--veil),0.04);
    font-weight: 600;
}
.trends-ts {
    color: var(--muted);
    min-width: 110px;
}
.trends-latest-badge {
    display: inline-block;
    font-size: 9px;
    text-transform: uppercase;
    letter-spacing: 0.5px;
    background: var(--accent-dim);
    color: var(--accent);
    padding: 1px 6px;
    border-radius: 3px;
    margin-left: 6px;
    font-weight: 700;
}
.trends-bar {
    display: inline-block;
    height: 12px;
    border-radius: 2px;
    vertical-align: middle;
    min-width: 2px;
    transition: width 0.3s ease;
}
.trends-bar-pass { background: var(--pass-dim); }
.trends-bar-fail { background: var(--block-dim); }
.trends-bar-warn { background: var(--warn-dim); }
.trends-val {
    font-size: 12px;
    color: var(--muted);
    margin-left: 4px;
}
.trends-pass { color: var(--pass); font-weight: 700; }
.trends-fail { color: var(--block); font-weight: 700; }
.trends-indicator {
    font-size: 12px;
    font-weight: 600;
    padding: 4px 10px;
    border-radius: var(--radius-sm);
}
.trends-indicator.trend-improving { background: transparent; color: var(--pass); border: 1px solid var(--pass); }
.trends-indicator.trend-degrading { background: transparent; color: var(--block); border: 1px solid var(--block); }
.trends-indicator.trend-mixed     { background: transparent; color: var(--warn); border: 1px solid var(--warn); }
.trends-indicator.trend-stable    { background: var(--surface-2); color: var(--faint); }

/* PRV-204: Flaky checks */
.flaky-section .flaky-stable-card {
    background: var(--glass-bg);
    border: 1px solid var(--glass-border);
    border-radius: var(--radius);
    padding: 16px 20px;
    color: var(--pass);
    font-weight: 600;
    font-size: 14px;
}
.flaky-table-wrap { overflow-x: auto; }
.flaky-table {
    width: 100%;
    border-collapse: collapse;
    font-size: 13px;
    font-family: var(--mono);
}
.flaky-table th {
    text-align: left;
    padding: 8px 12px;
    border-bottom: 2px solid var(--line);
    color: var(--muted);
    font-weight: 600;
    font-size: 11px;
    text-transform: uppercase;
    letter-spacing: 0.5px;
}
.flaky-table td {
    padding: 6px 12px;
    border-bottom: 1px solid var(--line);
    vertical-align: middle;
    white-space: nowrap;
}
.flaky-score {
    font-weight: 700;
    font-size: 13px;
}
.flaky-score-high { color: var(--block); }
.flaky-score-mid  { color: var(--warn); }
.flaky-score-low  { color: var(--muted); }
.flaky-sparkline {
    display: inline-flex;
    gap: 3px;
    align-items: center;
}
.flaky-dot {
    display: inline-block;
    width: 8px;
    height: 8px;
    border-radius: 50%;
}
.flaky-dot-pass { background: var(--pass); }
.flaky-dot-fail { background: var(--block); }
.flaky-dot-warn { background: var(--warn); }
.flaky-dot-skip { background: var(--faint); }
.flaky-dot-error { background: var(--block); opacity: 0.7; }
.flaky-confidence {
    display: inline-block;
    font-size: 10px;
    text-transform: uppercase;
    letter-spacing: 0.5px;
    padding: 2px 6px;
    border-radius: 3px;
    font-weight: 600;
}
.flaky-confidence-high { background: transparent; color: var(--pass); border: 1px solid var(--pass); }
.flaky-confidence-medium { background: transparent; color: var(--warn); border: 1px solid var(--warn); }
.flaky-confidence-low { background: var(--surface-2); color: var(--faint); }

/* === Dashboard v2: Collapsible sections === */
.section-collapsible { margin-bottom: 12px; }
.section-collapsible > .section-header {
    cursor: pointer;
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 12px;
    flex-wrap: wrap;
    min-width: 0;
    padding: 12px 16px;
    border-radius: var(--radius);
    background: rgba(var(--veil),0.03);
    border: 1px solid var(--line);
    transition: background 0.15s;
    user-select: none;
}
.section-collapsible > .section-header:hover { background: var(--hover); }
.section-collapsible > .section-header .toggle-indicator {
    color: var(--faint);
    font-size: 11px;
    transition: transform 0.15s;
    margin-right: 8px;
    display: inline-block;
}
.section-collapsible.expanded > .section-header .toggle-indicator { transform: rotate(90deg); color: var(--signal); }
.section-collapsible .section-collapsible-title {
    display: inline-flex;
    align-items: center;
    min-width: 0;
}
.section-collapsible .section-body {
    display: none;
    padding: 12px 0 0;
    min-width: 0;
}
.section-collapsible.expanded .section-body { display: block; }
.section-collapsible .section-body > .section {
    margin-bottom: 0;
    min-width: 0;
}
.section-collapsible .section-body > .section > .section-header {
    justify-content: flex-end;
    margin-bottom: 10px;
    padding: 0;
    border-bottom: 0;
    gap: 10px;
}
.section-collapsible .section-body > .section > .section-header .section-title {
    display: none;
}
.section-collapsible .section-body > .section > .tab-container,
.section-collapsible .section-body > .section > .card,
.section-collapsible .section-body > .section > .merge-decision-card {
    min-width: 0;
}
.section-summary {
    color: var(--muted);
    font-size: 12px;
    font-family: 'JetBrains Mono', monospace;
    flex: 1 1 280px;
    min-width: 0;
    max-width: none;
    text-align: right;
    white-space: normal;
    overflow-wrap: anywhere;
    word-break: break-word;
}

/* === Dashboard v2: Severity badges === */
.severity-badge {
    display: inline-block;
    padding: 4px 14px;
    border-radius: 12px;
    font-weight: 600;
    font-size: 13px;
    font-family: 'JetBrains Mono', monospace;
    letter-spacing: 0.5px;
}
.severity-ok   { background: rgba(var(--veil),0.06); color: var(--muted); border: 1px solid rgba(var(--veil),0.14); }
.severity-low  { background: rgba(var(--veil),0.06); color: var(--muted); border: 1px solid rgba(var(--veil),0.14); }
.severity-med  { background: transparent; color: var(--warn); border: 1px solid var(--warn); }
.severity-high { background: transparent; color: var(--serious); border: 1px solid var(--serious); }
.severity-crit { background: transparent; color: var(--block); border: 1px solid var(--block); }

/* === Dashboard v2: Action row (traffic lights) === */
.action-row {
    display: grid;
    gap: 10px;
    padding: 10px 0 14px;
}
.signal-cards {
    display: grid;
    grid-template-columns: repeat(auto-fit, minmax(220px, 1fr));
    gap: 10px;
}
.signal-card-title {
    font-size: 11px;
    text-transform: uppercase;
    letter-spacing: 0.08em;
    color: var(--muted);
}
.signal-card-value {
    font-size: 18px;
    font-weight: 700;
    margin-top: 4px;
}
.signal-card-meta {
    font-size: 12px;
    color: var(--muted);
    margin-top: 6px;
}
.signal-chips {
    display: flex;
    flex-wrap: wrap;
    gap: 8px;
}
.action-chip {
    display: inline-flex;
    align-items: center;
    gap: 6px;
    padding: 6px 12px;
    border-radius: 20px;
    font-size: 12px;
    font-weight: 500;
    cursor: pointer;
    transition: opacity 0.15s;
    border: 1px solid var(--line);
    text-decoration: none;
}
.action-chip:hover { opacity: 0.8; }
.action-chip.chip-ok   { background: transparent; color: var(--muted); border-color: rgba(var(--veil),0.12); }
.action-chip.chip-ok .chip-ok-check { color: var(--pass); }
.action-chip.chip-warn { background: transparent; color: var(--warn); border-color: var(--warn); }
.action-chip.chip-error{ background: transparent; color: var(--block); border-color: var(--block); }

/* === Dashboard v2: Sticky sidebar + groups === */
.sidebar-nav {
    position: sticky;
    top: 20px;
    max-height: calc(100vh - 40px);
    overflow-y: auto;
}
.sidebar-context {
    margin-bottom: 14px;
    padding: 0 0 14px;
    border-bottom: 1px solid var(--line);
}
.sidebar-context-repo {
    font-size: 12px;
    font-weight: 700;
    color: var(--fg);
    overflow-wrap: anywhere;
}
.sidebar-context-branch {
    margin-top: 4px;
    font-size: 11px;
    color: var(--muted);
    font-family: var(--mono);
    overflow-wrap: anywhere;
}
.sidebar-nav .nav-group-label {
    font-size: 10px;
    font-weight: 600;
    letter-spacing: 1.5px;
    color: var(--faint);
    text-transform: uppercase;
    padding: 12px 16px 4px;
    margin-top: 8px;
}
.sidebar-nav .nav-group-label:first-child { margin-top: 0; }
.sidebar-nav .nav-badge {
    display: inline-block;
    min-width: 16px;
    height: 16px;
    padding: 0 5px;
    border-radius: 8px;
    font-size: 10px;
    font-weight: 600;
    line-height: 16px;
    text-align: center;
    margin-left: 6px;
}
.sidebar-nav .nav-badge.badge-error { background: transparent; color: var(--block); border: 1px solid var(--block); }
.sidebar-nav .nav-badge.badge-warn  { background: transparent; color: var(--warn); border: 1px solid var(--warn); }

/* === Dashboard v2: Regression widget === */
.regression-widget {
    padding: 16px 20px;
    border-radius: var(--radius);
    background: rgba(var(--veil),0.03);
    border: 1px solid var(--line);
}
.regression-widget-header {
    display: flex;
    justify-content: space-between;
    align-items: center;
    margin-bottom: 12px;
}
.regression-reasons {
    list-style: none;
    padding: 0;
    margin: 0 0 12px;
    font-size: 13px;
    font-family: 'JetBrains Mono', monospace;
    color: var(--muted);
}
.regression-reasons li { padding: 2px 0; }
.regression-reasons li::before { content: "- "; color: var(--faint); }
.risk-files { margin-top: 8px; }
.risk-table { width: 100%; font-size: 13px; }
.risk-table td { padding: 4px 8px; }
.risk-table td:first-child { font-family: 'JetBrains Mono', monospace; }
.risk-table a { color: var(--fg); text-decoration: none; }
.risk-table a:hover { text-decoration: underline; }

.regression-mini-note {
    margin-top: 10px;
    font-size: 12px;
    color: var(--muted);
}

/* === Dashboard v2: Tab container === */
.tab-container { margin-top: 8px; }
.tab-buttons {
    display: flex;
    gap: 0;
    border-bottom: 1px solid var(--line);
    margin-bottom: 0;
    flex-wrap: wrap;
}
.tab-btn {
    padding: 8px 16px;
    background: none;
    border: none;
    color: var(--muted);
    cursor: pointer;
    font-size: 13px;
    font-family: 'JetBrains Mono', monospace;
    border-bottom: 2px solid transparent;
    transition: color 0.15s;
}
.tab-btn:hover { color: var(--fg); }
.tab-btn.active { color: var(--fg); border-bottom-color: var(--accent); }
.tab-panel { display: none; padding: 16px 0; }
.tab-panel.active { display: block; }

/* === Dashboard v2: File toggle === */
.file-toggle {
    display: flex;
    gap: 4px;
    margin-bottom: 12px;
}
.file-toggle .toggle-btn {
    padding: 4px 12px;
    border-radius: 14px;
    border: 1px solid var(--line);
    background: none;
    color: var(--muted);
    font-size: 12px;
    cursor: pointer;
    transition: all 0.15s;
}
.file-toggle .toggle-btn:hover { background: var(--hover); }
.file-toggle .toggle-btn.active { background: var(--surface-2); color: var(--fg); border-color: var(--line-strong); }

/* === Dashboard v2: Tier dividers === */
.tier-divider {
    border: none;
    border-top: 1px solid var(--line);
    margin: 16px 0;
}

/* === Dashboard v2: Hero stats row === */
.header-stats {
    font-size: 13px;
    font-family: 'JetBrains Mono', monospace;
    color: var(--muted);
    margin-top: 4px;
}

"#;

// ---------------------------------------------------------------------------
// JavaScript
// ---------------------------------------------------------------------------

pub(super) fn js() -> &'static str {
    r##"
document.addEventListener('DOMContentLoaded', function() {
    var I18N = {
        en: {
            'button.close': 'Close',
            'button.copyMarkdown': 'Copy as Markdown',
            'button.copyPath': 'Copy path',
            'button.copyPrComment': 'Copy PR Comment',
            'button.copyShort': 'Copy',
            'button.copied': 'Copied!',
            'button.expandAll': 'Expand all',
            'button.hideFiles': 'Hide files',
            'button.showFiles': 'Show files',
            'button.viewFullPatch': 'View full patch',
            'badge.mergeBlock': 'Merge: BLOCK',
            'badge.mergeGo': 'Merge: GO',
            'badge.mergeGoReview': 'Merge: GO WITH REVIEW',
            'badge.mergeHold': 'Merge: HOLD',
            'badge.policyAllow': 'Policy: ALLOW',
            'badge.policyBlock': 'Policy: BLOCK',
            'badge.qualityFail': 'Quality: FAIL',
            'badge.qualityPass': 'Quality: PASS',
            'chip.artifactPackOk': 'Artifact Pack OK',
            'chip.sanityOk': 'Sanity OK',
            'chip.breakingClear': 'Breaking: 0',
            'chip.checksOk': 'Checks OK ({passed}/{total})',
            'chip.coverageOk': 'Coverage: {pct}%',
            'chip.findingsClear': 'Findings: 0',
            'chip.heuristicsOk': 'Heuristics OK',
            'count.checks': '{count} checks',
            'count.changedFilesWithIssues': '{count} changed files with issues',
            'count.changes': '{count} changes',
            'count.detected': '{count} detected',
            'count.failing': '{count} failing',
            'count.files': '{count} files',
            'count.filesLoc': '{files} files / {loc} LOC',
            'count.findings': '{count} findings',
            'count.flaky': '{count} flaky',
            'count.inline': '{count} inline',
            'count.issues': '{count} issues',
            'count.locales': '{count} locales',
            'count.owners': '{count} owners',
            'count.runs': '{count} runs',
            'count.skipped': '{count} skipped',
            'count.signals': '{count} signals',
            'count.warnings': '{count} warnings',
            'footer.generatedBy': 'Generated by',
            'header.report': 'prview Report',
            'hero.mergeAllow': 'ALLOW MERGE',
            'hero.mergeAllowWithReview': 'ALLOW WITH REVIEW',
            'hero.mergeBlock': 'BLOCK MERGE',
            'hero.mergeHold': 'HOLD MERGE',
            'label.additions': 'Additions',
            'label.all': 'All',
            'label.allCommits': 'All commits',
            'label.after': 'After',
            'label.batch': 'Batch {count}',
            'label.base': 'Base',
            'label.before': 'Before',
            'label.branch': 'Branch',
            'label.checks': 'Checks',
            'label.commit': 'Commit',
            'label.code': 'code',
            'label.codeCaps': 'Code',
            'label.codeHealth': 'Code Health',
            'label.codeHotspots': 'Code hotspots',
            'label.cached': '(cached)',
            'label.churn': 'Churn',
            'label.coverageHeuristic': 'coverage heuristic',
            'label.cycles': 'Cycles?',
            'label.commits': 'Commits',
            'label.confidence': 'Confidence',
            'label.deadExports': 'Dead exports',
            'label.dashboardLanguage': 'Dashboard language',
            'label.dashboardViewMode': 'Dashboard view mode',
            'label.detected': 'detected',
            'label.deletions': 'Deletions',
            'label.duration': 'Duration',
            'label.error': 'Error',
            'label.failed': 'Failed',
            'label.factors': 'Factors',
            'label.file': 'File',
            'label.findings': 'Findings',
            'label.flags': 'Flags',
            'label.flakyScore': 'Flaky Score',
            'label.generated': 'Generated',
            'label.history': 'History',
            'label.hotspot': 'HOTSPOT',
            'label.hotspots': 'Hotspots',
            'label.kind': 'Kind',
            'label.keyCountsPerLocale': 'Key Counts per Locale',
            'label.key': 'Key',
            'label.languages': 'Languages',
            'label.artifactPack': 'Artifact Pack',
            'label.latest': 'latest',
            'label.latestCommit': 'Latest commit',
            'label.hideAssets': 'Hide assets',
            'label.hideConfig': 'Hide config',
            'label.hideI18n': 'Hide i18n',
            'label.hideRenames': 'Hide renames',
            'label.high': 'high',
            'label.message': 'Message',
            'label.low': 'low',
            'label.merge': 'Merge',
            'label.medium': 'medium',
            'label.missingKeys': 'Missing Keys',
            'label.missingInLocales': 'Missing in Locales',
            'label.name': 'Name',
            'label.nonCode': 'non-code',
            'label.noIssuesDetected': 'No issues detected',
            'label.path': 'Path',
            'label.passed': 'Passed',
            'label.perCommitPatches': 'Per-commit patches',
            'label.policy': 'Policy',
            'label.policyMode': 'Policy mode',
            'label.pullRequest': 'PR',
            'label.previousRun': 'vs Previous Run',
            'label.quality': 'Quality',
            'label.reviewSignals': 'Review signals',
            'label.run': 'Run',
            'label.runId': 'Run',
            'label.runs': 'Runs',
            'label.risk': 'Risk',
            'label.rule': 'Rule',
            'label.score': 'Score',
            'label.severity': 'Severity',
            'label.skipped': 'Skipped',
            'label.slowest': 'slowest',
            'label.scope': 'Scope',
            'label.clean': 'CLEAN',
            'label.legacyOnly': 'LEGACY ONLY',
            'label.newIssues': 'NEW ISSUES',
            'label.sortAlpha': 'Sort: A-Z',
            'label.sortChurn': 'Sort: Churn',
            'label.sortStatus': 'Sort: Status',
            'label.status': 'Status',
            'label.summary': 'Summary',
            'label.symbol': 'Symbol',
            'label.testHotspots': 'Test hotspots',
            'label.tests': 'tests',
            'label.testsCaps': 'Tests',
            'label.total': 'Total',
            'label.topDirectoriesByChurn': 'top directories by churn',
            'label.topRiskyFiles': 'Top risky files',
            'label.transitions': 'Transitions',
            'label.type': 'Type',
            'label.untested': 'Untested?',
            'label.variable': 'Variable',
            'label.warned': 'Warned',
            'label.warning': 'Warning',
            'label.yes': 'YES',
            'message.allChecksStableAcrossRuns': 'All checks stable across {count} runs',
            'message.noPerFilePatch': 'No per-file patch available.',
            'message.allLocalesMatching': 'All locales have matching keys.',
            'message.analysisFlaggedChangedLines': 'Lint / analysis flagged changed lines',
            'message.artifactPackNeedsLook': 'Artifact pack needs a quick look',
            'message.cleanLintAcrossChecks': 'Clean lint — no issues found across {count} checks',
            'message.codeHotspotsLead': 'are shown first to keep reviewer focus on risky implementation paths.',
            'message.coverageDetail': '{covered}/{total} changed source files have matching test changes',
            'message.coverageHeuristicNote': 'File-name matching heuristic — not actual code coverage',
            'message.cyclesLabel': 'Circular imports',
            'message.changedCodeWithoutMatchingTests': 'Changed code without matching tests',
            'message.exactTwinsLabel': 'Exact twins',
            'message.excludedNonCode': ' · {count} non-code files excluded',
            'message.fileOwnershipHint': 'File ownership from CODEOWNERS or path-based module detection. Use this to identify who to ping for review.',
            'message.filesWithMatchingTests': 'Files with matching test changes ({count})',
            'message.filesWithoutTests': 'Files without test changes ({count})',
            'message.heuristicsImprovement': 'Heuristics improvement',
            'message.heuristicsRegression': 'Heuristics regression',
            'message.heuristicsUnchanged': 'Heuristics unchanged',
            'message.heuristicScanNote': 'Heuristic scan — may contain false positives. Verify manually.',
            'message.lintChangedFilesWithIssues': 'Files with issues: {count}',
            'message.lintLegacyPreExisting': '{count} legacy (pre-existing)',
            'message.lintMetricsUnavailable': 'Lint metrics unavailable — missing change data',
            'message.lintNewInChangedFiles': '{count} new in changed files',
            'message.noDetailsAvailable': 'No details available',
            'message.noRegressionSignals': 'No regression signals',
            'message.openChecksFirst': 'Open checks first',
            'message.publicApiImpactDetected': 'Public API / env impact detected',
            'message.qualityChecksFailed': '{count} quality checks failed',
            'message.qualityGatesNeedAttention': 'Quality gates passed, but {count} review signals need attention',
            'message.rerunLocallyVerbose': 'Re-run locally with verbose output',
            'message.reviewNoisyChecksBeforeMerge': 'Review noisy checks before merge',
            'message.reviewSignalsPrefix': 'Review signals',
            'message.seeFullRegressionSection': 'See the full Regression section for heuristics changes and deeper hotspot context.',
            'message.seeLogForDetails': 'See log for details',
            'message.structureChangedNotably': 'Structure changed in notable ways',
            'message.unusedSymbolsLabel': 'Unused symbols',
            'message.verifyCompatibilityBeforeMerge': 'Verify compatibility before merge',
            'message.viewDiff': 'View diff',
            'message.viewFullLog': 'View full log',
            'message.viewPrOnGitHub': 'View PR #{num} on GitHub ↗',
            'misc.loading': 'Loading...',
            'nav.artifacts': 'Artifacts',
            'nav.artifactsLink': 'Artifacts',
            'nav.breaking': 'Breaking',
            'nav.checks': 'Checks',
            'nav.code': 'Code',
            'nav.commits': 'Commits',
            'nav.coverage': 'Coverage',
            'nav.decision': 'Decision',
            'nav.files': 'Files',
            'nav.findings': 'Findings',
            'nav.flaky': 'Flaky',
            'nav.i18n': 'i18n',
            'nav.meta': 'Meta',
            'nav.narrative': 'Narrative',
            'nav.overview': 'Overview',
            'nav.ownership': 'Ownership',
            'nav.quality': 'Quality',
            'nav.regression': 'Regression',
            'nav.security': 'Security',
            'nav.statistics': 'Statistics',
            'nav.timeBudget': 'Time Budget',
            'nav.trends': 'Trends',
            'policy.mode.block': 'block',
            'policy.mode.shadow': 'monitor',
            'policy.mode.warn': 'warn',
            'placeholder.searchArtifacts': 'Search artifacts...',
            'placeholder.searchDiff': 'Search in diff...',
            'placeholder.searchFiles': 'Search files...',
            'section.artifactsExplorer': 'Artifacts Explorer',
            'section.breaking': 'Breaking',
            'section.checks': 'Checks',
            'section.codeStatistics': 'Code Statistics',
            'section.commits': 'Commits',
            'section.coverage': 'Coverage',
            'section.filesChanged': 'Files Changed',
            'section.findings': 'Findings',
            'section.flaky': 'Flaky',
            'section.flakyChecks': 'Flaky Checks',
            'section.heuristics': 'Heuristics',
            'section.historicalTrends': 'Historical Trends',
            'section.i18n': 'i18n',
            'section.i18nParity': 'i18n Parity Check',
            'section.inlineFindings': 'Inline Findings',
            'section.mergeDecision': 'Merge Decision',
            'section.narrative': 'Narrative',
            'section.narrativeReview': 'Narrative Review',
            'section.ownership': 'Ownership',
            'section.prOverview': 'PR Overview',
            'section.regression': 'Regression',
            'section.security': 'Security',
            'section.statistics': 'Statistics',
            'section.timeBudget': 'Time Budget',
            'section.trends': 'Trends',
            'section.assetsChanged': 'Assets Changed',
            'section.blockers': 'Blockers',
            'section.changeDistribution': 'Change Distribution',
            'section.changedSignatures': 'Changed Signatures',
            'section.lint': 'Lint',
            'section.newEnvRequirements': 'New Environment Requirements',
            'section.removedPublicSymbols': 'Removed Public Symbols',
            'section.topRiskFiles': 'Top Risk Files',
            'summary.checks': '{passed} passed, {failed} failed, {warn} warn',
            'summary.checksDetailed': '{passed} passed, {warn} warned, {failed} failed, {skipped} skipped',
            'summary.checkDurations': 'check durations',
            'summary.codeOwners': 'code owners',
            'summary.commitsCount': '{count} commits',
            'summary.coveragePct': '{pct}% coverage',
            'summary.filesBreakdown': '{total} files | {code} code | {tests} tests | {other} non-code',
            'summary.findingsCount': '{count} findings',
            'summary.headerStats': '{total} files | {code} code | {test} tests | {other} non-code | +{add} / -{del}',
            'summary.lintTotals': '{new} new / {legacy} legacy / {total} total',
            'summary.newCount': '{count} new',
            'summary.none': 'none',
            'summary.packContents': 'artifact pack contents',
            'summary.regressionBundle': 'score + heuristics + risky files',
            'summary.regressionScore': 'Score: {score}/100 ({severity})',
            'summary.reviewSignalCount': '{count} review signals',
            'summary.runHistory': 'run history',
            'summary.scoreValue': 'Score: {value}',
            'summary.securityChecks': 'security review',
            'summary.severityValue': 'Severity: {value}',
            'summary.stabilityAnalysis': 'stability analysis',
            'summary.statisticsBundle': 'loctree + lint + change distribution',
            'summary.translationDelta': 'translation changes',
            'status.blocking': 'BLOCKING',
            'status.nonBlocking': 'NON-BLOCKING',
            'trend.degrading': 'Degrading',
            'trend.improving': 'Improving',
            'trend.mixed': 'Mixed',
            'trend.stable': 'Stable',
            'view.author': 'Author View',
            'view.review': 'Review View'
        },
        pl: {
            'button.close': 'Zamknij',
            'button.copyMarkdown': 'Kopiuj jako Markdown',
            'button.copyPath': 'Kopiuj ścieżkę',
            'button.copyPrComment': 'Kopiuj komentarz PR',
            'button.copyShort': 'Kopiuj',
            'button.copied': 'Skopiowano!',
            'button.expandAll': 'Rozwiń wszystkie',
            'button.hideFiles': 'Ukryj pliki',
            'button.showFiles': 'Pokaż pliki',
            'button.viewFullPatch': 'Pokaż pełny patch',
            'badge.mergeBlock': 'Merge: blokuj',
            'badge.mergeGo': 'Merge: OK',
            'badge.mergeGoReview': 'Merge: po review',
            'badge.mergeHold': 'Merge: wstrzymaj',
            'badge.policyAllow': 'Polityka: pozwala',
            'badge.policyBlock': 'Polityka: blokuje',
            'badge.qualityFail': 'Jakość: do poprawy',
            'badge.qualityPass': 'Jakość: OK',
            'chip.artifactPackOk': 'Pakiet artefaktów OK',
            'chip.sanityOk': 'Sanity OK',
            'chip.breakingClear': 'Zmiany łamiące: 0',
            'chip.checksOk': 'Checki OK ({passed}/{total})',
            'chip.coverageOk': 'Pokrycie: {pct}%',
            'chip.findingsClear': 'Znaleziska: 0',
            'chip.heuristicsOk': 'Heurystyki OK',
            'count.checks': '{count} checków',
            'count.changedFilesWithIssues': '{count} zmienionych plików z problemami',
            'count.changes': 'Zmiany: {count}',
            'count.detected': '{count} wykryto',
            'count.failing': 'Nieudane: {count}',
            'count.files': '{count} plików',
            'count.filesLoc': '{files} plików / {loc} LOC',
            'count.findings': '{count} znalezisk',
            'count.flaky': '{count} niestabilnych',
            'count.inline': 'Inline: {count}',
            'count.issues': 'Problemy: {count}',
            'count.locales': '{count} locale',
            'count.owners': '{count} właścicieli',
            'count.runs': '{count} uruchomień',
            'count.skipped': '{count} pominiętych',
            'count.signals': 'Sygnały: {count}',
            'count.warnings': 'Ostrzeżenia: {count}',
            'footer.generatedBy': 'Wygenerowano przez',
            'header.report': 'Raport prview',
            'hero.mergeAllow': 'MERGE OK',
            'hero.mergeAllowWithReview': 'MERGE PO REVIEW',
            'hero.mergeBlock': 'BLOKUJ MERGE',
            'hero.mergeHold': 'WSTRZYMAJ MERGE',
            'label.additions': 'Dodania',
            'label.all': 'Wszystkie',
            'label.allCommits': 'Wszystkie commity',
            'label.after': 'Po',
            'label.batch': 'Batch {count}',
            'label.base': 'Baza',
            'label.before': 'Przed',
            'label.branch': 'Gałąź',
            'label.checks': 'Checki',
            'label.commit': 'Commit',
            'label.code': 'kod',
            'label.codeCaps': 'Kod',
            'label.codeHealth': 'Zdrowie kodu',
            'label.codeHotspots': 'Hotspoty kodu',
            'label.cached': '(cache)',
            'label.churn': 'Churn',
            'label.coverageHeuristic': 'heurystyka pokrycia',
            'label.cycles': 'Cykle?',
            'label.commits': 'Commity',
            'label.confidence': 'Pewność',
            'label.deadExports': 'Martwe eksporty',
            'label.dashboardLanguage': 'Język dashboardu',
            'label.dashboardViewMode': 'Tryb widoku dashboardu',
            'label.detected': 'wykryto',
            'label.deletions': 'Usunięcia',
            'label.duration': 'Czas',
            'label.error': 'Błąd',
            'label.failed': 'Nieudane',
            'label.factors': 'Czynniki',
            'label.file': 'Plik',
            'label.findings': 'Znaleziska',
            'label.flags': 'Flagi',
            'label.flakyScore': 'Wskaźnik flaky',
            'label.generated': 'Wygenerowano',
            'label.history': 'Historia',
            'label.hotspot': 'HOTSPOT',
            'label.hotspots': 'Hotspoty',
            'label.kind': 'Typ',
            'label.keyCountsPerLocale': 'Liczba kluczy w locale',
            'label.key': 'Klucz',
            'label.languages': 'Języki',
            'label.artifactPack': 'Pakiet artefaktów',
            'label.latest': 'najnowszy',
            'label.latestCommit': 'Ostatni commit',
            'label.hideAssets': 'Ukryj assety',
            'label.hideConfig': 'Ukryj config',
            'label.hideI18n': 'Ukryj i18n',
            'label.hideRenames': 'Ukryj rename’y',
            'label.high': 'wysoka',
            'label.message': 'Komunikat',
            'label.low': 'niska',
            'label.merge': 'Merge',
            'label.medium': 'średnia',
            'label.missingKeys': 'Brakujące klucze',
            'label.missingInLocales': 'Braki w locale',
            'label.name': 'Nazwa',
            'label.nonCode': 'nie-kod',
            'label.noIssuesDetected': 'Brak wykrytych problemów',
            'label.path': 'Ścieżka',
            'label.passed': 'Udane',
            'label.perCommitPatches': 'Patche per commit',
            'label.policy': 'Polityka',
            'label.policyMode': 'Tryb polityki',
            'label.pullRequest': 'PR',
            'label.previousRun': 'vs poprzednie uruchomienie',
            'label.quality': 'Jakość',
            'label.reviewSignals': 'Sygnały do sprawdzenia',
            'label.run': 'Uruchomienie',
            'label.runId': 'Run',
            'label.runs': 'Uruchomienia',
            'label.risk': 'Ryzyko',
            'label.rule': 'Reguła',
            'label.score': 'Wynik',
            'label.severity': 'Poziom',
            'label.skipped': 'Pominięte',
            'label.slowest': 'najwolniejszy',
            'label.scope': 'Zakres',
            'label.clean': 'CZYSTO',
            'label.legacyOnly': 'TYLKO ZALEGŁE',
            'label.newIssues': 'NOWE PROBLEMY',
            'label.sortAlpha': 'Sortuj: A-Z',
            'label.sortChurn': 'Sortuj: churn',
            'label.sortStatus': 'Sortuj: status',
            'label.status': 'Status',
            'label.summary': 'Podsumowanie',
            'label.symbol': 'Symbol',
            'label.testHotspots': 'Hotspoty testów',
            'label.tests': 'testy',
            'label.testsCaps': 'Testy',
            'label.total': 'Suma',
            'label.topDirectoriesByChurn': 'top katalogi wg churnu',
            'label.topRiskyFiles': 'Najbardziej ryzykowne pliki',
            'label.transitions': 'Przejścia',
            'label.type': 'Typ',
            'label.untested': 'Bez testów?',
            'label.variable': 'Zmienna',
            'label.warned': 'Ostrzeżenia',
            'label.warning': 'Ostrzeżenie',
            'label.yes': 'TAK',
            'message.allChecksStableAcrossRuns': 'Wszystkie checki są stabilne w {count} runach',
            'message.noPerFilePatch': 'Brak patcha dla pojedynczego pliku.',
            'message.allLocalesMatching': 'Wszystkie locale mają zgodny zestaw kluczy.',
            'message.analysisFlaggedChangedLines': 'Lint / analiza wskazały zmienione linie',
            'message.artifactPackNeedsLook': 'Pakiet artefaktów warto szybko przejrzeć',
            'message.cleanLintAcrossChecks': 'Lint czysty — brak problemów w {count} checkach',
            'message.codeHotspotsLead': 'są pokazane najpierw, żeby utrzymać fokus review na ryzykownych ścieżkach implementacji.',
            'message.coverageDetail': '{covered}/{total} zmienionych plików źródłowych ma pasujące zmiany w testach',
            'message.coverageHeuristicNote': 'Heurystyka po nazwach plików — to nie jest rzeczywiste pokrycie testami',
            'message.cyclesLabel': 'Importy cykliczne',
            'message.changedCodeWithoutMatchingTests': 'Kod zmienił się bez pasujących zmian w testach',
            'message.exactTwinsLabel': 'Dokładne duplikaty',
            'message.excludedNonCode': ' · wykluczono {count} plików nie-kodowych',
            'message.fileOwnershipHint': 'Własność plików z CODEOWNERS albo detekcji modułów po ścieżkach. Użyj tego, żeby wiedzieć, kogo pingnąć do review.',
            'message.filesWithMatchingTests': 'Pliki z pasującymi zmianami w testach ({count})',
            'message.filesWithoutTests': 'Pliki bez zmian w testach ({count})',
            'message.heuristicsImprovement': 'Poprawa heurystyk',
            'message.heuristicsRegression': 'Regresja heurystyk',
            'message.heuristicsUnchanged': 'Heurystyki bez zmian',
            'message.heuristicScanNote': 'Skan heurystyczny — może zawierać false positive. Zweryfikuj ręcznie.',
            'message.lintChangedFilesWithIssues': 'Pliki z problemami: {count}',
            'message.lintLegacyPreExisting': '{count} zaległych (sprzed tej zmiany)',
            'message.lintMetricsUnavailable': 'Metryki lint niedostępne — brak danych o zmianach',
            'message.lintNewInChangedFiles': '{count} nowych w zmienionych plikach',
            'message.noDetailsAvailable': 'Brak szczegółów',
            'message.noRegressionSignals': 'Brak sygnałów regresji',
            'message.openChecksFirst': 'Najpierw otwórz checki',
            'message.publicApiImpactDetected': 'Wykryto wpływ na publiczne API lub env',
            'message.qualityChecksFailed': 'Nie przeszły {count} checki jakości',
            'message.qualityGatesNeedAttention': 'Bramki jakości przeszły, ale {count} sygnały wymagają uwagi',
            'message.rerunLocallyVerbose': 'Uruchom lokalnie jeszcze raz z pełniejszym logiem',
            'message.reviewNoisyChecksBeforeMerge': 'Sprawdź checki z ostrzeżeniami przed mergem',
            'message.reviewSignalsPrefix': 'Sygnały do sprawdzenia',
            'message.seeFullRegressionSection': 'W pełnej sekcji Regresja zobaczysz zmiany heurystyk i szerszy kontekst hotspotów.',
            'message.seeLogForDetails': 'Szczegóły w logu',
            'message.structureChangedNotably': 'Struktura zmieniła się w zauważalny sposób',
            'message.unusedSymbolsLabel': 'Nieużywane symbole',
            'message.verifyCompatibilityBeforeMerge': 'Sprawdź kompatybilność przed mergem',
            'message.viewDiff': 'Pokaż diff',
            'message.viewFullLog': 'Pokaż pełny log',
            'message.viewPrOnGitHub': 'Otwórz PR #{num} na GitHubie ↗',
            'misc.loading': 'Ładowanie...',
            'nav.artifacts': 'Artefakty',
            'nav.artifactsLink': 'Artefakty',
            'nav.breaking': 'Zmiany łamiące',
            'nav.checks': 'Checki',
            'nav.code': 'Kod',
            'nav.commits': 'Commity',
            'nav.coverage': 'Pokrycie',
            'nav.decision': 'Decyzja',
            'nav.files': 'Pliki',
            'nav.findings': 'Znaleziska',
            'nav.flaky': 'Niestabilność',
            'nav.i18n': 'i18n',
            'nav.meta': 'Meta',
            'nav.narrative': 'Narracja',
            'nav.overview': 'Przegląd',
            'nav.ownership': 'Własność',
            'nav.quality': 'Jakość',
            'nav.regression': 'Regresja',
            'nav.security': 'Bezpieczeństwo',
            'nav.statistics': 'Statystyki',
            'nav.timeBudget': 'Budżet czasu',
            'nav.trends': 'Trendy',
            'policy.mode.block': 'blokuj',
            'policy.mode.shadow': 'monitoruj',
            'policy.mode.warn': 'ostrzegaj',
            'placeholder.searchArtifacts': 'Szukaj artefaktów...',
            'placeholder.searchDiff': 'Szukaj w diffie...',
            'placeholder.searchFiles': 'Szukaj plików...',
            'section.artifactsExplorer': 'Eksplorator artefaktów',
            'section.breaking': 'Zmiany łamiące',
            'section.checks': 'Checki',
            'section.codeStatistics': 'Statystyki kodu',
            'section.commits': 'Commity',
            'section.coverage': 'Pokrycie',
            'section.filesChanged': 'Zmienione pliki',
            'section.findings': 'Znaleziska',
            'section.flaky': 'Niestabilność',
            'section.flakyChecks': 'Niestabilne checki',
            'section.heuristics': 'Heurystyki',
            'section.historicalTrends': 'Trendy historyczne',
            'section.i18n': 'i18n',
            'section.i18nParity': 'Kontrola spójności i18n',
            'section.inlineFindings': 'Znaleziska inline',
            'section.mergeDecision': 'Rekomendacja merge',
            'section.narrative': 'Narracja',
            'section.narrativeReview': 'Opis zmian',
            'section.ownership': 'Własność',
            'section.prOverview': 'Przegląd PR',
            'section.regression': 'Regresja',
            'section.security': 'Bezpieczeństwo',
            'section.statistics': 'Statystyki',
            'section.timeBudget': 'Budżet czasu',
            'section.trends': 'Trendy',
            'section.assetsChanged': 'Zmienione zasoby',
            'section.blockers': 'Blokery',
            'section.changeDistribution': 'Rozkład zmian',
            'section.changedSignatures': 'Zmienione sygnatury',
            'section.lint': 'Lint',
            'section.newEnvRequirements': 'Nowe wymagania środowiskowe',
            'section.removedPublicSymbols': 'Usunięte publiczne symbole',
            'section.topRiskFiles': 'Najbardziej ryzykowne pliki',
            'summary.checks': 'udane: {passed} | nieudane: {failed} | ostrzeżenia: {warn}',
            'summary.checksDetailed': '{passed} udane, {warn} z ostrzeżeniami, {failed} nieudane, {skipped} pominięte',
            'summary.checkDurations': 'czas checków',
            'summary.codeOwners': 'właściciele kodu',
            'summary.commitsCount': '{count} commitów',
            'summary.coveragePct': '{pct}% pokrycia',
            'summary.filesBreakdown': '{total} plików | {code} kod | {tests} testy | {other} nie-kod',
            'summary.findingsCount': '{count} znalezisk',
            'summary.headerStats': '{total} plików | {code} kod | {test} testy | {other} nie-kod | +{add} / -{del}',
            'summary.lintTotals': '{new} nowych / {legacy} zaległych / {total} łącznie',
            'summary.newCount': '{count} nowych',
            'summary.none': 'brak',
            'summary.packContents': 'zawartość paczki artefaktów',
            'summary.regressionBundle': 'wynik + heurystyki + ryzykowne pliki',
            'summary.regressionScore': 'Wynik: {score}/100 ({severity})',
            'summary.reviewSignalCount': '{count} sygnałów do sprawdzenia',
            'summary.runHistory': 'historia uruchomień',
            'summary.scoreValue': 'Wynik: {value}',
            'summary.securityChecks': 'kontrole bezpieczeństwa',
            'summary.severityValue': 'Poziom: {value}',
            'summary.stabilityAnalysis': 'analiza stabilności',
            'summary.statisticsBundle': 'loctree + lint + rozkład zmian',
            'summary.translationDelta': 'zmiany w tłumaczeniach',
            'status.blocking': 'BLOKUJĄCY',
            'status.nonBlocking': 'NIEBLOKUJĄCY',
            'trend.degrading': 'Pogarsza się',
            'trend.improving': 'Poprawia się',
            'trend.mixed': 'Niejednoznacznie',
            'trend.stable': 'Stabilnie',
            'view.author': 'Widok autora',
            'view.review': 'Widok recenzenta'
        }
    };
    var currentLang = 'en';
    var langToggleEn = document.getElementById('lang-toggle-en');
    var langTogglePl = document.getElementById('lang-toggle-pl');

    function t(key) {
        return (I18N[currentLang] && I18N[currentLang][key]) || I18N.en[key] || key;
    }

    function polishPlural(count, one, few, many) {
        var abs = Math.abs(Number(count)) || 0;
        var mod10 = abs % 10;
        var mod100 = abs % 100;
        if (abs === 1) return one;
        if (mod10 >= 2 && mod10 <= 4 && !(mod100 >= 12 && mod100 <= 14)) return few;
        return many;
    }

    function translateReviewSignalPart(part) {
        if (currentLang !== 'pl') return part;
        var trimmed = (part || '').trim();
        var match;
        match = trimmed.match(/^(\d+)\s+breaking change(?:s)?$/);
        if (match) {
            var breakingCount = Number(match[1]);
            return match[1] + ' ' + polishPlural(breakingCount, 'zmiana łamiąca', 'zmiany łamiące', 'zmian łamiących');
        }
        match = trimmed.match(/^(\d+)%\s+coverage heuristic$/);
        if (match) return 'heurystyka pokrycia ' + match[1] + '%';
        match = trimmed.match(/^(\d+)\s+inline finding(?:s)?$/);
        if (match) {
            var inlineCount = Number(match[1]);
            return match[1] + ' ' + polishPlural(inlineCount, 'znalezisko inline', 'znaleziska inline', 'znalezisk inline');
        }
        // MERGE_GATE runtime caveats. Tool names (Semgrep, Clippy,
        // heuristics_loctree, ...) stay original; only the formulaic wrapper
        // is localized. Unknown shapes fall through to EN below.
        match = trimmed.match(/^(.+) returned warnings$/);
        if (match) return match[1] + ': ostrzeżenia';
        match = trimmed.match(/^(.+) skipped: (.+)$/);
        if (match) {
            var skipReasonMap = {
                'lint disabled': 'lint wyłączony',
                'tests disabled': 'testy wyłączone',
                'security disabled': 'security wyłączone'
            };
            var skipReason = skipReasonMap[match[2].trim()] || match[2];
            return match[1] + ' pominięty: ' + skipReason;
        }
        match = trimmed.match(/^(.+) needs manual review$/);
        if (match) return match[1] + ' wymaga ręcznego przeglądu';
        return trimmed;
    }

    function translatePolicyMode(mode) {
        var source = (mode || '').trim().toLowerCase();
        if (!source) return '';
        var key = 'policy.mode.' + source;
        var translated = t(key);
        return translated === key ? source : translated;
    }

    function translateDecisionReason(reason) {
        if (currentLang !== 'pl') return reason;
        var text = (reason || '').trim();
        var match;
        if (text === 'All quality gates passed') return 'Wszystkie bramki jakości przeszły';
        match = text.match(/^Quality gates passed, but (\d+) review signal(?:s)? need attention$/);
        if (match) {
            var reviewCount = Number(match[1]);
            return 'Bramki jakości przeszły, ale ' + match[1] + ' ' + polishPlural(reviewCount, 'sygnał wymaga uwagi', 'sygnały wymagają uwagi', 'sygnałów wymaga uwagi');
        }
        match = text.match(/^(\d+) quality check(?:s)? failed$/);
        if (match) {
            var checkCount = Number(match[1]);
            return 'Nie przeszły ' + match[1] + ' ' + polishPlural(checkCount, 'check jakości', 'checki jakości', 'checków jakości');
        }
        if (text === 'Merge not recommended') return 'Merge nie jest rekomendowany';
        match = text.match(/^(\d+) blocking issue(?:s)? found: (.+)$/);
        if (match) {
            var blockDetailCount = Number(match[1]);
            return 'znaleziono ' + match[1] + ' ' + polishPlural(blockDetailCount, 'blokujący problem', 'blokujące problemy', 'blokujących problemów') + ': ' + match[2];
        }
        match = text.match(/^(\d+) blocking issue(?:s)? found$/);
        if (match) {
            var issueCount = Number(match[1]);
            return 'Wykryto ' + match[1] + ' ' + polishPlural(issueCount, 'blokujący problem', 'blokujące problemy', 'blokujących problemów');
        }
        return text;
    }

    function translateSeverityValue(value) {
        if (currentLang !== 'pl') return value;
        var normalized = (value || '').trim().toUpperCase();
        var map = {
            'OK': 'OK',
            'LOW': 'NISKIE',
            'MED': 'ŚREDNIE',
            'HIGH': 'WYSOKIE',
            'CRITICAL': 'KRYTYCZNE'
        };
        return map[normalized] || value;
    }

    function translateRegressionReason(reason) {
        if (currentLang !== 'pl') return reason;
        var text = (reason || '').trim();
        var match;
        match = text.match(/^max code churn (\d+) \(\+(\d+)\)$/);
        if (match) return 'maks. churn kodu ' + match[1] + ' (+' + match[2] + ')';
        match = text.match(/^(\d+) code churn \(\+(\d+)\)$/);
        if (match) return 'churn kodu: ' + match[1] + ' (+' + match[2] + ')';
        match = text.match(/^(\d+) untested code files \(\+(\d+)\): (.+)$/);
        if (match) {
            var untestedCount = Number(match[1]);
            return match[1] + ' ' + polishPlural(untestedCount, 'plik kodu bez testów', 'pliki kodu bez testów', 'plików kodu bez testów') + ' (+' + match[2] + '): ' + match[3];
        }
        match = text.match(/^(\d+) query-in-loop files \(\+(\d+)\): (.+)$/);
        if (match) return match[1] + ' plików z query-in-loop (+' + match[2] + '): ' + match[3];
        match = text.match(/^(\d+) clone\/collect-in-loop files \(\+(\d+)\): (.+)$/);
        if (match) return match[1] + ' plików z clone/collect-in-loop (+' + match[2] + '): ' + match[3];
        match = text.match(/^(\d+) exact twins \(\+(\d+)\): (.+)$/);
        if (match) return match[1] + ' dokładne duplikaty (+' + match[2] + '): ' + match[3];
        match = text.match(/^(\d+) dead exports \(\+(\d+)\): (.+)$/);
        if (match) return match[1] + ' martwe eksporty (+' + match[2] + '): ' + match[3];
        match = text.match(/^(\d+) cycles \(\+(\d+)\): (.+)$/);
        if (match) return match[1] + ' cykle (+' + match[2] + '): ' + match[3];
        return text;
    }

    function applyTranslations() {
        document.documentElement.lang = currentLang;
        document.querySelectorAll('[data-i18n]').forEach(function(el) {
            var key = el.getAttribute('data-i18n');
            el.textContent = t(key);
        });
        document.querySelectorAll('[data-i18n-count]').forEach(function(el) {
            var key = el.getAttribute('data-i18n-count');
            var count = el.getAttribute('data-count') || '';
            el.textContent = t(key) + ' (' + count + ')';
        });
        document.querySelectorAll('[data-i18n-placeholder]').forEach(function(el) {
            var key = el.getAttribute('data-i18n-placeholder');
            el.setAttribute('placeholder', t(key));
        });
        document.querySelectorAll('[data-i18n-title]').forEach(function(el) {
            var key = el.getAttribute('data-i18n-title');
            el.setAttribute('title', t(key));
        });
        document.querySelectorAll('[data-i18n-aria-label]').forEach(function(el) {
            var key = el.getAttribute('data-i18n-aria-label');
            el.setAttribute('aria-label', t(key));
        });
        document.querySelectorAll('[data-i18n-template]').forEach(function(el) {
            var key = el.getAttribute('data-i18n-template');
            var rendered = t(key);
            Object.keys(el.dataset).forEach(function(name) {
                if (name === 'i18n' || name === 'i18nCount' || name === 'i18nPlaceholder' || name === 'i18nTemplate') {
                    return;
                }
                rendered = rendered.split('{' + name + '}').join(el.dataset[name]);
            });
            el.textContent = rendered;
        });
        document.querySelectorAll('[data-i18n-template="summary.regressionScore"]').forEach(function(el) {
            var rendered = t('summary.regressionScore');
            rendered = rendered.split('{score}').join(el.dataset.score || '');
            rendered = rendered.split('{severity}').join(translateSeverityValue(el.dataset.severity || ''));
            el.textContent = rendered;
        });
        document.querySelectorAll('[data-i18n-template="summary.severityValue"]').forEach(function(el) {
            var rendered = t('summary.severityValue');
            rendered = rendered.split('{value}').join(translateSeverityValue(el.dataset.value || ''));
            el.textContent = rendered;
        });
        document.querySelectorAll('[data-review-signals]').forEach(function(el) {
            var source = el.getAttribute('data-review-signals') || '';
            var parts = source.split('·').map(function(part) { return translateReviewSignalPart(part); });
            el.textContent = parts.join(' · ');
        });
        document.querySelectorAll('[data-policy-mode]').forEach(function(el) {
            var source = el.getAttribute('data-policy-mode') || el.textContent || '';
            el.textContent = translatePolicyMode(source);
        });
        document.querySelectorAll('[data-decision-reason]').forEach(function(el) {
            var source = el.getAttribute('data-decision-reason') || el.textContent || '';
            el.textContent = translateDecisionReason(source);
        });
        document.querySelectorAll('[data-regression-reason]').forEach(function(el) {
            var source = el.getAttribute('data-regression-reason') || el.textContent || '';
            el.textContent = translateRegressionReason(source);
        });
        document.querySelectorAll('[data-regression-severity]').forEach(function(el) {
            var source = el.getAttribute('data-regression-severity') || el.textContent || '';
            el.textContent = translateSeverityValue(source);
        });
        document.querySelectorAll('.ownership-toggle').forEach(function(btn) {
            var expanded = btn.dataset.expanded === 'true';
            btn.textContent = expanded ? t('button.hideFiles') : t('button.showFiles');
        });
    }

    function setDashboardLanguage(lang) {
        currentLang = I18N[lang] ? lang : 'en';
        if (langToggleEn) {
            langToggleEn.classList.toggle('active', currentLang === 'en');
            langToggleEn.setAttribute('aria-pressed', currentLang === 'en' ? 'true' : 'false');
        }
        if (langTogglePl) {
            langTogglePl.classList.toggle('active', currentLang === 'pl');
            langTogglePl.setAttribute('aria-pressed', currentLang === 'pl' ? 'true' : 'false');
        }
        applyTranslations();
        try { localStorage.setItem('dashboardLang', currentLang); } catch(e) {}
    }

    window.toggleOwnershipFiles = function(btn) {
        if (!btn) return;
        var expanded = btn.dataset.expanded === 'true';
        var nextExpanded = !expanded;
        var files = btn.nextElementSibling;
        if (files) files.style.display = nextExpanded ? 'block' : 'none';
        btn.dataset.expanded = nextExpanded ? 'true' : 'false';
        btn.textContent = nextExpanded ? t('button.hideFiles') : t('button.showFiles');
    };

    if (langToggleEn && langTogglePl) {
        langToggleEn.addEventListener('click', function() { setDashboardLanguage('en'); });
        langTogglePl.addEventListener('click', function() { setDashboardLanguage('pl'); });
        try {
            setDashboardLanguage(localStorage.getItem('dashboardLang') || 'en');
        } catch(e) {
            setDashboardLanguage('en');
        }
    } else {
        applyTranslations();
    }

    // -- Check expand/collapse --
    document.querySelectorAll('.check-row.expandable').forEach(function(row) {
        row.addEventListener('click', function() {
            var id = this.dataset.checkId;
            var outputRow = document.getElementById('check-output-' + id);
            var arrow = this.querySelector('.check-expand');
            if (outputRow && arrow) {
                outputRow.classList.toggle('open');
                arrow.classList.toggle('open');
            }
        });
    });

    // -- Expand all failed checks --
    var expandBtn = document.getElementById('expand-all-failed');
    if (expandBtn) {
        expandBtn.addEventListener('click', function() {
            document.querySelectorAll('.check-output').forEach(function(row) {
                row.classList.add('open');
            });
            document.querySelectorAll('.check-expand').forEach(function(arrow) {
                arrow.classList.add('open');
            });
        });
    }

    // -- Directory collapse/expand --
    document.querySelectorAll('.dir-header').forEach(function(header) {
        header.addEventListener('click', function() {
            var wrap = this.nextElementSibling;
            var chevron = this.querySelector('.dir-chevron');
            if (wrap) {
                wrap.classList.toggle('collapsed');
                if (chevron) chevron.classList.toggle('open');
            }
        });
    });

    // -- File search with debounce --
    var searchInput = document.getElementById('file-search');
    var searchTimeout;
    if (searchInput) {
        searchInput.addEventListener('input', function() {
            clearTimeout(searchTimeout);
            var input = this;
            searchTimeout = setTimeout(function() {
                applyFileFilters();
            }, 150);
        });
    }

    // -- File category toggle (Code / Tests / All) --
    var activeFileToggle = 'code';
    document.querySelectorAll('.file-toggle .toggle-btn').forEach(function(btn) {
        btn.addEventListener('click', function() {
            this.closest('.file-toggle').querySelectorAll('.toggle-btn').forEach(function(b) { b.classList.remove('active'); });
            this.classList.add('active');
            activeFileToggle = this.dataset.show;
            applyFileFilters();
        });
    });

    // -- Filter chips --
    document.querySelectorAll('.filter-chip').forEach(function(chip) {
        chip.addEventListener('click', function() {
            this.classList.toggle('active');
            applyFileFilters();
        });
    });

    // -- Sort select --
    var sortSelect = document.getElementById('sort-files');
    if (sortSelect) {
        sortSelect.addEventListener('change', function() {
            sortFiles(this.value);
        });
    }

    function applyFileFilters() {
        var q = (document.getElementById('file-search') || {}).value || '';
        q = q.toLowerCase();

        // Active status filters
        var activeStatuses = [];
        document.querySelectorAll('.filter-chip[data-status].active').forEach(function(c) {
            activeStatuses.push(c.dataset.status);
        });

        var hotspotOnly = document.querySelector('.filter-chip[data-filter="hotspot"]');
        hotspotOnly = hotspotOnly && hotspotOnly.classList.contains('active');

        var hideRenames = document.querySelector('.filter-chip[data-filter="hide-renames"]');
        hideRenames = hideRenames && hideRenames.classList.contains('active');

        // B7: Noise filters
        var hideAssets = document.querySelector('.filter-chip[data-filter="hide-assets"]');
        hideAssets = hideAssets && hideAssets.classList.contains('active');
        var hideI18n = document.querySelector('.filter-chip[data-filter="hide-i18n"]');
        hideI18n = hideI18n && hideI18n.classList.contains('active');
        var hideConfig = document.querySelector('.filter-chip[data-filter="hide-config"]');
        hideConfig = hideConfig && hideConfig.classList.contains('active');

        document.querySelectorAll('.dir-group').forEach(function(group) {
            var rows = group.querySelectorAll('.file-row');
            var anyVisible = false;
            rows.forEach(function(row) {
                var path = (row.dataset.path || '').toLowerCase();
                var status = row.dataset.status || '';
                var churn = parseInt(row.dataset.churn || '0', 10);
                var category = row.dataset.category || 'code';

                var show = true;
                if (q && path.indexOf(q) === -1) show = false;
                if (activeStatuses.length > 0 && activeStatuses.indexOf(status) === -1) show = false;
                if (hotspotOnly && churn < 80) show = false;
                if (hideRenames && status === 'R') show = false;
                // B7: category noise filters
                if (hideAssets && category === 'asset') show = false;
                if (hideI18n && category === 'i18n') show = false;
                if (hideConfig && category === 'config') show = false;
                // Category toggle (code/test/all)
                if (activeFileToggle !== 'all') {
                    if (category !== activeFileToggle) show = false;
                }

                row.style.display = show ? '' : 'none';
                if (show) anyVisible = true;
            });
            group.style.display = anyVisible ? '' : 'none';
            if (q && anyVisible) {
                var wrap = group.querySelector('.dir-files-wrap');
                var chevron = group.querySelector('.dir-chevron');
                if (wrap) wrap.classList.remove('collapsed');
                if (chevron) chevron.classList.add('open');
            }
        });
    }

    function sortFiles(mode) {
        var container = document.getElementById('files-container');
        if (!container) return;

        var groups = Array.from(container.querySelectorAll('.dir-group'));
        groups.forEach(function(group) {
            var wrap = group.querySelector('.dir-files-wrap');
            if (!wrap) return;
            var rows = Array.from(wrap.querySelectorAll('.file-row'));

            rows.sort(function(a, b) {
                if (mode === 'churn') {
                    return parseInt(b.dataset.churn || '0') - parseInt(a.dataset.churn || '0');
                } else if (mode === 'alpha') {
                    return (a.dataset.path || '').localeCompare(b.dataset.path || '');
                } else if (mode === 'status') {
                    var order = {A:0, M:1, D:2, R:3, C:4};
                    return (order[a.dataset.status] || 5) - (order[b.dataset.status] || 5);
                }
                return 0;
            });
            rows.forEach(function(row) { wrap.appendChild(row); });
        });

        if (mode === 'churn') {
            groups.sort(function(a, b) {
                var ca = parseInt(a.dataset.churn || '0');
                var cb = parseInt(b.dataset.churn || '0');
                return cb - ca;
            });
            groups.forEach(function(g) { container.appendChild(g); });
        }
    }

    applyFileFilters();

    function setCollapsibleExpanded(collapsible, expanded) {
        if (!collapsible || !collapsible.classList.contains('section-collapsible')) return;
        collapsible.classList.toggle('expanded', expanded);
        var header = collapsible.querySelector(':scope > .section-header');
        if (header) header.setAttribute('aria-expanded', expanded ? 'true' : 'false');
    }

    function expandForTarget(target) {
        if (!target) return;
        var collapsible = target.closest('.section-collapsible');
        if (collapsible) setCollapsibleExpanded(collapsible, true);
    }

    // -- Sidebar scroll tracking --
    var navLinks = document.querySelectorAll('.sidebar-nav a[href^="#"]');
    var sections = [];
    navLinks.forEach(function(link) {
        var id = link.getAttribute('href').slice(1);
        var el = document.getElementById(id);
        if (el) sections.push({ id: id, el: el, link: link });
    });

    function updateActiveNav() {
        if (!sections.length) return;
        var scrollY = window.scrollY + 120;
        var current = sections[0];
        for (var i = 0; i < sections.length; i++) {
            if (sections[i].el.offsetTop <= scrollY) {
                current = sections[i];
            }
        }
        navLinks.forEach(function(l) { l.classList.remove('active'); });
        if (current) current.link.classList.add('active');
    }

    function scrollToSection(id, behavior) {
        var target = document.getElementById(id);
        if (!target) return;
        expandForTarget(target);
        target.scrollIntoView({ behavior: behavior || 'smooth', block: 'start' });
        history.replaceState(null, '', '#' + id);
        updateActiveNav();
    }

    if (sections.length > 0) {
        window.addEventListener('scroll', updateActiveNav, { passive: true });
        updateActiveNav();
    }

    // -- Deep link: scroll to hash on load --
    if (window.location.hash) {
        var hashTarget = document.getElementById(window.location.hash.slice(1));
        if (hashTarget) {
            expandForTarget(hashTarget);
            setTimeout(function() {
                hashTarget.scrollIntoView({ behavior: 'smooth', block: 'start' });
                updateActiveNav();
            }, 100);
        }
    }

    navLinks.forEach(function(link) {
        link.addEventListener('click', function(e) {
            e.preventDefault();
            scrollToSection(this.getAttribute('href').slice(1), 'smooth');
        });
    });

    // -- Smooth-scroll for #file-* anchor links (hotspot badges etc.) --
    document.querySelectorAll('a[href^="#file-"]').forEach(function(link) {
        link.addEventListener('click', function(e) {
            e.preventDefault();
            var id = this.getAttribute('href').slice(1);
            var el = document.getElementById(id);
            if (el) {
                expandForTarget(el);
                // Expand parent dir-group if collapsed
                var dirWrap = el.closest('.dir-files-wrap');
                if (dirWrap && dirWrap.classList.contains('collapsed')) {
                    dirWrap.classList.remove('collapsed');
                    var chevron = dirWrap.previousElementSibling ? dirWrap.previousElementSibling.querySelector('.dir-chevron') : null;
                    if (chevron) chevron.classList.add('open');
                }
                el.scrollIntoView({ behavior: 'smooth', block: 'center' });
                history.replaceState(null, '', '#' + id);
                // Flash highlight animation
                el.classList.add('highlight-flash');
                setTimeout(function() { el.classList.remove('highlight-flash'); }, 1500);
            }
        });
    });

    // -- Diff modal --
    var diffOverlay = document.getElementById('diff-modal-overlay');
    var diffTitle = document.getElementById('diff-modal-title');
    var diffBody = document.getElementById('diff-modal-body');
    var diffSearch = document.getElementById('diff-modal-search');
    var diffCopyPath = document.getElementById('diff-copy-path');

    function closeDiffModal() {
        if (diffOverlay) diffOverlay.classList.remove('open');
    }

    function highlightDiffLines(text) {
        var lines = text.split('\n');
        var html = '';
        for (var i = 0; i < lines.length; i++) {
            var line = lines[i];
            // Escape HTML entities
            var escaped = line.replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;');
            if (line.indexOf('@@') === 0) {
                html += '<span class="diff-line-hunk">' + escaped + '</span>\n';
            } else if (line.indexOf('+') === 0 && line.indexOf('+++') !== 0) {
                html += '<span class="diff-line-add">' + escaped + '</span>\n';
            } else if (line.indexOf('-') === 0 && line.indexOf('---') !== 0) {
                html += '<span class="diff-line-del">' + escaped + '</span>\n';
            } else {
                html += escaped + '\n';
            }
        }
        return html;
    }

    function openDiffModal(path, patchPath) {
        if (!diffOverlay || !diffBody || !diffTitle) return;
        diffTitle.textContent = path;
        diffBody.innerHTML = '<pre style="color:var(--faint);padding:24px;text-align:center">' + t('misc.loading') + '</pre>';
        diffOverlay.classList.add('open');

        if (patchPath) {
            // Progressive enhancement: try fetch for modal, fallback to direct link (file:// CORS safe)
            fetch(patchPath).then(function(r) {
                if (!r.ok) throw new Error('HTTP ' + r.status);
                return r.text();
            }).then(function(text) {
                diffBody.innerHTML = '<pre>' + highlightDiffLines(text) + '</pre>';
            }).catch(function() {
                // fetch failed (file:// CORS, network error) — fallback to direct navigation
                closeDiffModal();
                window.open(patchPath, '_blank');
            });
        } else {
            diffBody.innerHTML = '<pre style="color:var(--faint);padding:24px">' + t('message.noPerFilePatch') + ' <a href="10_diff/full.patch" style="color:var(--accent)">' + t('button.viewFullPatch') + '</a></pre>';
        }
    }

    // File row click -> open diff (modal if fetch works, direct link otherwise)
    document.querySelectorAll('.file-row').forEach(function(row) {
        row.style.cursor = 'pointer';
        row.addEventListener('click', function(e) {
            // Don't trigger if user clicked an anchor link
            if (e.target.tagName === 'A') return;
            var path = this.dataset.path || '';
            var patchPath = this.dataset.patchPath || '';
            if (patchPath) {
                openDiffModal(path, patchPath);
            } else {
                // No per-file patch — open full.patch directly
                window.open('10_diff/full.patch', '_blank');
            }
        });
    });

    if (diffOverlay) {
        diffOverlay.addEventListener('click', function(e) {
            if (e.target === this) closeDiffModal();
        });
    }

    var closeBtn = document.getElementById('diff-modal-close');
    if (closeBtn) closeBtn.addEventListener('click', closeDiffModal);

    document.addEventListener('keydown', function(e) {
        if (e.key === 'Escape') closeDiffModal();
    });

    // Diff modal: copy path button
    if (diffCopyPath) {
        diffCopyPath.addEventListener('click', function() {
            var path = diffTitle ? diffTitle.textContent : '';
            if (navigator.clipboard && path) {
                navigator.clipboard.writeText(path).then(function() {
                    diffCopyPath.textContent = t('button.copied');
                    setTimeout(function() { diffCopyPath.textContent = t('button.copyPath'); }, 1500);
                }).catch(function() {});
            }
        });
    }

    // Diff modal: search in diff (uses TreeWalker on text nodes to avoid
    // corrupting HTML entities like &amp; when the search term overlaps them)
    if (diffSearch) {
        var diffSearchTimeout;
        diffSearch.addEventListener('input', function() {
            clearTimeout(diffSearchTimeout);
            var q = this.value;
            diffSearchTimeout = setTimeout(function() {
                if (!diffBody) return;
                var pre = diffBody.querySelector('pre');
                if (!pre) return;
                // Remove existing highlights (unwrap <span class="diff-line-match">)
                pre.querySelectorAll('.diff-line-match').forEach(function(el) {
                    var parent = el.parentNode;
                    while (el.firstChild) parent.insertBefore(el.firstChild, el);
                    parent.removeChild(el);
                    parent.normalize();
                });
                if (!q) return;
                var escaped = q.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
                var regex = new RegExp(escaped, 'gi');
                // Walk only text nodes — never touch innerHTML
                var walker = document.createTreeWalker(pre, NodeFilter.SHOW_TEXT, null, false);
                var textNodes = [];
                while (walker.nextNode()) textNodes.push(walker.currentNode);
                textNodes.forEach(function(node) {
                    var match, last = 0, frag = document.createDocumentFragment(), txt = node.nodeValue;
                    regex.lastIndex = 0;
                    while ((match = regex.exec(txt)) !== null) {
                        if (match.index > last) frag.appendChild(document.createTextNode(txt.slice(last, match.index)));
                        var mark = document.createElement('span');
                        mark.className = 'diff-line-match';
                        mark.textContent = match[0];
                        frag.appendChild(mark);
                        last = regex.lastIndex;
                    }
                    if (last === 0) return; // no matches in this node
                    if (last < txt.length) frag.appendChild(document.createTextNode(txt.slice(last)));
                    node.parentNode.replaceChild(frag, node);
                });
            }, 200);
        });
    }

    // -- Narrative copy button --
    var narrativeCopyBtn = document.getElementById('copy-narrative-btn');
    if (narrativeCopyBtn) {
        narrativeCopyBtn.addEventListener('click', function() {
            var content = document.querySelector('.narrative-content');
            if (content && navigator.clipboard) {
                navigator.clipboard.writeText(content.textContent).then(function() {
                    narrativeCopyBtn.textContent = t('button.copied');
                    setTimeout(function() { narrativeCopyBtn.textContent = t('button.copyMarkdown'); }, 1500);
                }).catch(function() {});
            }
        });
    }

    // -- Artifacts Explorer: copy path button --
    document.querySelectorAll('.artifact-copy-btn').forEach(function(btn) {
        btn.addEventListener('click', function() {
            var row = this.closest('.artifact-row');
            var path = row ? row.dataset.path : '';
            if (navigator.clipboard && path) {
                var btn = this;
                navigator.clipboard.writeText(path).then(function() {
                    btn.textContent = t('button.copied');
                    setTimeout(function() { btn.textContent = t('button.copyShort'); }, 1000);
                }).catch(function() {});
            }
        });
    });

    // -- Artifacts Explorer: search + kind filter --
    var artifactSearch = document.getElementById('artifact-search');
    var artifactSearchTimeout;
    if (artifactSearch) {
        artifactSearch.addEventListener('input', function() {
            clearTimeout(artifactSearchTimeout);
            artifactSearchTimeout = setTimeout(filterArtifacts, 150);
        });
    }
    document.querySelectorAll('.artifact-kind-chip').forEach(function(chip) {
        chip.addEventListener('click', function() {
            this.classList.toggle('active');
            filterArtifacts();
        });
    });
    function filterArtifacts() {
        var q = (artifactSearch ? artifactSearch.value : '').toLowerCase();
        var activeKinds = [];
        document.querySelectorAll('.artifact-kind-chip.active').forEach(function(c) {
            activeKinds.push(c.dataset.kind);
        });
        document.querySelectorAll('.artifact-row').forEach(function(row) {
            var path = (row.dataset.path || '').toLowerCase();
            var kind = row.dataset.kind || '';
            var show = true;
            if (q && path.indexOf(q) === -1) show = false;
            if (activeKinds.length > 0 && activeKinds.indexOf(kind) === -1) show = false;
            row.style.display = show ? '' : 'none';
        });
    }

    // -- B5: Copy PR Comment --
    var copyPrBtn = document.getElementById('copy-pr-comment');
    if (copyPrBtn) {
        copyPrBtn.addEventListener('click', function() {
            var report = {};
            try { report = JSON.parse(document.getElementById('report-data').textContent || '{}'); } catch(e) {}
            var gate = report.gate || {};
            var checks = report.checks || [];
            var diff = report.diff || {};
            var quality = report.quality || {};

            var comment = '## PR Review Summary\n\n';
            var mergeLabel = gate.recommended_label || (gate.recommended_merge ? 'MERGE' : (gate.allow_merge ? 'HOLD' : 'BLOCK'));
            comment += '**Gate:** ' + (gate.status || 'N/A') + ' | **Quality:** ' + (gate.quality_pass ? 'PASS' : 'FAIL') + ' | **Merge:** ' + mergeLabel + '\n\n';
            if (gate.summary) {
                comment += '**Decision:** ' + gate.summary + '\n\n';
            }
            if (gate.review_caveats && gate.review_caveats.length) {
                comment += '**Review signals:** ' + gate.review_caveats.join(' · ') + '\n\n';
            }

            var passed = checks.filter(function(c) { return c.status === 'PASS'; }).length;
            var failed = checks.filter(function(c) { return c.status === 'FAIL' || c.status === 'ERROR'; }).length;
            comment += '**Checks:** ' + passed + ' passed, ' + failed + ' failed\n\n';

            if (quality.breaking_changes && quality.breaking_changes.has_breaking) {
                comment += '**Breaking:** ' + (quality.breaking_changes.summary || 'Yes') + '\n\n';
            }

            if (quality.coverage) {
                var pct = Math.round(quality.coverage.heuristic_ratio * 100);
                comment += '**Coverage heuristic:** ' + pct + '% (' + quality.coverage.matched + '/' + quality.coverage.total + ')\n\n';
            }

            var hotspots = (diff.files || [])
                .filter(function(f) { return f.is_hotspot; })
                .sort(function(a, b) { return b.churn - a.churn; })
                .slice(0, 5);
            if (hotspots.length > 0) {
                comment += '**Top hotspots:**\n';
                hotspots.forEach(function(f) {
                    comment += '- `' + f.path + '` (+' + f.additions + '/-' + f.deletions + ')\n';
                });
                comment += '\n';
            }

            comment += '---\n*Generated by [prview](https://github.com/vetcoders/prview)*';

            var btn = copyPrBtn;
            if (navigator.clipboard) {
                navigator.clipboard.writeText(comment).then(function() {
                    btn.textContent = t('button.copied');
                    setTimeout(function() { btn.textContent = t('button.copyPrComment'); }, 2000);
                }).catch(function() {});
            }
        });
    }

    // -- Review/Author view toggle (PRV-105) --
    var reviewViewBtn = document.getElementById('view-mode-review');
    var authorViewBtn = document.getElementById('view-mode-author');

    function setAuthorMode(enabled) {
        document.body.classList.toggle('author-mode', enabled);
        if (reviewViewBtn) {
            reviewViewBtn.classList.toggle('active', !enabled);
            reviewViewBtn.setAttribute('aria-pressed', !enabled ? 'true' : 'false');
        }
        if (authorViewBtn) {
            authorViewBtn.classList.toggle('active', enabled);
            authorViewBtn.setAttribute('aria-pressed', enabled ? 'true' : 'false');
        }
        try { sessionStorage.setItem('authorMode', enabled ? 'true' : 'false'); } catch(e) {}
    }

    if (reviewViewBtn && authorViewBtn) {
        reviewViewBtn.addEventListener('click', function() { setAuthorMode(false); });
        authorViewBtn.addEventListener('click', function() { setAuthorMode(true); });
        try {
            setAuthorMode(sessionStorage.getItem('authorMode') === 'true');
        } catch(e) {
            setAuthorMode(false);
        }
    }

    // -- Dashboard v2: Collapsible sections --
    document.querySelectorAll('.section-collapsible > .section-header').forEach(function(header) {
        header.addEventListener('click', function() {
            var collapsible = this.closest('.section-collapsible');
            setCollapsibleExpanded(collapsible, !collapsible.classList.contains('expanded'));
        });
        header.addEventListener('keydown', function(e) {
            if (e.key === 'Enter' || e.key === ' ') {
                e.preventDefault();
                var collapsible = this.closest('.section-collapsible');
                setCollapsibleExpanded(collapsible, !collapsible.classList.contains('expanded'));
            }
        });
    });

    // -- Dashboard v2: Tab switching --
    document.querySelectorAll('.tab-btn').forEach(function(btn) {
        btn.addEventListener('click', function() {
            var container = this.closest('.tab-container');
            container.querySelectorAll('.tab-btn').forEach(function(b) { b.classList.remove('active'); });
            container.querySelectorAll('.tab-panel').forEach(function(p) { p.classList.remove('active'); });
            this.classList.add('active');
            var panel = container.querySelector('#' + this.dataset.tab);
            if (panel) panel.classList.add('active');
        });
    });
});
"##
}
