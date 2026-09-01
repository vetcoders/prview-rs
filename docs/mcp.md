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
  "version": "<prview version>",
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

MCP `run_review` is supported on Linux, macOS, and Windows. Its durable
`RUNNING.json` marker must bind a PID to a native process-birth identity, so on
other source-buildable targets the tool returns `run_failed` before taking the
branch activation lock or spawning prview. The CLI remains the direct review
surface there; source-build availability does not manufacture a PID-reuse-safe
MCP lifecycle contract.

| Arg | Required | Description |
|-----|----------|-------------|
| `repo` | yes | Absolute path to the git repo to review. |
| `base` | no | Base ref to diff against. Default: detected repo default branch (`origin/HEAD`, then `remote.origin.HEAD`); if detection fails, falls back to existing `develop` / `main` / `master` candidates and marks `base_fallback: true`. |
| `profile` | no | `"quick"` (default) or `"deep"`. An unknown value is a fail-loud `run_failed`. |

**`quick` is synchronous.** It blocks until the pack is written, under a hard
**120-second budget**. Exceeding the budget runs bounded whole-tree containment
and returns `run_timeout` with `retry_hint.profile: "deep"` plus
`containment_confirmed`. Success is defined
by a finalized pack plus its exact durable run-id/path index row, not by the
child's exit code (prview exits non-zero on a `BLOCK` verdict, yet the run is a
valid completed review). Response:

If the synchronous child wait itself fails, the server routes it through the
same bounded containment and direct-root reap before returning `run_failed`.
On Unix, timeout and wait-error cleanup first sends Ctrl-C to let the review's
governor drain its exact child registry. A private parent-owned sidecar mirrors
each separately-grouped tool PID together with its native process-birth
identity, so the MCP server can still terminate the exact owned groups before
killing the root when cooperative unwind does not complete. To close the window
before that full registration, each fork writes a provisional PGID in
`pre_exec`; the inherited mode-0600 ledger remains locked until the review root
and every in-flight pre-exec copy have closed it. The descriptor stays CLOEXEC
in the multi-threaded MCP parent, is made inheritable only in the already-forked
review root, and is restored to CLOEXEC before repository discovery or startup
helpers. For hard fallback, the parent sends `SIGSTOP`,
accepts a local process-table census only after the same snapshot reports the
root stopped, and signals only direct hardened child groups or committed identities.
A provisional PID is never signal authority by itself. The parent then kills
and reaps the root, acquires the ledger lock, and accepts the final drain. The
sidecar lives beside, never inside, the immutable run directory. Confirmed
cleanup removes it; unconfirmed cleanup retains it and returns
`containment_confirmed: false` instead of claiming success. Hardened tool
children receive neither capability env nor ledger descriptor after exec;
their descendants are already contained by the one tool group. This is
required because one Unix process group cannot contain another. Windows uses
recursive process-tree termination. The run's `RUNNING.json` remains as
diagnostic `Stale` state after the root is reaped and does not block a later
review.

Completion requires durable publication into prview's run index. If the index
is unreadable or the finished pack cannot be committed to discoverable history,
the call returns fail-loud `storage_corrupt` or `run_failed`; SANITY without the
matching durable row is never exposed as completed. Synchronous launch fails
loud; later readers report failed or stale according to the surviving control
marker instead of fabricating completion from SANITY alone.

**`deep` is asynchronous but still process-owned.** The MCP server keeps the
spawned child handle in a dedicated waiter until the direct process is reaped;
an immediately failing review therefore cannot remain as a zombie or block the
next review. On Unix, normal root exit also terminates residual members of the
root's dedicated process group; Windows retains the complete Job Object. The
`running` response is returned only after `RUNNING.json` has
been written and that reaper has started. If process-identity capture, marker
publication, or reaper setup fails, prview terminates the spawned process tree,
reaps its direct root, and returns `run_failed`.

