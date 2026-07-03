# MERGE_GATE Contract (schema 2.1)

`MERGE_GATE.json` is the policy-aware merge decision emitted at
`00_summary/MERGE_GATE.json`. It is the single machine-readable verdict surface
consumed by AI reviewers, CI, and the `prview mcp` adapter. This document is
derived from the emitter `src/artifacts/merge_gate.rs` and the decision engine
`src/policy/engine.rs` + `src/artifacts/verdict.rs`. When the code and this
document disagree, the code is the contract and this document is the bug.

## Top-level object

| Field | Type | Notes |
|---|---|---|
| `schema_version` | string | `"2.1"` |
| `generated_at` | string | RFC 3339 local datetime |
| `bridge_stage` | integer | `0..4` |
| `target` | string | Resolved target branch name (not raw CLI input) |
| `bases` | string[] | Resolved base branch names |
| `profile` | string | Resolved profile kind |
| `policy` | object | `{ version, mode, default_severity, source }` |
| `checks` | object[] | Per-check evaluation records (see below) |
| `inline_findings` | object | Inline SARIF summary (see below) |
| `decision` | object | The merge decision (see below) |
| `files` | object | Artifact-root-relative paths (see below) |

### `policy`

| Field | Type | Notes |
|---|---|---|
| `version` | string | Policy document version |
| `mode` | string | Policy mode (e.g. `shadow` / `warn` / `block`) |
| `default_severity` | string | `block` \| `warn` \| `ignore` |
| `source` | string | Path to the resolved policy file |

### `files`

Paths are relative to the artifact root, using the subdirectory pack layout:

| Field | Value |
|---|---|
| `merge_gate_json` | `"00_summary/MERGE_GATE.json"` |
| `inline_findings` | `"30_context/INLINE_FINDINGS.sarif"` when emitted, otherwise `null` |
| `full_patch` | `"10_diff/full.patch"` |
| `checks_log` | `"20_quality/full-checks.log"` |
| `dashboard` | `"dashboard.html"` |

`MERGE_GATE.md` is also written beside the JSON, but it is a human-readable
companion and is not listed in `files`.

## `checks[]` — check item schema

Every element of `checks` is one policy evaluation record:

| Field | Type | Notes |
|---|---|---|
| `id` | string | Policy check id (`check_id`) |
| `name` | string | Human-readable check name |
| `status` | string | Raw check status |
| `execution_state` | string | `executed` \| `skipped` \| `unavailable` \| `unknown` |
| `outcome` | string | `passed` \| `findings_failed` \| `findings_warning` \| `system_error` \| `skipped` \| `unavailable` \| `unknown` |
| `class` | string | `PASS` \| `SKIP` \| `FAIL` \| `INFO` |
| `severity` | string | `block` \| `warn` \| `ignore` |
| `policy_conclusion` | string | `satisfied` \| `advisory` \| `blocked` |
| `confidence_impact` | string | `complete` \| `degraded` \| `incomplete` |
| `merge_impact` | string | `approve` \| `review_required` \| `block` |
| `blocking` | boolean | True iff `merge_impact == block` |
| `duration_secs` | number | Non-negative; `0.0` for a check with no executed result |
| `cached` | boolean \| null | `null` for a check with no executed result |
| `reason` | string \| null | Policy reason, when present |
| `evidence` | string | `20_quality/<artifact_id>.result.json` for an executed check; otherwise the reason text or `"skipped — no artifact generated"` |
| `log` | string \| null | `20_quality/<artifact_id>.log` for an executed check, else `null` |

Skipped or unavailable checks carry no executed `CheckResult`, so `duration_secs`
is `0.0`, `cached` is `null`, `log` is `null`, and `evidence` degrades to a
non-empty placeholder. These are contract-valid placeholders, never `null`
evidence — the artifact must not fail its own gate on a runner that lacks a tool.

## `inline_findings`

