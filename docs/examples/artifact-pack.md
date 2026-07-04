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
│   └── BREAKING_CHANGES.md   # Removed pub symbols, changed signatures
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
