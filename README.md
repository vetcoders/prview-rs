<p align="center">
  <img src="assets/branding/readme-banner.png" alt="prview — surfaces the signal before merge" width="760">
</p>

<p align="center">
  <em>surfaces the signal before merge</em>
</p>

<p align="center">
  <code>pull request</code> · <code>rust cli</code> · <code>diff intelligence</code>
</p>

<p align="center">
  <a href="https://github.com/vetcoders/prview-rs/actions/workflows/ci.yml"><img src="https://github.com/vetcoders/prview-rs/actions/workflows/ci.yml/badge.svg" alt="CI"></a>
  <a href="https://crates.io/crates/prview"><img src="https://img.shields.io/crates/v/prview.svg" alt="crates.io"></a>
  <a href="LICENSE"><img src="https://img.shields.io/badge/License-BUSL--1.1-blue.svg" alt="License: BUSL-1.1"></a>
</p>

---

**prview** reads a pull request the way a good reviewer does: it separates signal from noise. It compares a branch against one or more bases, runs language-aware checks, computes structural heuristics, and emits both human- and machine-readable review packs — so you see the risk before you merge, not after.

No dashboards to babysit. No "powerful insights." Just the things that would block the merge, surfaced early.

```text
1 failed check · test changes matched for 3/4 source files · 1 public API change
```

## Why prview

- **One human review entry point** — `dashboard.html` guides you through changes, check results, failure evidence and provenance, with an offline reader for the underlying artifacts.
- **Signal, not noise** — high-signal review pack: `PR_REVIEW.md`, compact failure summaries, source-to-test matching, breaking changes.
- **Merge decision support** — policy-aware `MERGE_GATE.json/.md` and optional per-finding `INLINE_FINDINGS.sarif`.
- **Multi-language** — JavaScript/TypeScript, Rust, Python, or mixed repos.
- **Fast** — native Rust binary, parallel checks, `git2` for git operations.
- **Structural heuristics** — Loctree candidates with files, symbols and source locations across Rust/JS/TS/Python; inspect the evidence before concluding that code is unused or duplicated.
- **Made for agents** — a compact `AI_INDEX.md` entry point plus a native MCP server.
- **Shell completions** — bash, zsh, fish, elvish, powershell.

## Install

Quickest — download the latest checksum-verified release binary into `~/.local/bin` (no sudo):

```bash
curl -fsSL https://raw.githubusercontent.com/vetcoders/prview-rs/main/install.sh | sh
```

The installer is fail-closed: it installs an official binary whose checksum, macOS signature and notarization, and build provenance all verify — or it installs nothing and exits non-zero. It has no source-build fallback and never runs `cargo` on your machine.

From crates.io:

```bash
cargo install prview --locked --force
```

`--force` overwrites any older `prview` in place, so upgrades are seamless; it is harmless on a clean machine.

From a local checkout (contributors / maintainers):

```bash
make install        # binary + local git hooks (fast guards, no compilation)
make install-bin    # binary only
```

Full instructions — release binaries, checksums, and PATH setup — live in [`docs/INSTALL.md`](docs/INSTALL.md).

## Quick start

```bash
# Fast local review of the current branch vs the default base
prview --quick

# Review a GitHub PR with stricter presets
prview --pr 23 --deep

# Run the automation gate with contractual exit codes
prview gate

# Open the latest generated dashboard
prview open
```

Every run writes an artifact pack:

- `AI_INDEX.md` — entry point for humans and agents
- `PR_REVIEW.md` — the unified review narrative
- `report.json` — machine-readable output
- `dashboard.html` — the default human report and offline evidence reader
- `00_summary/MERGE_GATE.json` — gate automation
- `00_summary/PROVENANCE.json` — what the pack judged, and which tree each check read

Open `dashboard.html` from the extracted pack to follow the review without
searching folders for logs or JSON. The reader includes the most useful text
artifacts; larger or binary files remain available as original downloads.
`--no-dashboard` generates a simpler `review.html` instead. A run produces one
of these HTML entry points, and `AI_INDEX.md` points to the selected one.

## Usage

```bash
# Auto-detect profile, diff current branch vs the default base
prview

# Full analysis with stricter presets
prview --deep feature/x main

# Opt into higher throughput with capped child pools (safe is the default)
prview --deep --resource-budget balanced feature/x main

# Incremental update after new commits
prview --update feature/x main

# Python project
prview --profile python --with-tests --with-lint

# Compact JSON for CI / agents (stdout = JSON only)
prview --pr 23 --quick --json --quiet

# Gate JSON for automation
prview gate --json

# Interactive TUI for browsing results
prview --tui
```

