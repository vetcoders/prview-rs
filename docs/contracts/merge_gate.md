# MERGE_GATE Contract (v1.0)

`MERGE_GATE.json` defines policy-aware merge decision output.

## Required fields

- `schema_version` (`"1.0"`)
- `generated_at` (ISO datetime)
- `bridge_stage` (`0..4`)
- `target`, `bases`, `profile` (resolved runtime values, not raw CLI input)
- `policy.version`, `policy.mode`, `policy.default_severity`, `policy.source`
- `checks[]`
- `inline_findings.file/status/severity/blocking/findings_count`
  - `inline_findings.file` is `null` when no SARIF file is emitted for the run
- `decision.verdict` (`"PASS"` | `"CONDITIONAL"` | `"BLOCK"`) — single-field gate for AI consumers
- `decision.allow_merge/decision_reason/blocking_issues`
- `files` — paths relative to artifact root, using subdirectory layout:
  - `merge_gate_json`: `"00_summary/MERGE_GATE.json"`
  - `merge_gate_md`: `"00_summary/MERGE_GATE.md"`
  - `inline_findings`: `"30_context/INLINE_FINDINGS.sarif"` when emitted, otherwise `null`
  - `checks_log`: `"20_quality/full-checks.log"`
  - `full_patch`: `"10_diff/full.patch"`
  - `dashboard`: `"dashboard.html"`

## Check item schema

Every element in `checks` contains:

- `id`
- `name`
- `status`
- `class` (`PASS|SKIP|FAIL|INFO`)
- `severity` (`block|warn|ignore`)
- `blocking`
- `duration_secs`
- `evidence`

## Verdict semantics

The `decision.verdict` field collapses three signals into one enum for AI consumers:

| verdict | allow_merge | quality_pass | Meaning |
|---|---|---|---|
| `PASS` | true | true | All gates green, safe to merge |
| `CONDITIONAL` | true | false | Policy allows merge despite quality issues |
| `BLOCK` | false | * | Merge blocked by policy violations |

Existing fields (`allow_merge`, `quality_pass`, `policy_allow_merge`) are kept for backwards compatibility.

## Blocking rules

- `shadow`: never blocks.
- `warn`: blocks only `FAIL + block`.
- `block`: blocks `FAIL + (block|warn)`.
