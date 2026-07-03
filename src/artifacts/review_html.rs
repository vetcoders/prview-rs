//! Consolidated REVIEW_SUMMARY.md and standard-review HTML rendering.

use super::*;
use crate::mdrender::{self, Theme};

/// Consolidated human-readable review summary.
///
/// Merges MERGE_GATE.md + PR_REVIEW.md + AI_INDEX.md sections into one file.
/// Runs AFTER all artifacts exist, so `.exists()` checks are accurate (fixes A4).
pub(crate) fn generate_review_summary(out_dir: &Path) -> Result<()> {
    use std::fmt::Write;

    let mut md = String::new();
    writeln!(md, "# PR Review Summary\n")?;

    // Section 1: Gate Decision (from MERGE_GATE.json)
    let gate_path = Path::new("00_summary/MERGE_GATE.json");
    if out_dir.join(gate_path).exists() {
        writeln!(md, "## Gate Decision\n")?;
        if let Ok(raw) = read_to_string_within(out_dir, gate_path)
            && let Ok(gate) = serde_json::from_str::<serde_json::Value>(&raw)
        {
            let verdict = gate["decision"]["verdict"].as_str().unwrap_or("UNKNOWN");
            let reason = gate["decision"]["decision_reason"]
                .as_str()
                .or_else(|| gate["decision"]["reason"].as_str())
                .unwrap_or("No reason");
            writeln!(
                md,
                "**Verdict: `{}`**\n\nRecommended: {}\n",
                verdict, reason
            )?;
        }
    }

    // Section 2: Review (from PR_REVIEW.md)
    let review_path = Path::new("PR_REVIEW.md");
    if out_dir.join(review_path).exists() {
        writeln!(md, "## Review\n")?;
        let content = read_to_string_within(out_dir, review_path)?;
        // Skip only the first title line and any blank lines after it
        for line in content.lines().skip(1).skip_while(|l| l.is_empty()) {
            writeln!(md, "{}", line)?;
        }
        writeln!(md)?;
    }

    writeln!(md, "## Artifact Map\n")?;
    writeln!(md, "## Available Artifacts\n")?;

    let pattern_scan = out_dir.join("30_context/PATTERN_SCAN.json");
    if pattern_scan.exists() {
        writeln!(md, "- `30_context/PATTERN_SCAN.json` — pattern scan")?;
    }

    let deps_delta = out_dir.join("30_context/DEPS_DELTA.json");
    if deps_delta.exists() {
        writeln!(md, "- `30_context/DEPS_DELTA.json` — dependency changes")?;
    }

    if out_dir.join("30_context/cargo-sbom.txt").exists() {
        writeln!(
            md,
            "- `30_context/cargo-sbom.txt` — dependency/license SBOM"
        )?;
    }
    if out_dir.join("30_context/npm-sbom.txt").exists() {
        writeln!(md, "- `30_context/npm-sbom.txt` — dependency SBOM")?;
    }

    if out_dir.join("30_context/INLINE_FINDINGS.sarif").exists() {
        writeln!(
            md,
            "- `30_context/INLINE_FINDINGS.sarif` — machine-readable findings"
        )?;
    }
    writeln!(md)?;

    fs::write(out_dir.join("REVIEW_SUMMARY.md"), md)?;
    Ok(())
}