The full flag reference is always one command away: `prview --help`. A written guide lives in [`docs/usage.md`](docs/usage.md).

Deep reviews use a conservative whole-machine resource contract by default:
one expensive tool and one supported child worker at a time. The preflight names
the effective budget, expensive checks, and schedule; `balanced` remains capped
and falls back to `safe` under load. Because `safe` admits one check at a time,
a big repository's check stage is serialized by design — the progress line
reports each running check's own elapsed time and says that the rest are
waiting on run resources. See
[`docs/usage.md`](docs/usage.md#resource-budget) for the full contract.

On Unix, cancellation and timeout cleanup also follows live PPID ancestry when
a tool descendant leaves its inherited process group with `setsid` or
`setpgid`; Windows uses Job Object ownership. An already-reparented Unix
double-fork is outside the portable containment guarantee.

In TUI raw mode, `q`, Escape, or the first Ctrl-C follows the TUI's cooperative
quit path and returns normally after cleanup. If an in-process Git analysis
stage is still unwinding, a second Ctrl-C event is the immediate escape hatch:
the terminal is restored before prview exits 130.

## Quality gate

`prview gate` runs the standard fast gate profile, reads the verdict from the
generated merge-gate artifact, and exits with the automation contract:

| Exit code | Meaning |
|-----------|---------|
| `0` | `PASS`, advisory `CONDITIONAL`, or a typed warnings-only decision under `--strict` |
| `1` | `BLOCK` |
| `2` | Review-required under `--strict`, or warnings-only with `--strict --fail-on-warnings` |
| `3` | Gate execution failed before a trustworthy verdict was available |
| `130` | A headless/preflight Ctrl-C or second raw-mode TUI Ctrl-C event forced cancellation; the CLI reports no new verdict, while any pack already durably committed remains discoverable |

Use `prview gate --json` for schema-friendly stdout with the verdict, caveats,
blocking issues, typed `enforcement_disposition`, and artifact paths. A strict
gate accepts `clean` and `warnings_only`; confirmed/potential breaking
changes, degraded or unknown analysis, quality failures, and hard blocks remain
non-zero. Add `--fail-on-warnings` when a Required check must also enforce a
warnings-clean pack.

For local pre-push recipes and the recommended Shadow -> Warn -> Block rollout,
see [`docs/gate-playbook.md`](docs/gate-playbook.md).

### GitHub Action

External repositories can run the gate with one composite Action step:

```yaml
permissions:
  contents: read
  security-events: write

jobs:
  prview:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
        with:
          fetch-depth: 0
      - uses: vetcoders/prview-rs@v0.8.0 # current published Action
        id: prview
        with:
          strict: "true"
          version: "0.8.0"
      - uses: github/codeql-action/upload-sarif@v3
        if: ${{ steps.prview.outputs['sarif-path'] != '' }}
        with:
          sarif_file: ${{ steps.prview.outputs['sarif-path'] }}
```

The Action maps pass/fail only from the `prview gate` exit-code contract. JSON
stdout is used for step-summary details and artifact paths, not for deciding
whether the check passed. `cargo-binstall` is used when available, with
`cargo install prview` as the fallback. This copy-pasteable example uses the
currently published `v0.8.0` Action/runtime, which carries the typed
warnings-only contract and the Action's `fail-on-warnings` input. Set
`fail-on-warnings: "true"` alongside `strict: "true"` to require a
warning-clean pack as well.

On `push` events the gate auto-detects the base from `develop`, `main`, and
`master`, so a push to the default branch ends up comparing that branch with
itself and reviews an empty change unless you pass the pre-push commit with
`--base`. Add `--exact-base` there: a base is otherwise normalized to its
merge-base with the target, which is the pre-push commit itself on an ordinary
push but widens the review past what a force-push delivered. Both flags need a
prview runtime newer than `0.8.0` — set the Action's `version` input (or
whatever release you install) accordingly; the Action ref itself can stay
pinned, since it just forwards `args`. See
[`docs/gate-playbook.md#choosing-the-base`](docs/gate-playbook.md#choosing-the-base).

GitHub code scanning accepts SARIF uploads through
`github/codeql-action/upload-sarif`. Keep SARIF under GitHub's ingestion limits:
10 MB gzip-compressed upload size and 50 displayed annotations per workflow
step.

## The review pack

| File | What it's for |
|------|---------------|
| `AI_INDEX.md` | Compact entry point for human/agent review |
| `PR_REVIEW.md` | Unified review narrative |
| `report.json` | Machine-readable findings (schema 3.0; canonical gate status and nullable breaking report path) |
| `dashboard.html` | Default human report: guided review and an offline evidence reader |
| `review.html` | Static review export, generated only with `--no-dashboard` |
| `00_summary/MERGE_GATE.json` | Pass/fail gate for automation |
| `00_summary/PROVENANCE.json` | Schema 2.0: reviewed commits, separate operator checkout state, the tree each check scanned, and any `PROVENANCE_CONTRADICTION` between them |
| `20_quality/SNAPSHOT_INTEGRITY.json/.md` | Tracked changes or unverifiable state in the shared review snapshot; requires review (optional) |
| `20_quality/PUBLIC_API_DIFF.json` | Additive API contract: compatibility rows plus lossless repo-backed Rust delta |
| `20_quality/BREAKING_CHANGES.json` | Lossless repo-backed Rust breaking/API delta (optional) |
| `20_quality/BREAKING_CHANGES.md` | Human Rust API truth plus bounded JS/TS and env signals (optional) |
| `30_context/INLINE_FINDINGS.sarif` | Tool observations with supported source locations (optional) |

The merge decision is a single enum — `PASS`, `CONDITIONAL`, or `BLOCK` — so both humans and automation read one truth. See [`docs/contracts/merge_gate.md`](docs/contracts/merge_gate.md).

## MCP server

Agents don't have to drive the CLI and parse files. prview ships a native MCP (Model Context Protocol) server so an agent can run a review and consume the verdict and artifacts through tools. The server speaks JSON-RPC over stdio:

MCP `run_review` is supported on Linux, macOS, and Windows, where prview can bind
its durable running marker to a native PID-reuse-safe process identity. Other
source-buildable targets can use the CLI directly; see
[`docs/mcp.md`](docs/mcp.md#run_review) for the lifecycle boundary.

```bash
prview mcp
```

Canonical client entry (e.g. in an `mcp.json`):

```json
{
  "mcpServers": {
    "prview": { "command": "prview", "args": ["mcp"] }
  }
}
```

Six tools cover the loop end to end:

| Tool | Purpose |
|------|---------|
| `health` | Confirm prview is operational; report version, git, and per-repo tool availability. |
| `state` | Cheap repo snapshot: branch, HEAD, dirty, files changed, latest run for HEAD. |
| `run_review` | Generate a review pack (`quick` synchronous, `deep` detached — poll `verdict`). |
| `verdict` | Single decision truth for a run: `PASS`/`CONDITIONAL`/`BLOCK`, blocking issues, caveats, per-gate status. |
| `findings` | Paged structured findings, filterable by severity and path. |
| `read_artifact` | Raw artifact body, paged and guarded to stay inside the run directory. |

Every tool takes an explicit absolute `repo` path and reads truth from storage, so the server never depends on its own working directory. Every response carries `schema_version: "prview.mcp.v1"`, and failures are fail-loud — a structured `error_class`, never an empty success. Full reference: [`docs/mcp.md`](docs/mcp.md).

Use `prview mcp --probe` as the first manual smoke check; it performs a real MCP handshake and exits instead of leaving the stdio server waiting for a client.

## Repository workflow

`prview-rs` is trunk-based on `main`:

- `main` — the trunk and the stable release branch
- feature / fix / chore branches are created from `main` and open PRs back into `main`
- PRs land as merge commits (no squash)
- release tags (`v*`) are cut from `main`

The `prview` tool itself analyzes repositories using any base branch (`develop`, `main`, `master`, …).

## Shell completions

```bash
prview completions bash > $HOME/.local/share/bash-completion/completions/prview
prview completions zsh  > $HOME/.zfunc/_prview
prview completions fish > $HOME/.config/fish/completions/prview.fish
```

## Documentation

- [`docs/INSTALL.md`](docs/INSTALL.md) — installation details
- [`docs/usage.md`](docs/usage.md) — full usage guide
- [`docs/configuration.md`](docs/configuration.md) — policy & config
- [`docs/gate-playbook.md`](docs/gate-playbook.md) — hook recipes and gate rollout
- [`docs/mcp.md`](docs/mcp.md) — MCP server for agents
- [`docs/mcp-smoke.md`](docs/mcp-smoke.md) — MCP smoke walkthrough for agents
- [`docs/architecture.md`](docs/architecture.md) — how it works
- [`docs/development.md`](docs/development.md) — contributing
- [`docs/contracts/merge_gate.md`](docs/contracts/merge_gate.md) — `MERGE_GATE.json` contract

## License

BUSL-1.1 — see [LICENSE](LICENSE). Package and binary are both named `prview`; the GitHub repo remains `prview-rs`.

<p align="center"><sub><code>prview</code> · surfaces the signal before merge</sub></p>
