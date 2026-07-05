# Dashboard locales

`en.json` and `pl.json` are embedded into the self-contained HTML dashboard with
`include_str!`. Edit `pl.json` directly when adjusting Polish UI copy; no Rust
string editing is needed for dictionary entries.

Rules for editing:

- Keep the same key set in `en.json` and `pl.json`.
- Values must stay JSON strings.
- Keep placeholders such as `{count}`, `{pct}`, `{passed}`, and `{total}` intact.
- Keep product/tool names and common dev terms natural: `prview`, `PR`, `merge`,
  `review`, `runtime`, `dashboard`, `i18n`, `Loctree`, `Vibecrafted`.
- Follow the vetcoders-agents localization canon:
  `vibecrafted_glossary_rules-PL.md` and
  `vibecrafted-skill-PL-localization-spec.md`. A prview-specific glossary is
  planned there as `prview_glossary_rules-PL.md`; until it exists, use the
  general glossary plus the existing dashboard tone.

Run the parity/embed tests locally:

```bash
cargo test dashboard_
```
