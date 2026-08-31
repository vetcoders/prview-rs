# prview usage guide

This document is the user guide for `prview`.
If you want to work on the repo, see `docs/development.md`.
If you want to understand the architecture, see `docs/architecture.md`.
For the `prview.toml` and `.prview-policy.yml` configuration files, see `docs/configuration.md`.

## Installation

### From source

```bash
cd prview-rs
cargo build --release
./target/release/prview --help
```

### Globally (cargo install)

```bash
cargo install --path .
prview --help
```

Full installation details, including release binaries and checksums, are in `docs/INSTALL.md`.

## Basic usage

### Auto mode (default)

```bash
prview
```

Automatically:
- detects the profile (JS/Rust/Python/Mixed)
- uses the current branch as the target
- diffs against the repo's default base
- in standard mode, generates the full artifact pack
- runs tests and lint by default, unless you pick a lighter mode (`--quick`, `--update`, `--ai-only`) or an explicit `--skip-*`

The `prview` tool can analyze repositories that use any base branch
(`develop`, `main`, `master`, etc.); by default it resolves the first of
`develop`, `main`, `master` that exists.

### Quick mode

```bash
prview --quick
```

Skips tests, lint, and heuristics. Diffs and core artifacts only.

`prview --quick` analyzes the current local branch (`HEAD`) and compares it
against the bases resolved for the current repo. It does not pick a PR number
automatically.

To review a specific GitHub PR:

```bash
prview --pr 23 --quick
```

### Gate mode

```bash
prview gate
prview gate --strict
prview gate --strict --fail-on-warnings
prview gate --json
```

`prview gate` runs the standard fast gate profile, consumes the existing
policy/merge-gate verdict, and exits with a stable automation contract. It does
not compute a second verdict path.

| Exit code | Meaning |
|-----------|---------|
| `0` | `PASS`, advisory `CONDITIONAL`, or typed warnings-only under `--strict` |
| `1` | `BLOCK` |
| `2` | Review-required under `--strict`, or warnings-only with `--strict --fail-on-warnings` |
| `3` | Gate execution failed before a trustworthy verdict was available |

`--json` makes stdout machine-readable (`schema_version: "gate-json/v1"`) with
the verdict, `enforcement_disposition`, caveats, blocking issues, and artifact
paths. Only a schema 2.3 pack with typed warning proof can use the warnings-only
strict exception; older or malformed packs remain strict-rejected.

Local pre-push hook recipes and the recommended Shadow -> Warn -> Block rollout
are in [`docs/gate-playbook.md`](gate-playbook.md).

#### Gate profile and measured pre-push budget

`prview gate` applies its own deterministic pre-push profile. It runs as a
quick, quiet review and does not inherit global step opt-ins such as
`--with-tests`, `--with-lint`, `--with-security`, or `--security-full`.

Effective profile:

| Surface | Gate behavior |
|---------|---------------|
| Rust / Cargo | `Cargo check` runs; `Clippy`, `Rustfmt`, `Cargo test`, and `Cargo audit` stay visible as skipped checks |
| Security | Semgrep runs when the `semgrep` binary is available |
| Geiger | `Cargo geiger` is out of the gate profile |
| Tests, lint, bundle, heuristics | Disabled for the pre-push gate budget |
| JS/TS | Existing JS checks only run when repo-local `node_modules` tools exist; they are not part of the measured budget below |

Measured on 2026-07-06 with a prebuilt release binary
(`target/release/prview`); compile time is excluded.

| Repo checkout | State | Runs | Wall times | Mean | Verdicts | Dominant measured check |
|---------------|-------|------|------------|------|----------|--------------------------|
| `prview-rs` | `feat/gate-core`, W1-B worktree dirty | 2 | 8.73s, 7.36s | 8.05s | `CONDITIONAL` / exit 0 | Semgrep 4.41s; `Cargo check` cached |
| `pensieve` | `chore/deprivatize`, clean checkout, 14 commits behind origin | 2 | 48.27s, 45.95s | 47.11s | `CONDITIONAL` / exit 0 | Semgrep 32.78s |

### GitHub Actions

This repository ships a composite Action at `action.yml` for external CI usage.
It installs `prview`, runs `prview gate --json`, and maps the step result only
from the gate exit-code contract:

