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

- `## Recommended reading order (human-first)`
- `## Layout`
- `## If you need deeper context`
- `## Packs`

## Required references

The reading order must always include references to:

- `dashboard.html` by default, or `review.html` with `--no-dashboard`
- `00_summary/MERGE_GATE.md`
- `00_summary/MERGE_GATE.json`
- `00_summary/PROVENANCE.json`
- `PR_REVIEW.md`

Exactly one HTML entry point is generated for a run. It is the first reading
step, and the layout section names the same file. `dashboard.html` is the
default guided human report and offline evidence reader; `review.html` is the
static fallback selected by `--no-dashboard`. The reference follows the output
configuration even though the HTML may be generated after the index itself.

This reading order is a minimum navigation path, not a complete inventory. The
dashboard also exposes the full diff, check evidence, source locations and
other available artifacts without requiring a reader to find them manually in
the extracted pack. Full text is embedded within bounded size limits; the
original files remain the source of truth. See the
[browser reading guide](../usage.md#read-a-pack-in-the-browser).

`PROVENANCE.json` follows the gate data directly: a verdict is only as good as
the substrate it was computed on, so the reading order places what was judged
(target and base commits, separately named operator checkout state, the tree each check actually
scanned, and any `PROVENANCE_CONTRADICTION` between those statements) next to the judgement itself. It is written after `AI_INDEX.md` and is
mandatory for every pack, so the reference is unconditional rather than
existence-guarded.

The index states the limited self-check scope: SANITY/CONSISTENCY_CHECK test
selected artifact invariants, counters, provenance and checklist consistency;
manifest verification samples at most five files. Their success does not
independently validate classifier or matching results, scan completeness, or
the reviewed code.

The deeper-context section must always include references to:

- `20_quality/full-checks.log`
- `10_diff/full.patch`
- `10_diff/per-commit-diffs/00-SUMMARY.md` when emitted, otherwise a compact
  note that no per-commit context was emitted, naming the directory and full patch

At most 50 commits receive individual or batched patch files. Above that limit,
the index describes the per-commit directory as **summary-only**, states the
50-commit limit, and points to `10_diff/full.patch` for the full reviewed range.
The summary still lists the top ten commits by churn. The directory reference
must not promise commit patch files when they were skipped.

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

When SARIF is readable, the index counts its emitted result rows by
`properties.finding_class`: source, dependency, general, and unknown for absent
or unrecognized values. It describes these as tool observations, keeping
`properties.classification` (diff origin) separate. A dependency or general row
must not be described as a diagnostic in changed source. Raw SARIF remains
canonical for per-row location, severity and origin.

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