| Field | Type | Notes |
|---|---|---|
| `file` | string \| null | `"30_context/INLINE_FINDINGS.sarif"` when `findings_count > 0`, else `null` |
| `file_exists` | boolean | `findings_count > 0` |
| `status` | string | Inline analysis status |
| `severity` | string | `block` \| `warn` \| `ignore` |
| `blocking` | boolean | Whether inline findings block the merge |
| `findings_count` | integer | Total inline findings |
| `introduced_count` | integer | Findings this diff introduced (`in_diff == true`) |
| `preexisting_count` | integer | Pre-existing whole-repo findings (`in_diff == false`) |

`introduced_count + preexisting_count` may be less than `findings_count`: the
split counts only tool-finding rows and excludes cargo-audit / check SARIF rows.

## `decision`

The decision object is the merge verdict. Its scalar fields are derived from two
authoritative axes — `analysis_status` (confidence) and `merge_recommendation`
(policy) — plus `quality_pass`, through a single function.

| Field | Type | Notes |
|---|---|---|
| `analysis_status` | string | `complete` \| `degraded` \| `incomplete` |
| `merge_recommendation` | string | `approve` \| `review_required` \| `block` |
| `verdict` | string | `PASS` \| `CONDITIONAL` \| `BLOCK` — the single-field gate for AI consumers |
| `allow_merge` | boolean | Derived: `true` **iff** `verdict == "PASS"` |
| `policy_allow_merge` | boolean | Whether policy hard-blocked (no blocking issues); NOT the same as `allow_merge` |
| `quality_pass` | boolean | No new quality failures from this diff |
| `recommended_merge` | boolean | Legacy flag: `merge_recommendation == approve` |
| `recommended_label` | string | Human gate label (e.g. `MERGE`, `MERGE WITH REVIEW`, `HOLD`, `BLOCK`) |
| `quality_failures` | string[] | Names of quality failures |
| `introduced_quality_failures` | string[] | Failures introduced by this diff |
| `preexisting_quality_failures` | string[] | Pre-existing failures |
| `mixed_quality_failures` | string[] | Mixed-provenance failures |
| `unclassified_quality_failures` | string[] | Failures with unknown provenance |
| `quality_failure_details` | object[] | `[{ name, classification }]` |
| `decision_reason` | string | Human-readable reason for the verdict |
| `review_caveats` | string[] | Non-blocking caveats requiring reviewer attention |
| `blocking_issues` | string[] | Issues that block the merge |

## Verdict semantics

`verdict` collapses the decision into one enum for AI consumers. It is produced
by `derive_decision` (`src/artifacts/verdict.rs`), which calls
`MergeRecommendation::legacy_verdict` (`src/policy/engine.rs`):

| verdict | condition |
|---|---|
| `BLOCK` | `merge_recommendation == block` |
| `CONDITIONAL` | `merge_recommendation == review_required`, OR `approve` with degraded/incomplete analysis or failing quality |
| `PASS` | `merge_recommendation == approve` AND `analysis_status == complete` AND `quality_pass == true` |

## Invariants

- **`allow_merge == (verdict == "PASS")`.** `allow_merge` is derived in
  `derive_decision` and set nowhere else. The contradictory state
  `allow_merge: true` beside a `CONDITIONAL` or `BLOCK` verdict is
  unrepresentable (PV-03).
- **`derive_decision` is the single source** of `verdict`, `allow_merge`, and
  `recommended_merge`. No caller sets these fields independently.
- **`policy_allow_merge` is a distinct axis** ("policy did not hard-block") and
  is not conflated with `allow_merge` or the recommendation.
- **An executed check always carries its result artifact and log** (non-null
  `evidence` + `log`); a non-executed check carries non-null placeholders, never
  `null` evidence.

`HOLD` and `ALLOW` are retired pre-2.1 verdict synonyms. Current runs never emit
them; the schema validator and the `prview mcp` adapter still tolerate them on
read-back of older packs.

## Blocking rules

Whether a check's `FAIL` blocks the merge depends on its policy severity:

- `shadow`: never blocks.
- `warn`: blocks only `FAIL + block`.
- `block`: blocks `FAIL + (block | warn)`.