| Exit code | Action result |
|-----------|---------------|
| `0` | success (`PASS`, advisory `CONDITIONAL`, or typed warnings-only in strict mode) |
| `1` | failure (`BLOCK`) |
| `2` | failure (strict review-required, or warnings-only with `fail-on-warnings`) |
| `3` | failure (gate execution error) |

Minimal blocking gate:

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
      - uses: vetcoders/prview-rs@v0.7.0 # current published Action
        id: prview
        with:
          strict: "true"
          version: "0.7.0"
      - uses: github/codeql-action/upload-sarif@v3
        if: ${{ steps.prview.outputs['sarif-path'] != '' }}
        with:
          sarif_file: ${{ steps.prview.outputs['sarif-path'] }}
```

Use `strict: "false"` for advisory rollout: `CONDITIONAL` remains exit `0`,
while `BLOCK` still exits `1`. Extra CLI flags can be passed as whitespace-
separated `args`. On the published 0.7 lane, `strict: "true"` rejects every
`CONDITIONAL`. In the staged 0.8 source contract, typed warnings-only remains
successful under strict mode; after 0.8 is published and the pins move, set
`fail-on-warnings: "true"` to require a warning-clean pack as well.

The Action prefers `cargo-binstall` when that binary is already available on the
runner and falls back to `cargo install prview --locked --force`. The base gate
exists from `0.6.0`; typed warnings-only enforcement and the
`fail-on-warnings` Action input are staged for `0.8.0`; release preparation
owns switching both pins and adding that input only after the tag and crate are
published. This copy-pasteable example deliberately uses the currently
published `v0.7.0` Action/runtime and its historical verdict-only strict
semantics. Until 0.8 is published, exercise its contract from source rather
than using an unissued release pin.

SARIF upload requires `permissions: security-events: write`. prview writes
`30_context/INLINE_FINDINGS.sarif` only when there are inline findings or
advisories. GitHub code scanning limits matter for large diffs: SARIF uploads
must be at most 10 MB after gzip compression, and GitHub displays at most 50
annotations per workflow step.

### Specific branch and base

```bash
prview feature/my-feature main
prview feature/x main
```

## Execution modes

| Mode | What it does |
|------|--------------|
| standard (default) | Full review with tests and lint enabled by default |
| `--quick` | Light pass: skip tests/lint/bundle/heuristics |
| `--deep` | All heavier checks enabled (including security and heuristics) |
| `--ci` | Like deep, tuned for automation: no colors; non-zero on Block/quality failure |
| `--update` | Incremental rerun after new commits, skipping heavy checks unless forced |
| `--ai-only` | Minimal artifact pack for AI/review flows |

### Resource budget

`prview` defaults to `--resource-budget safe`: at most one whole-machine tool
runs at a time and supported descendant pools receive one worker. This is the
recommended setting for ordinary developer machines.

`--resource-budget balanced` is an explicit throughput opt-in. It still admits
at most two capped heavy parents; Cargo/rustc receive `CARGO_BUILD_JOBS`, Vitest
receives `--maxWorkers`, and Semgrep receives `--jobs`. Tools without a stable
portable cap (including tsc and ESLint across supported project versions) remain
serialized. High current load, or an unavailable load reading, backpressures the
effective plan to `safe`.

Before checks start, the human preflight prints the requested/effective budget,
parent and child caps, expensive tools, and the cheap-first execution schedule.
The envelope is conservative; it does not pretend to predict exact future peak
memory.

## Quick cheat sheet

```bash
# 1) Fast daily check of the current branch
prview --quick

# 2) Check a specific GitHub PR
prview --pr 23 --quick

# 3) Deeper PR analysis (tests + lint + full gate)
prview --pr 23 --deep

# 4) Compact JSON for automation (stdout = JSON only)
prview --pr 23 --quick --json --quiet

# 5) Automation gate with contractual exit codes
prview gate --json

# 6) Pick up new commits without a full run
prview --update

