# prview MCP server

`prview` ships a native [Model Context Protocol](https://modelcontextprotocol.io)
server so an agent can run reviews and consume the verdict and artifacts through
tools, instead of driving the CLI and parsing files by hand. The server speaks
JSON-RPC over **stdio**.

## Running

```bash
prview mcp
```

The process serves the protocol over stdio until the client disconnects. There
are no flags or environment variables to set — every tool call carries the repo
it operates on.

Canonical client entry (for example in an `mcp.json`):

```json
{
  "mcpServers": {
    "prview": { "command": "prview", "args": ["mcp"] }
  }
}
```

## Model

A few invariants hold across every tool:

- **Stateless and repo-explicit.** Every tool takes an absolute `repo` path and
  reads truth from prview storage (`~/.prview/`, or `$PRVIEW_HOME`). The server
  never depends on its own working directory and can be started from anywhere.
- **Versioned.** Every response carries `schema_version: "prview.mcp.v1"`.
- **Fail-loud.** A failure is a structured error result (`is_error: true`) with
  an `error_class` and a `message`, never an empty success. See
  [Error classes](#error-classes).
- **Opaque run ids.** `run_id` is globally unique within a repo storage tree and
  encodes the run's commit identity in new runs (for example
  `YYYYMMDD-HHMMSS-<short_sha>`). Clients must treat it as an opaque token; older
  timestamp-only ids remain readable.

A typical agent loop is: `health` at session start → `state` to decide whether a
fresh run is needed → `run_review` → `verdict` / `findings` / `read_artifact` to
consume the result.

## Tools

### `health`

Confirm prview is operational. Call once at session start.

| Arg | Required | Description |
|-----|----------|-------------|
| `repo` | no | Absolute path to a git repo. When present, per-repo profile and tool availability are included; omit for a global-only probe. |

Response:

```json
{
  "version": "0.3.1",
  "protocol": "prview.mcp.v1",
  "deps_global": { "git": true },
  "deps_repo": {
    "profile": "rust",
    "tools": { "cargo": true, "cargo-clippy": true, "rustfmt": true }
  },
  "schema_version": "prview.mcp.v1"
}
```

`deps_repo` is `null` when no `repo` is given, or when the path exists but its
profile could not be detected (honest null, never a fabricated profile). The
`tools` map is keyed by the external binaries relevant to the detected profile.

### `state`

Cheap repo snapshot — use it before deciding whether a fresh `run_review` is
needed.

| Arg | Required | Description |
|-----|----------|-------------|
| `repo` | yes | Absolute path to the git repo to inspect. |

Response:

```json
{
  "branch": "feature/x",
  "commit": "a1b2c3d",
  "default_branch": "main",
  "base_fallback": false,
  "base_caveats": [],
  "dirty": false,
  "files_changed": 4,
  "latest_run_for_head": {
    "run_id": "20260701-120000-a1b2c3d",
    "commit": "a1b2c3d",
    "status": "completed",
    "profile": null,
    "base_used": ["main"],
    "merge_status": "PASS",
    "generated_at": "2026-07-01T12:00:03+02:00"
  },
  "latest_run_any": { "...": "same shape, latest run on this branch key" },
  "schema_version": "prview.mcp.v1"
}
```

Either run summary is `null` when no matching run exists. `profile` is reported
as `null` because quick/deep is not persisted in the run index. If a run is
currently active for HEAD, `latest_run_for_head` reports `status: "in_progress"`
with factual `started_at` and `elapsed_s` fields; no ETA is fabricated.

`default_branch` is the same base-selection path used by `run_review` when no
explicit `base` is provided. `base_fallback: true` means prview could not detect
the repo default branch and fell back to existing `develop` / `main` / `master`
candidates; `base_caveats` explains that fallback.

### `run_review`

Generate a review pack.

| Arg | Required | Description |
|-----|----------|-------------|
| `repo` | yes | Absolute path to the git repo to review. |
| `base` | no | Base ref to diff against. Default: detected repo default branch (`origin/HEAD`, then `remote.origin.HEAD`); if detection fails, falls back to existing `develop` / `main` / `master` candidates and marks `base_fallback: true`. |
| `profile` | no | `"quick"` (default) or `"deep"`. An unknown value is a fail-loud `run_failed`. |

**`quick` is synchronous.** It blocks until the pack is written, under a hard
**120-second budget**. Exceeding the budget kills the whole review process tree
and returns `run_timeout` with `retry_hint.profile: "deep"`. Success is defined by a finalized pack on disk, not by
the child's exit code (prview exits non-zero on a `BLOCK` verdict, yet the run is
a valid completed review). Response:

