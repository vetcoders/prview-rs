# Example Artifact Pack

When you run `prview`, it generates an artifact pack in `~/.prview/runs/<repo>/<branch>/<run_id>/` (or `$PRVIEW_HOME/...`).
New run ids use a timestamp plus short HEAD suffix, for example `20260704-120500-a1b2c3d`; treat the value as opaque.
This pack contains all the analysis output in both human-readable and machine-readable formats.

## Directory Structure

A typical artifact pack looks like this:

```
├── PR_REVIEW.md             # Unified review narrative with risks and recommendations
├── report.json              # Full machine-readable report payload on disk
├── 00_summary/
│   ├── RUN.json             # Run metadata + execution mode + check inventory
│   ├── FAILURES_SUMMARY.md  # Compact blocking failures with links to logs
│   ├── MANIFEST.json        # SHA256 hashes for generated files
│   ├── SANITY.json          # Integrity validation results
│   ├── MERGE_GATE.json       # Machine-readable merge decision
│   ├── MERGE_GATE.md         # Human-readable merge decision
│   ├── pr-metadata.txt       # Branch/base/profile metadata
│   ├── file-status.txt       # A/M/D + file paths
│   └── commit-list.txt       # hash date author message
├── 10_diff/
│   ├── full.patch            # Full diff with diff-stat header
│   ├── per-commit-diffs/     # Individual commit patches
│   └── per-file-diffs/       # Hotspot files (>=80 lines changed)
├── 20_quality/
│   ├── *.result.json         # Per-check machine-readable outputs
│   ├── *.log                 # Per-check raw logs
│   ├── coverage-delta.txt    # Source↔test mapping with change status
│   ├── PUBLIC_API_DIFF.json  # Compatibility rows + lossless repo-backed Rust API delta
│   ├── PUBLIC_API_DIFF.md    # Human API summary
│   ├── BREAKING_CHANGES.json # Same lossless Rust delta used by the merge gate
│   └── BREAKING_CHANGES.md   # Human Rust API truth + bounded JS/TS/env signals
├── 30_context/
│   ├── INLINE_FINDINGS.sarif # Optional SARIF output for findings
│   ├── cargo-tree.txt        # Dependency tree
│   ├── cargo-sbom.json       # Generated SBOM
│   └── npm-sbom.json         # Generated SBOM
├── dashboard.html            # Visual HTML summary
└── artifacts.zip             # Everything zipped
```

## Key Files

1. **`PR_REVIEW.md`**: The main entry point for a human reviewer. It contains a narrative summary of the PR, including structural risks, test coverage gaps, and architectural insights.
2. **`00_summary/MERGE_GATE.json`**: The canonical source of truth for CI/CD automation. It determines if the PR is safe to merge based on the active `.prview-policy.yml`.
3. **`dashboard.html`**: A zero-dependency, self-contained HTML dashboard that visualizes the PR metrics, test hotspots, and quality gates.
4. **`report.json`**: The complete state of the analysis, useful for building custom integrations or training AI models.

For Rust, `PUBLIC_API_DIFF.json`, `BREAKING_CHANGES.json`, `report.json`, and
`00_summary/MERGE_GATE.json` serialize the same revision-backed API-delta IDs,
counts, confidence, evidence, unknown reasons, and base/target provenance. The
delta is frozen before checks from exact comparison anchors; an equal-OID dirty
local target uses an immutable tracked `WorkingTreeOverlay`, while clean,
untracked-only, remote, and off-HEAD targets use exact Git trees. Its tracked
digest is separate from the broader pack worktree digest. The
legacy compatibility rows in `PUBLIC_API_DIFF.json` and the JS/TS and
environment sections in `BREAKING_CHANGES.md` are separate bounded signals;
they are not a second Rust API analyzer. The generic `CONSISTENCY_CHECK` keeps
its existing file/finding/verdict checks and intentionally does not reconstruct
Rust API facts into a scalar count; equality is enforced by the lossless
artifact contract tests instead.