# 7) Fast machine-readable repo state (branch, HEAD, dirty, latest run)
prview state --json --fast
```

## zsh aliases (`prv`, `prvpr`, `prvjson`)

The repo ships a ready-to-source file:

`tools/shell/prview-aliases.zsh`

To wire it up:

```bash
echo 'source $HOME/Git/prview-rs/tools/shell/prview-aliases.zsh' >> $HOME/.zshrc
source $HOME/.zshrc
```

## Flags

This is a practical shortlist of the most common flags.
The full, always-current option list is available via:

```bash
prview --help
```

### Presets

| Flag | Effect |
|------|--------|
| `--quick` | Skip tests/lint/bundle/heuristics |
| `--deep` | Enable all checks (including security and heuristics) |
| `--ci` | CI mode: all checks, no colors; Block/quality failure exit |
| `--ai-only` | Minimal checks, AI context pack only |

### Step control

| Flag | Effect |
|------|--------|
| `--with-tests` | Force tests on when a preset disabled them; also restores tests for standard `--remote-only` |
| `--skip-tests` | Skip tests |
| `--with-lint` | Force linters on when a preset disabled them; also restores heavier Rust lint for standard `--remote-only` |
| `--skip-lint` | Skip linters |
| `--with-bundle` | Enable the bundle build |
| `--skip-bundle` | Skip the bundle build |
| `--with-security` | Raise the heavy security posture (does not add cargo-geiger or full-tree Semgrep) |
| `--skip-security` | Skip heavy security checks |
| `--security-full` | Full security tier: runs full-tree Semgrep and adds cargo-geiger's unsafe scan (slow; off even under `--deep`) |
| `--resource-budget safe\|balanced` | Select the whole-machine envelope (`safe` is the default; `balanced` is capped and load-aware) |

By default, Semgrep is scoped to the change when prview can resolve a clean git
baseline: it passes Semgrep `--baseline-commit <merge-base>` so existing
findings outside the diff do not degrade the merge verdict. If the worktree is
dirty or the merge-base is unavailable, prview falls back to a full scan and the
artifact classifier still separates introduced from pre-existing findings.
Semgrep parse/scan errors remain surfaced even when no rule findings are
introduced. When Semgrep reports paths in `errors[]`, the merge decision caveats
name the files that were only partially parsed, so the reviewer does not need to
open the raw tool log to discover the analysis gap. Up to ten paths are shown in
the caveat, followed by a remaining count when necessary.

Cargo-audit advisories are baseline-classified as `new`, `pre-existing`,
`resolved`, or `unknown-baseline`. If `Cargo.lock` did not change, every current
advisory is safely pre-existing. If it changed, prview audits the base lockfile
governing the same Cargo root as the live check (a member lock first, then the
workspace lock) and compares advisory, package, and locked version. Inability to
establish that base remains explicit as baseline `unavailable`; current
findings stay unclassified, including for lock-only changes, instead of being
inferred from manifest deltas. Invalid or truncated current tool output fails
the check and uses a separate `current-unavailable` baseline state; it is never
interpreted as a clean report.

An intentionally omitted check is described as “Not executed by this PrView
run. External CI status not included.” The reason from this run is retained;
prview never treats an absent local execution as evidence that an external CI
job passed or failed.

### Profiles

| Flag | Effect |
|------|--------|
| `--profile auto` | Auto-detect (default) |
| `--profile js` | JavaScript/TypeScript only |
| `--profile rust` | Rust only |
| `--profile python` | Python only |
| `--profile mixed` | All available |

### Special modes

| Flag | Effect |
|------|--------|
| `--update` | Incremental update (skip heavy checks) |
| `-R, --remote` | Analyze a branch from origin/ |
| `--remote-only` | Resolve bases from origin/* only; a faster remote-review preset in standard mode |
| `--local-only` | Resolve bases from local refs only |
| `--policy-file <path>` | Path to `.prview-policy.yml` |
| `--policy-mode <shadow\|warn\|block>` | Override the policy mode |
| `--why-blocked` | Explain why the merge gate is blocking |

### Output

| Flag | Effect |
|------|--------|
| `-q, --quiet` | Minimal output |
| `--json` | JSON output |
| `--no-color` | Disable ANSI colors |
| `--no-zip` | Skip ZIP creation |
| `--no-dashboard` | Skip HTML dashboard generation |
| `--soft-exit` | Always exit 0, whatever the checks found |
| `--fail-on-warnings` | With `--ci`: also exit 1 when any check reports warnings |

### `--ci` exit codes

`--ci` is the automation variant of the plain review run: it exits `1` on a `BLOCK`
verdict or a broken quality gate, and `0` otherwise. Warning-level signals — a
formatter delta, an unmaintained-crate advisory, lint warnings — are advisory:
they can keep the verdict at `PASS` or `CONDITIONAL` and surface as review
caveats, but they do not fail the process. Add `--fail-on-warnings` to opt into
exit `1` for `--ci`; that top-level flag still requires `--ci`. The gate
subcommand exposes its own equivalent lane as
`prview gate --strict --fail-on-warnings`, which exits `2` for typed
warnings-only (see `docs/gate-playbook.md`).

`--fail-on-warnings` counts the artifact pack's canonical typed warning tally,
not only the CLI's in-memory check list. That tally includes `checks[]` plus the
validated inline-findings warning source. The artifact run also generates
further checks — `public_api_diff`, `unsafe_audit`, `ghost_refs` and the
synthetic `heuristics_loctree` — which reach `MERGE_GATE.json` and the dashboard
but never the in-memory report the plain tally is built from. The `--json`
summary states both numbers:
`checks_summary.warned` is what the CLI ran, `checks_summary.warned_in_pack` is
the complete count the flag keys off, and it is always the larger of the two.

Strictness follows the `--ci` you typed, not the preset label the run reports.
`--update` outranks `--ci` when the execution preset is resolved, so
`prview --ci --fail-on-warnings --update` publishes `mode.execution_mode:
"update"` — and reading strictness off that label made both `--ci` exits
(`!quality_pass` and the warning hardening) silently inert for exactly the
combination CI jobs use.

An `--update` run that finds no new commits reuses the previous pack, and its
exit code is derived from that pack like any other run's: a reused `BLOCK` or a
reused warning under `--fail-on-warnings` exits non-zero rather than reporting a
green second invocation over an artifact nothing re-checked. `--soft-exit`
remains the one way to ask for `0` regardless.

## Examples

### Rust project

```bash
prview --profile rust --with-tests feature/x main
```

### Python project

```bash
prview --profile python --with-tests --with-lint
```

### Mixed (Tauri app)

```bash
prview --profile mixed --deep
```

### CI pipeline

```bash
prview --ci feature/x main
```

### Incremental update

After new commits on a branch:

```bash
prview --update feature/x main
```

### JSON for scripts

```bash
prview --json --quiet feature/x main > summary.json
```

After the initial banner, human-readable non-watch runs show a `PRVIEW CONFIG`
panel. The panel follows the active terminal width, wraps long refs and notes
within aligned walls, and falls back to an unboxed list when the terminal is too
narrow to draw a coherent frame or its width cannot be queried safely.
Redirected human output uses a deterministic 100-column layout; `--watch` and
`--json --quiet` emit no panel, with the latter remaining the machine-readable
surface.

In `--json --quiet` mode, stdout contains only the compact JSON summary
(no banners or text summaries). This payload is meant for scripts and agents:
it carries the verdict, `output_dir`, a short `checks_summary`, `top_failures`,
`context_artifacts`, and paths to artifacts. Full detail stays in the run files
on disk, especially the canonical `RUN.json` and `MERGE_GATE.json` pair, plus
`PR_REVIEW.md`.

The verdict fields (`verdict`, `allow_merge`, `quality_pass`,
`merge_recommendation`, `analysis_status`) are read from the run's
`00_summary/MERGE_GATE.json` and from nowhere else. If that artifact is missing,
unparsable, or stamped with a `schema_version` this build cannot read, prview
reports an execution error and exits `3` instead of re-deriving a verdict —
including on `--update` runs that re-read an earlier pack. A newer MINOR schema
within a known MAJOR is accepted and reported through the optional `caveats`
array, which also carries any verdict the reader had to normalize.

## Output

Artifacts are written to `$PRVIEW_HOME/runs/<repo>/<branch>/<run_id>/`
or, when `PRVIEW_HOME` is unset, to
`$HOME/.prview/runs/<repo>/<branch>/<run_id>/` in an ordered numbered layout.
New run ids use a timestamp plus short HEAD suffix, for example
`20260704-120500-a1b2c3d`; treat the full value as opaque.
A `latest` symlink points at the most recent **completed** run in
`$PRVIEW_HOME/runs/<repo>/<branch>/latest` or
`$HOME/.prview/runs/<repo>/<branch>/latest` when `PRVIEW_HOME` is unset.
A cancelled run writes `00_summary/INCOMPLETE.json` and does not update
`latest` or the run index. If cancellation is observed after `latest` has
already been retargeted, the previous completed run is restored. A cancel
during index registration rolls the index file back and does not prune older
runs. Retention candidates are staged atomically in
`$PRVIEW_HOME/prune-trash`; physical cleanup occurs before the next index
registration and may therefore lag one completed run. If a custom output path
cannot be staged atomically, prview keeps every index row and skips that prune
with a warning.

```
$HOME/.prview/runs/my-repo/feature-x/20260225-185357/

