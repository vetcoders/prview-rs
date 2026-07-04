# MCP smoke from an agent

> **`prview mcp` is not a hang.** It starts a JSON-RPC server over **stdio** and
> waits for a client to speak the protocol. Running it in a plain terminal looks
> frozen because it is holding the connection open. For a manual smoke check run
> `prview mcp --probe` first; it starts a child MCP server, performs
> `initialize` → `tools/list` → `health`, and exits within a hard timeout.
> `prview mcp --help` only proves CLI parsing. Do not "wait for it to finish" —
> it finishes when the client
> disconnects.

This page is an operational smoke walkthrough for agents. The tool contract
(arguments, response fields, error classes) lives in [`mcp.md`](mcp.md) — this
document links it rather than restating it.

## Client configuration

First confirm the stdio server answers a real JSON-RPC handshake:

```bash
prview mcp --probe
```

Successful output is intentionally short:

```text
prview mcp probe ok
version: 0.4.0
schema_version: prview.mcp.v1
tools: 6
response_ms: 15
```

For automation use `prview mcp --probe --json`.

Register the server with your MCP client (for example in an `mcp.json`):

```json
{
  "mcpServers": {
    "prview": { "command": "prview", "args": ["mcp"] }
  }
}
```

Set the client's per-call timeout to **≥ 150 s** for `run_review` with the
`quick` profile: the server enforces a hard 120 s budget on the child review,
and the extra headroom covers process startup and I/O without the client giving
up first. `deep` returns immediately (it is async), so it does not need a long
timeout — you poll `verdict` instead.

## The loop

A healthy smoke run is: `health` → `state` → `run_review` → poll `verdict` →
`findings`. Every response carries `"schema_version": "prview.mcp.v1"`; failures
are fail-loud (`"is_error": true` with an `error_class`), never an empty
success.

### 1. `health`

Confirm the server is operational at session start.

```json
// request args
{ "repo": "/abs/path/to/repo" }

// response
{
  "version": "0.4.0",
  "protocol": "prview.mcp.v1",
  "deps_global": { "git": true },
  "deps_repo": {
    "profile": "rust",
    "tools": { "cargo": true, "cargo-clippy": true, "rustfmt": true }
  },
  "schema_version": "prview.mcp.v1"
}
```

### 2. `state`

Cheap snapshot — decide whether a fresh run is even needed. If
`latest_run_for_head` is a completed run for the current HEAD, you can skip
`run_review` and read that run directly.

```json
// request args
{ "repo": "/abs/path/to/repo" }

// response
{
  "branch": "feature/x",
  "commit": "a1b2c3d",
  "dirty": false,
  "files_changed": 4,
  "latest_run_for_head": null,
  "latest_run_any": null,
  "schema_version": "prview.mcp.v1"
}
```

### 3. `run_review` (deep, async)

`deep` spawns a detached review and returns immediately with a `run_id` and
`status: "running"`. **`running` is an intermediate state, not a verdict** — do
not report it as a result.

```json
// request args
{ "repo": "/abs/path/to/repo", "profile": "deep" }

// response
{
  "run_id": "20260704-120500",
  "status": "running",
  "commit": "a1b2c3d",
  "base_used": ["develop", "main", "master"],
  "schema_version": "prview.mcp.v1"
}
```

A second `run_review` while one is in flight returns `storage_locked` with an
`active_run_id` and a `retry_after_ms` — wait and reuse that run rather than
starting another.

### 4. Poll `verdict`

Call `verdict` with the `run_id` until `status` is `completed`. While the run is
alive it reports liveness with a `retry_after_ms` hint — **honor it**: wait that
long before the next poll instead of hammering the server.

```json
// still running
{
  "run_id": "20260704-120500",
  "status": "in_progress",
  "run_status": "running",
  "started_at": "2026-07-04T12:05:00+02:00",
  "elapsed_s": 5,
  "retry_after_ms": 5000,
  "schema_version": "prview.mcp.v1"
}

// completed
{
  "run_id": "20260704-120500",
  "commit": "a1b2c3d",
  "status": "completed",
  "base_used": ["main"],
  "verdict": "PASS",
  "merge_recommendation": "approve",
  "allow_merge": true,
  "blocking_issues": [],
  "caveats": [],
  "gates": [
    { "id": "clippy", "status": "PASS", "reason": null, "evidence": null }
  ],
  "generated_at": "2026-07-04T12:05:31+02:00",
  "schema_version": "prview.mcp.v1"
}
```

A `stale` status means the review process died before finalizing; `failed` means
it produced no completed pack. Neither is a verdict — surface the failure, do not
invent one.

### 5. `findings`

Once `verdict.status` is `completed`, pull the structured findings (paged).
Requesting findings before completion is a fail-loud `stale_run`.

```json
// request args
{ "repo": "/abs/path/to/repo", "run_id": "20260704-120500", "severity": "error" }

// response
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

## Reporting rule for agents

Whenever you report a review to a human or another agent, always include:

- the **`run_id`** (so the run is addressable),
- the **`base_used`** (so it is clear what the branch was diffed against — prview
  falls back through `develop` → `main` → `master`, and the base materially
  changes the result),
- whether the result is **final** (`verdict.status == "completed"`) or still
  in progress. Never present `running`, `stale`, or `failed` as a verdict.

For error classes, retry semantics, and the full argument surface of each tool,
see [`mcp.md`](mcp.md).