```json
{
  "run_id": "20260701-120000-a1b2c3d",
  "status": "completed",
  "commit": "a1b2c3d",
  "base_used": ["main"],
  "base_fallback": false,
  "verdict": "PASS",
  "merge_recommendation": "approve",
  "allow_merge": true,
  "blocking_issues": [],
  "caveats": [],
  "gates": [
    { "id": "cargo_test", "status": "PASS", "reason": null, "evidence": null }
  ],
  "artifact_paths": {
    "pack": "/Users/tester/.prview/runs/<repo>/<branch>/20260701-120000-a1b2c3d",
    "merge_gate": "00_summary/MERGE_GATE.json",
    "sarif": "30_context/INLINE_FINDINGS.sarif",
    "report": "report.json"
  },
  "stats": { "checks_passed": 6, "checks_failed": 0, "files_changed": 4 },
  "schema_version": "prview.mcp.v1"
}
```

`artifact_paths.sarif` and `artifact_paths.report` appear only when those files
exist for the run; `pack` is absolute, the rest are pack-relative.

**`deep` is asynchronous.** It spawns a detached review and returns immediately:

```json
{
  "run_id": "20260701-120500-a1b2c3d",
  "status": "running",
  "commit": "a1b2c3d",
  "base_used": ["main"],
  "base_fallback": false,
  "caveats": [],
  "schema_version": "prview.mcp.v1"
}
```