pub(crate) fn generate_standard_review_html(out_dir: &Path) -> Result<()> {
    let summary = read_optional_artifact(out_dir, "REVIEW_SUMMARY.md");
    let gate = read_optional_artifact(out_dir, "00_summary/MERGE_GATE.md");
    let failures = read_optional_artifact(out_dir, "00_summary/FAILURES_SUMMARY.md");
    let ai_index = read_optional_artifact(out_dir, "AI_INDEX.md");

    let gate_json = read_to_string_within(out_dir, Path::new("00_summary/MERGE_GATE.json"))
        .ok()
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok());
    let verdict = gate_json
        .as_ref()
        .and_then(|gate| {
            gate["decision"]["verdict"]
                .as_str()
                .or_else(|| gate["verdict"].as_str())
        })
        .unwrap_or("UNKNOWN");
    let reason = gate_json
        .as_ref()
        .and_then(|gate| {
            gate["decision"]["decision_reason"]
                .as_str()
                .or_else(|| gate["decision"]["reason"].as_str())
                .or_else(|| gate["summary"].as_str())
        })
        .unwrap_or("No gate reason recorded");

    let verdict_class = match verdict.to_ascii_uppercase().as_str() {
        "ALLOW" | "PASS" | "MERGEABLE" | "CLEAN" => "v-pass",
        "BLOCK" | "FAIL" | "BLOCKED" => "v-block",
        "CONDITIONAL" | "WARN" => "v-warn",
        _ => "v-hold",
    };

    let dashboard_link = if out_dir.join("dashboard.html").exists() {
        r#"<a href="dashboard.html">Open interactive dashboard</a>"#
    } else {
        r#"<span class="muted">Interactive dashboard disabled for this run</span>"#
    };
    let inline_link = if out_dir.join("30_context/INLINE_FINDINGS.sarif").exists() {
        r#"<a href="30_context/INLINE_FINDINGS.sarif">INLINE_FINDINGS.sarif</a>"#
    } else {
        r#"<span class="muted">No inline SARIF generated</span>"#
    };

    let theme = brand_theme();
    let root_css = brand::root_css();

    let html = format!(
        r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>prview standard review</title>
{favicon}
<style>
{root_css}body {{ margin:0; background:var(--bg); color:var(--fg); font:14px/1.5 ui-sans-serif, system-ui, -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif; }}
main {{ max-width:1100px; margin:0 auto; padding:28px; }}
header {{ border-bottom:1px solid var(--line); margin-bottom:20px; padding-bottom:16px; }}
.wordmark {{ font-family:var(--font-heading); font-weight:700; font-size:30px; letter-spacing:-0.03em; line-height:1; display:inline-flex; align-items:baseline; gap:4px; }}
.wordmark .dot {{ width:7px; height:7px; border-radius:50%; background:var(--signal); display:inline-block; }}
.report-type {{ font-family:var(--mono); text-transform:uppercase; letter-spacing:0.16em; font-size:11px; font-weight:700; color:var(--muted); margin-top:8px; }}
h1 {{ margin:0 0 8px; font-size:28px; font-family:var(--font-heading); letter-spacing:-0.02em; }}
h2 {{ margin-top:28px; border-bottom:1px solid var(--line); padding-bottom:6px; font-family:var(--font-heading); }}
h3, h4 {{ margin-top:18px; font-family:var(--font-heading); }}
.cards {{ display:grid; grid-template-columns:repeat(auto-fit,minmax(220px,1fr)); gap:12px; margin:18px 0; }}
.card, details {{ background:var(--surface); border:1px solid var(--line); border-radius:10px; }}
.card {{ padding:14px; }}
.k {{ color:var(--muted); font-family:var(--mono); font-size:11px; text-transform:uppercase; letter-spacing:.08em; }}
.v {{ font-size:20px; font-weight:700; margin-top:6px; font-family:var(--font-heading); }}
.badge {{ display:inline-block; border:1px solid var(--line); border-radius:999px; padding:3px 10px; font-family:var(--mono); font-size:12px; font-weight:650; color:var(--muted); }}
.badge.v-pass {{ color:var(--pass); border-color:var(--pass); }}
.badge.v-warn {{ color:var(--warn); border-color:var(--warn); }}
.badge.v-hold {{ color:var(--hold); border-color:var(--hold); }}
.badge.v-block {{ color:var(--block); border-color:var(--block); }}
.muted {{ color:var(--muted); }}
nav {{ margin:6px 0 4px; }}
nav a, nav span {{ margin-right:14px; }}
nav a {{ color:var(--fg); text-decoration:none; border-bottom:1px solid var(--line); padding-bottom:1px; }}
nav a:hover {{ border-bottom-color:var(--signal); }}
summary {{ cursor:pointer; padding:12px 14px; font-weight:700; font-family:var(--font-heading); }}
details[open] > summary {{ border-bottom:1px solid var(--line); }}
.body {{ padding:0 14px 16px; }}
pre {{ overflow:auto; background:rgba(127,127,127,.10); border:1px solid var(--line); border-radius:8px; padding:12px; }}
code, pre {{ font-family:var(--mono); }}
/* --- markdown renderer (mdrender), scoped under .mdr --- */
{mdr_css}</style>
</head>
<body>
<main>
<header>
<div class="wordmark">prview<span class="dot"></span></div>
<div class="report-type">standard review</div>
<div class="muted" style="margin-top:8px">Portable human export generated from the artifact pack.</div>
</header>
<section class="cards">
<div class="card"><div class="k">Gate verdict</div><div class="v"><span class="badge {verdict_class}">{verdict}</span></div><div class="muted">{reason}</div></div>
<div class="card"><div class="k">Primary HTML</div><div class="v">review.html</div><div class="muted">Always generated, even when dashboard is disabled.</div></div>
</section>
<nav>{dashboard_link}<a href="report.json">report.json</a><a href="00_summary/MERGE_GATE.json">MERGE_GATE.json</a>{inline_link}</nav>
<details open><summary>Review Summary</summary><div class="body">{summary_html}</div></details>
<details><summary>Merge Gate</summary><div class="body">{gate_html}</div></details>
<details><summary>Failures Summary</summary><div class="body">{failures_html}</div></details>
<details><summary>AI Index</summary><div class="body">{ai_index_html}</div></details>
{brand_footer}
</main>
</body>
</html>
"#,
        favicon = BRAND_FAVICON_LINK_TAG,
        root_css = root_css,
        brand_footer = brand::mini_footer_html(),
        verdict = escape_basic_html(verdict),
        verdict_class = verdict_class,
        reason = escape_basic_html(reason),
        dashboard_link = dashboard_link,
        inline_link = inline_link,
        mdr_css = mdrender::stylesheet(&theme),
        summary_html = mdrender::render(&summary, &theme),
        gate_html = mdrender::render(&gate, &theme),
        failures_html = mdrender::render(&failures, &theme),
        ai_index_html = mdrender::render(&ai_index, &theme),
    );

    fs::write(out_dir.join("review.html"), html)?;
    Ok(())
}

pub(crate) fn read_optional_artifact(out_dir: &Path, rel: &str) -> String {
    read_to_string_within(out_dir, Path::new(rel)).unwrap_or_default()
}

/// Brand-aligned [`Theme`](crate::mdrender::Theme) for the standard-review export.
///
/// Color tokens reference the page's CSS variables (defined in the inline
/// `<style>`), so rendered markdown flips with the same OS light/dark system as
/// the rest of the artifact. `signal` stays reserved for tiny accents, so links
/// use the foreground color and callouts reuse the AA-cleared status variables
/// rather than the bright signal.
fn brand_theme() -> Theme {
    brand::review_theme()
}

pub(crate) fn escape_basic_html(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}
