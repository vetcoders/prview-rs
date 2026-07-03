# AI_INDEX Contract (v1.0)

`AI_INDEX.md` is the human- and agent-friendly entry point into an artifact pack.

This is a semantic contract, not a byte-for-byte snapshot format. Wording may
evolve, but the navigation intent and required sections should remain stable.

## Purpose

- Point a reviewer or agent at the highest-signal files first
- Keep the reading order compact and explicit
- Link out to canonical sources instead of duplicating raw data

## Required header fields

The document must start with `# AI Review Index` and include these top-level
metadata bullets near the top:

- `Generated`
- `PR`
- `Base`
- `Head`
- `Commits`
- `Files changed`
- `Reviewed range`

The header may also include compact structural context such as hotspot counts or
loctree twin counts.

## Required sections

The following sections must be present:

- `## Recommended reading order (agent-friendly)`
- `## Layout`
- `## If you need deeper context`
- `## Packs`

## Required references

The reading order must always include references to:

- `00_summary/MERGE_GATE.md`
- `00_summary/MERGE_GATE.json`
- `PR_REVIEW.md`

The deeper-context section must always include references to:

- `20_quality/full-checks.log`
- `10_diff/full.patch`
- `10_diff/per-commit-diffs/`

The packs section must describe one of:

- `artifacts.zip`
- a compact note that ZIP output was disabled for the run

## Optional references

These references are best-effort and may appear only when the file exists for a
given run:

- `00_summary/FAILURES_SUMMARY.md`
- `00_summary/RISK_HOTSPOTS.md`
- `30_context/INLINE_FINDINGS.sarif`
- `10_diff/per-commit-diffs/00-SUMMARY.md`
- `00_summary/file-status.txt`
- `30_context/changed-tests.txt`

An optional `## Deferred diagnostics` section may appear when the run
intentionally skips heavy context artifacts but wants to explain whether they
are recommended and why.

## Guarantees

- Paths are relative to the artifact root
- The reading order is numbered and prioritized
- The file stays compact and avoids raw log or JSON dumps
- `AI_INDEX.md` is navigational, not canonical

## Non-goals

- It does not replace `report.json`
- It does not replace `PR_REVIEW.md`
- It does not duplicate full findings, logs, or patches