├── 00_summary/
│   ├── RUN.json             # Run metadata, execution mode, check inventory
│   ├── PROVENANCE.json      # What was analysed: base/head/target SHAs, worktree state, per-check substrate
│   ├── FAILURES_SUMMARY.md  # Compact blocking failures without raw dumps
│   ├── MANIFEST.json        # SHA256 hashes for generated files
│   ├── SANITY.json          # Integrity validation results
│   ├── MERGE_GATE.json      # Machine-readable merge decision
│   ├── pr-metadata.txt      # Branch, bases, profile
│   ├── file-status.txt      # A/M/D + file paths
│   └── commit-list.txt      # hash date author message
├── 10_diff/
│   ├── full.patch            # Full diff with diff-stat header (+N -N per file)
│   ├── per-commit-diffs/     # Individual per-commit patches
│   │   ├── 00-SUMMARY.md     # Commit stats + batch mapping + thematic labels
│   │   ├── 01-abc1234-message.patch
│   │   └── ...
│   └── per-file-diffs/       # Hotspots: files with >=80 lines changed
│       ├── 00-INDEX.txt      # Index with diff-stat per file
│       └── *.patch
├── 20_quality/
│   ├── *.result.json         # Per-check machine-readable outputs
│   ├── *.log                 # Per-check raw logs
│   ├── full-checks.log       # Full output from all checks
│   ├── checks-errors.log     # Filtered: errors/warnings only, with +/-2 lines of context
│   ├── coverage-delta.txt    # Source<->test mapping with change status
│   ├── PUBLIC_API_DIFF.json  # Compatibility rows + lossless Rust ApiDelta
│   ├── PUBLIC_API_DIFF.md    # Human API summary
│   ├── BREAKING_CHANGES.json # Lossless Rust ApiDelta breaking view
│   └── BREAKING_CHANGES.md   # Human Rust + bounded JS/TS/env summary
├── 30_context/
│   ├── INLINE_FINDINGS.sarif # Optional: only when the run has findings/advisories
│   ├── changed-tests.txt     # Test files changed in this PR
│   ├── cargo-tree.txt        # (Rust) dependency tree for CVE/dependency paths
│   ├── tsc-trace.log         # (JS) optional module-resolution diagnostics
│   ├── eslint-report.json    # (JS) ESLint result
│   └── vitest-report.json    # (JS) Vitest result
├── dashboard.html            # Visual HTML summary
└── artifacts.zip             # Everything zipped
```

### Key artifacts

#### Signal generators (20_quality/, 10_diff/)

Several generators produce high-signal artifacts — each writes a file **only when**
it has something worth showing:

| Artifact | Description | When generated |
|----------|-------------|----------------|
| `checks-errors.log` | Errors and warnings from checks with +/-2 lines of context | When checks found errors |
| `PUBLIC_API_DIFF.json/.md` | Exact-tree Rust API delta plus legacy JS/TS export compatibility rows | When an API fact or typed unknown exists |
| `BREAKING_CHANGES.json/.md` | Same Rust delta IDs/counts/evidence used by the gate, plus bounded JS/TS and env presentation | When a Rust API fact/unknown or legacy JS/TS/env signal exists |
| `coverage-delta.txt` | Source-file-to-test mapping (multi-strategy matching) | When the diff contains source files |
| `per-file-diffs/` | Individual patches for files with `>=80` lines changed | When such hotspots exist |

When a generator produces no file, the CLI prints an `i` note explaining why.

Rust API facts are computed from the exact `Diff.base_commit_id` and
`Diff.target_commit_id` Git trees; the working tree and patch text are not
fallbacks. Duplicate exact OID pairs are compared once; distinct base/target
comparisons retain separate provenance. JS/TS exports remain on the legacy diff
analyzer behind a side-aware boundary: a cross-language rename retains only its
JS/TS side, including standard quoted Git paths, so Rust lines cannot leak and
removed JS exports cannot disappear. File markers must agree exactly with the
decoded Git header identity; incoherent, truncated, or markerless hunk sections
are discarded fail-closed, while coherent mode-only add/delete metadata remains
valid. Confirmed removed, changed, relocated, and
visibility-changed Rust facts are breaking; added-only facts are informational.
Typed unknowns degrade confidence and require review without claiming a
confirmed removal. Rust identities include ordinary type/value/macro items plus
public modules, library crates, and Cargo features. Tuple-constructor privacy,
ABI-sensitive `repr(C)`/`repr(packed)`/`repr(transparent)` private layout, and
exhaustive-enum variant additions are observable changes. `repr(Rust)` private
field order is not, although private field types remain observable through auto
traits. Additions to an otherwise unchanged `#[non_exhaustive]` enum, and named
fields added to a variant-level `#[non_exhaustive]` variant, are informational
unless an ABI-sensitive repr makes layout observable.
Expression-position include macros retain included-byte digests, and
transforming-attribute unknowns are bound to their annotated input. Legal
non-UTF-8 Git paths emit side-specific typed path uncertainty while valid sibling
files continue to be analyzed; their collision-free internal identity cannot be
forged by a literal UTF-8 surrogate filename. Multiple independent workspace
authorities in a rootless revision source emit `WorkspaceDiscovery` uncertainty
instead of unioning product and fixture crates. Unqualified imported trait impls on public owners
remain typed uncertainty until compiler-backed name resolution exists.