Poll [`verdict`](#verdict) with that `run_id` until `status` is `completed`.

**One active run per repo branch.** A second `run_review` while one is in flight
returns `storage_locked` with an `active_run_id` and a `retry_after_ms` hint.

### `verdict`

The single decision truth for a run. Default: the latest run for the current
`repo` HEAD. For a `deep` run, poll this until `status` is `completed`.

| Arg | Required | Description |
|-----|----------|-------------|
| `repo` | yes | Absolute path to the git repo. |
| `run_id` | no | Opaque repo-global run id. Default: latest run for HEAD. |

The decision surface is normalized so callers read one vocabulary:

- `verdict` — `PASS`, `CONDITIONAL`, or `BLOCK`.
- `merge_recommendation` — `approve`, `review_required`, or `block`.
- `allow_merge` — boolean, **derived** conservatively: it is `true` only for a
  clean `PASS`. A permissive flag on disk can never override a block/hold signal.

If the stored gate emits contradictory signals (for example `allow_merge: true`
alongside a block recommendation), the most conservative signal wins and a
`core_inconsistency` note is appended to `caveats`. Legacy gate tokens (`ALLOW`,
`HOLD`) written by older cores are still recognized on read and folded into the
`PASS` / `CONDITIONAL` surface rather than failing loud.

Completed response:

```json
{
  "run_id": "20260701-120000-a1b2c3d",
  "commit": "a1b2c3d",
  "status": "completed",
  "base_used": ["main"],
  "merge_recommendation": "approve",
  "allow_merge": true,
  "verdict": "PASS",
  "blocking_issues": [],
  "caveats": [],
  "gates": [
    { "id": "clippy", "status": "PASS", "reason": null, "evidence": null }
  ],
  "generated_at": "2026-07-01T12:00:03+02:00",
  "schema_version": "prview.mcp.v1"
}
```

While a `deep` run is in flight or after it dies, `verdict` reports liveness via
the run's `RUNNING.json` marker instead of a decision:

| `status` | Meaning | Extra fields |
|----------|---------|--------------|
| `in_progress` | The review process is alive. | `run_status: "running"`, `started_at`, `elapsed_s`, `retry_after_ms: 5000` |
| `stale` | The marker's process died before finalizing. | `started_at` |
| `failed` | The run produced no completed pack. | `base_used: []` |

`elapsed_s` is computed from the marker timestamp at response time. prview does
not invent ETA values; callers should poll `verdict(run_id)` until
`status: "completed"`.

### `findings`

Paged structured findings for a completed run, lifted from the run's inline
SARIF. Prefer this over `read_artifact` for findings. A run with no SARIF file is
an honest empty set, not an error.

| Arg | Required | Description |
|-----|----------|-------------|
| `repo` | yes | Absolute path to the git repo. |
| `run_id` | no | Opaque repo-global run id. Default: latest run for HEAD. |
| `severity` | no | Filter to a single SARIF level (e.g. `error`, `warning`, `note`), case-insensitive. |
| `path` | no | Keep only findings whose file path starts with this prefix. |
| `cursor` | no | Opaque pagination cursor from a previous call's `next_cursor`. |
| `limit` | no | Max items this page. Default `100`, clamped to `1..1000`. |

Response:

```json
{
  "items": [
    {
      "file": "src/auth/session.rs",
      "line": 42,
      "severity": "error",
      "rule": "clippy::unwrap_used",
      "message": "used `unwrap()` on a `Result` value",
      "artifact_ref": "30_context/INLINE_FINDINGS.sarif"
    }
  ],
  "total": 1,
  "next_cursor": null,
  "schema_version": "prview.mcp.v1"
}
```

Findings are ordered deterministically by `(file, line, rule)`. When more results
remain, `next_cursor` is a string; pass it back as `cursor` for the next page.
Requesting findings for a run that is not yet completed is a fail-loud
`stale_run` (with `retry_after_ms` while running). The message points callers at
`verdict(run_id)` as the polling target.

### `read_artifact`

Raw artifact body, paged by line. Use only when the `findings` / `verdict`
summaries are not enough.

| Arg | Required | Description |
|-----|----------|-------------|
| `repo` | yes | Absolute path to the git repo. |
| `run_id` | yes | Opaque repo-global run id that owns the artifact. |
| `artifact` | yes | Pack-relative artifact path (e.g. `00_summary/MERGE_GATE.json`). |
| `cursor` | no | Opaque pagination cursor from a previous call's `next_cursor`. |
| `limit` | no | Max lines this page. Default `200`, clamped to `1..5000`. |

Response:

```json
{
  "content": "…joined lines…",
  "total_lines": 128,
  "next_cursor": "200",
  "schema_version": "prview.mcp.v1"
}
```

The `artifact` path is resolved **inside the run directory** and validated
against escape even through symlinks. Any path that would leave the run
directory, or a missing/non-UTF-8 file, collapses to `artifact_missing` — the
server never reveals what exists outside the run. The two run logs
(`run.log`, `run.stderr.log`) are always readable so a failed or stale run can
expose its post-mortem; every other artifact requires a completed pack.
Requesting a non-log artifact while a run is still active returns `stale_run`
with a message pointing callers at `verdict(run_id)`.

## Error classes

Every failure is an error result whose body is
`{ "error_class", "message", "schema_version", … }`. Some classes carry extra
fields (e.g. `retry_after_ms`, `active_run_id`, `run_id`).

| `error_class` | When |
|---------------|------|
| `repo_not_found` | The `repo` path does not exist. |
| `not_a_git_repo` | The path exists but is not a readable git repository. |
| `run_failed` | The review process failed to produce a completed pack (or an unknown `profile` was requested). |
| `run_timeout` | A `quick` review exceeded the 120s budget. Carries `run_id` and `retry_hint.profile: "deep"`. |
| `run_not_found` | No run matches the given `run_id` / HEAD; call `run_review`. |
| `artifact_missing` | The requested artifact does not exist within the run, is not UTF-8 text, or would escape the run directory. |
| `tool_missing` | A required external tool is unavailable. |
| `storage_locked` | Another review is already running for this repo branch. Carries `active_run_id` and `retry_after_ms`. |
| `storage_corrupt` | `MERGE_GATE.json` is missing, invalid, has no recognizable decision, or an explicit `run_id` is ambiguous in storage. |
| `stale_run` | The run is still in progress or its process died before completing. Carries `retry_after_ms` while running. |

### Retrying

When a response carries `retry_after_ms`, wait that long before retrying the same
call. It appears on `storage_locked` (another run holds the branch), on an
`in_progress` `verdict`, and on a `stale_run` while a review is still in progress.