```json
{
  "run_id": "20260701-120000-a1b2c3d",
  "status": "completed",
  "commit": "a1b2c3d",
  "base_used": ["main"],
  "base_fallback": false,
  "verdict": "PASS",
  "enforcement_disposition": "clean",
  "merge_recommendation": "approve",
  "allow_merge": true,
  "blocking_issues": [],
  "caveats": [],
  "gates": [
    {
      "id": "cargo_test",
      "name": "Cargo test",
      "status": "passed",
      "execution_state": "executed",
      "outcome": "passed",
      "class": "PASS",
      "severity": "block",
      "policy_conclusion": "satisfied",
      "blocking": false,
      "merge_impact": "approve",
      "confidence_impact": "complete",
      "duration_secs": 12.4,
      "cached": false,
      "reason": null,
      "evidence": "20_quality/cargo_test.result.json",
      "log": "20_quality/cargo_test.log"
    }
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

Detached does not mean unowned. The MCP parent retains the root child and a
parent-owned child-group ledger; its reaper drains separately hardened nested
tool groups after the root exits and removes the running marker only after both
complete publication and confirmed process-tree containment.

Poll [`verdict`](#verdict) with that `run_id` until `status` is `completed`.

**One active run per repo branch.** A second `run_review` while one is in flight
returns retryable `storage_locked` with an `active_run_id` and a
`retry_after_ms` hint. A stale pre-0.8-compatible `.active.lock` is different:
automatic takeover could race a paused legacy contender, so the response is
non-retryable `storage_locked` with `recovery_required: true`, the exact
`lock_path`, and a recovery instruction. Remove only that path, and only after
verifying that no pre-0.8 prview process is using the same storage root.
Permission, link, and malformed-lock failures return non-retryable
`storage_corrupt` instead of pretending that another review is live.

The launcher exclusively reserves the future pack directory before spawning so
`RUNNING.json` and the two process logs are observable immediately. The child
can adopt that directory only once with the matching private nonce and only
while it contains the reservation plus those three control files. This is not
an exception to the public `--output-dir` contract: ordinary CLI calls still
require a nonexistent path. `RUNNING.json`, `run.log`, and `run.stderr.log`
remain readable beside the live pack but are mutable MCP control state, so they
are deliberately excluded from the immutable `MANIFEST.json` and
`artifacts.zip` payload.

### `verdict`

The single decision truth for a run. Default: the latest run for the current
`repo` HEAD. For a `deep` run, poll this until `status` is `completed`.

| Arg | Required | Description |
|-----|----------|-------------|
| `repo` | yes | Absolute path to the git repo. |
| `run_id` | no | Opaque repo-global run id. Default: latest run for HEAD. |

The decision surface is normalized so callers read one vocabulary:

- `verdict` — `PASS`, `CONDITIONAL`, or `BLOCK`.
- `enforcement_disposition` — `clean`, `warnings_only`, `review_required`, or
  `block`. This is an orthogonal process-policy axis, not another verdict rank:
  a warning-bearing pack may remain `PASS`/`allow_merge: true` while reporting
  `warnings_only`.
- `merge_recommendation` — `approve`, `review_required`, or `block`.
- `allow_merge` — boolean, **derived** conservatively: it is `true` only for a
  clean `PASS`. A permissive flag on disk can never override a block/hold signal.

If the stored gate emits contradictory signals (for example `allow_merge: true`
alongside a block recommendation, or a clean approval alongside
`quality_pass: false` — the contract permits `PASS` only when quality passes),
the most conservative signal wins and a `core_inconsistency` note is appended to
`caveats`. The note reports a
disagreement the pack actually states — the textual axes against the published
verdict, `allow_merge` against the flag published — so a self-consistent
`BLOCK` pack (`verdict: "BLOCK"`, `merge_recommendation: "block"`,
`allow_merge: false`) raises no caveat at all. The CLI `--json` surface
reconciles the same way through the same ranking
(`gate::rank_from_verdict` / `gate::rank_from_merge_rec`), so the two surfaces
cannot disagree about a contradictory pack. Legacy gate tokens (`ALLOW`,
`APPROVE`, `HOLD`) written by older cores are still recognized on read and folded
into the `PASS` / `CONDITIONAL` surface rather than failing loud. That fold is
`gate::canonical_verdict`, shared by this adapter, the CLI summary and
`prview gate`, and it ignores case: a stored `"pass"` reads as `PASS` on every
surface instead of approving on one and normalizing to `BLOCK` on another.

Schema 2.3 makes the disposition and its typed proof mandatory. The shared
CLI/MCP reader cross-checks check execution/outcome/confidence/merge axes,
effective inline class/disposition, policy blocking, and typed quality-failure
provenance. Only literal warning proof can preserve `warnings_only`; missing,
unknown, contradictory, or amputated additive proof raises enforcement to
`review_required` with a caveat without rewriting an otherwise readable
canonical `PASS`/`CONDITIONAL`/`BLOCK`. Packs through schema 2.2 cannot unlock
the warning exception, even if an unknown writer injected the new field.

Anything the adapter could not read is named rather than dropped, and every such
case sets `normalized: true`:

- `unknown_verdict:` / `unknown_merge_recommendation:` — the field was present
  but outside the known vocabulary, so it was ignored when deriving the decision.
  A verdict that could not be ranked — outside the vocabulary, or simply absent
  while another signal is stated — is substituted with `BLOCK`, and that
  substitution governs the axes published beside it: `merge_recommendation`
  reads `block` and `allow_merge` `false`, whatever the pack claimed. This is
  the CLI's rule, applied here so the two readers cannot answer the same bytes
  differently. `storage_corrupt` is reserved for a decision block stating NONE
  of `verdict`, `merge_recommendation` and `allow_merge`; a signal that is
  present but unrankable — including a lone `allow_merge` — is a decision the
  pack gave, and it is normalized with a caveat rather than called corrupt.
- `unreadable_<field>:` — the field was present with the wrong JSON type
  (`merge_recommendation: 7`, `allow_merge: "false"`, `quality_pass: "false"`,
  `analysis_status: 7`, `blocking_issues: "Clippy"`). Emitted for every axis in
  the ranking table of `docs/contracts/merge_gate.md`. A wrongly typed field is
  not an absent one: it is ignored for ranking, but it is named, and the
  decision is normalized conservatively around it. The pack is
  `storage_corrupt` only when no signal was stated at all.
- `unknown_analysis_status:` — the field is a string outside
  `complete` / `degraded` / `incomplete`. Like `unknown_merge_recommendation:`,
  it cannot rank, so it is excluded from the reconciliation and named rather
  than dropped in silence.
- `schema_forward_compat:` — the pack's `schema_version` is a newer MINOR of a
  known MAJOR; it is read, and fields this build does not know are ignored. An
  unknown MAJOR is `storage_corrupt`, and so is a `schema_version` that is
  present but not a `MAJOR.MINOR` string. A pack with no `schema_version` at all
  is pre-2.1 and is accepted silently, like the `ALLOW`/`HOLD` tokens — including
  the pre-2.1 shape that carries its signals at the root instead of under
  `decision`. A pack that STATES a `schema_version` and still has no `decision`
  object is `storage_corrupt`, and so is a schema-less pack whose root is not an
  object at all (an array, a scalar, `null`) — that root states no decision, it
  is not a decision missing every field. Both readers apply those rules from one
  place (`gate::select_decision_object`), so a pack the CLI reads is never one
  the MCP adapter calls corrupt.

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
  "enforcement_disposition": "clean",
  "blocking_issues": [],
  "caveats": [],
  "gates": [
    {
      "id": "clippy",
      "name": "Clippy",
      "status": "passed",
      "execution_state": "executed",
      "outcome": "passed",
      "class": "PASS",
      "severity": "block",
      "policy_conclusion": "satisfied",
      "blocking": false,
      "merge_impact": "approve",
      "confidence_impact": "complete",
      "duration_secs": 4.2,
      "cached": false,
      "reason": null,
      "evidence": "20_quality/clippy.result.json",
      "log": "20_quality/clippy.log"
    }
  ],
  "generated_at": "2026-07-01T12:00:03+02:00",
  "schema_version": "prview.mcp.v1"
}
```