#### How to read an artifact pack

- `00_summary/MERGE_GATE.json` is the canonical source of check statuses.
- `00_summary/PROVENANCE.json` answers *what was judged*: the target/base/head commits, whether the local
  working tree was clean when the run started (plus a digest fingerprinting what was dirty, content included),
  and, per check, the directory and commit it actually read. `bases[]` names every baseline the pack's patches
  were computed from — the merge base of each diff, not the tip of the base branch — and `base_sha` is its first
  entry, kept for older consumers. A `cached: true` row replays the provenance of the earlier run
  that filled the cache entry, and a row with a non-null `skipped` is a gate that was ruled out
  before it ran, with the reason.
- `PR_REVIEW.md` is a concise review narrative, not a raw log dump.
- `00_summary/FAILURES_SUMMARY.md` summarizes blocking failures and advisories without copying whole JSON files.
- When `30_context/INLINE_FINDINGS.sarif` exists, it emits findings per location/advisory and is suitable for annotation integrations.
- In Rust runs, `PR_REVIEW.md` and `FAILURES_SUMMARY.md` can surface dependency paths to vulnerable crates based on `30_context/cargo-tree.txt`.
- `10_diff/per-commit-diffs/00-SUMMARY.md` carries both the batching and thematic batch labels.
- `20_quality/coverage-delta.txt` is a heuristic; for Rust it drops DELETED files in favor of disk searches for orphaned tests (raising an `ORPHANED_TEST_DETECTED` flag). Inline `#[cfg(test)]` modules are handled silently, without false alarms.
- In fast `remote-only` runs, heavier diagnostics such as `30_context/tsc-trace.log` or `30_context/tauri-info.log` may be intentionally skipped. Check `RUN.json` for `recommended`/`reason` notes on whether they are worth generating.

#### Commit batching

For large PRs, per-commit diffs are grouped automatically:

| Commit count | Behavior |
|--------------|----------|
| <=10 | Individual `.patch` per commit |
| 11–50 | Batches of 5 commits in `batch-NN.patch` |
| >50 | Per-commit diffs skipped (only `full.patch`) |

#### MERGE_GATE

The policy-aware merge decision. Details: `docs/contracts/merge_gate.md`.

## Troubleshooting

### "Not inside a git repository"

Run from a directory inside a git repo.

### "Could not resolve ref: &lt;branch&gt;"

The named base branch does not exist. Pass an existing branch explicitly:

```bash
prview feature/x develop
# or, if the repo has no develop:
prview feature/x main
```

### TypeScript check fails

Make sure `tsconfig.json` exists and `pnpm`/`npm` are available.

### Cargo check fails

Check that `cargo` is on PATH and `Cargo.toml` is valid.