While a `deep` run is in flight or after it dies, `verdict` reports liveness via
the run's versioned `RUNNING.json` marker instead of a decision. Marker v2 binds
the PID to a native process-birth identity (Linux boot UUID plus start ticks,
macOS process start time, or Windows creation FILETIME), so PID recycling cannot
make a v2 run appear live. Those are the supported MCP `run_review` targets; a
different target is rejected before spawn. A PID-only legacy marker with a live
PID deliberately blocks as `running` until that PID exits; without a birth token,
preserving the one-active-run invariant is safer than starting a second heavy
review. A legacy marker becomes `stale` once its PID is dead:

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
| `run_timeout` | A `quick` review exceeded the 120s budget. Carries `run_id`, `containment_confirmed`, and `retry_hint.profile: "deep"`. |
| `run_not_found` | No run matches the given `run_id` / HEAD; call `run_review`. |
| `artifact_missing` | The requested artifact does not exist within the run, is not UTF-8 text, or would escape the run directory. |
| `tool_missing` | A required external tool is unavailable. |
| `storage_locked` | Another review is already running for this repo branch (`retryable: true`, `active_run_id`, `retry_after_ms`), or a stale legacy-compatible activation sentinel requires explicit recovery (`retryable: false`, `recovery_required: true`, `lock_path`). |
| `storage_corrupt` | `MERGE_GATE.json` is missing, invalid, carries a `schema_version` with an unknown MAJOR, states no decision signal at all; an explicit `run_id` is ambiguous; or the branch activation lock cannot be opened safely. Activation-lock failures carry `retryable: false` and `lock_path`. |
| `stale_run` | The run is still in progress or its process died before completing. Carries `retry_after_ms` while running. |

### Retrying

When a response carries `retry_after_ms`, wait that long before retrying the same
call. It appears on `storage_locked` (another run holds the branch), on an
`in_progress` `verdict`, and on a `stale_run` while a review is still in progress.
Never loop on `retryable: false`; follow its recovery detail or repair the named
storage path condition first.
