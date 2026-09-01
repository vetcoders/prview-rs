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
at most two capped heavy parents and never creates more parent permits than the
detected logical-core count; a one-core host therefore remains single-parent
and single-worker. Cargo/rustc receive `CARGO_BUILD_JOBS`, Cargo test binaries
receive `RUST_TEST_THREADS`, and Semgrep receives `--jobs`.
Vitest remains capped at one CLI worker because a higher CLI value would
override and raise a repository's stricter `maxWorkers` setting. Tools without
a stable portable cap (including tsc
and ESLint across supported project versions) remain serialized. High current
load, or an unavailable load reading, backpressures the effective plan to
`safe`. Inherited `CARGO_BUILD_JOBS`, repository Cargo `[build].jobs`, and
positive inherited `RUST_TEST_THREADS` values remain operator ceilings: prview
may lower an effective limit to the active plan, but never raises it. Cargo config is
resolved from the exact reviewed cwd, including a materialized remote snapshot,
using Cargo's nearest-scalar and legacy-`config` precedence. An unreadable,
invalid, zero-valued, or include-dependent `build.jobs` contract fails closed
to one worker instead of guessing a wider limit. Signed inherited
`CARGO_BUILD_JOBS` values use Cargo's logical-core-relative semantics; invalid
or zero inherited values fail closed in the same way. An empty `CARGO_HOME`
falls back to the operator home rather than being mistaken for the reviewed cwd.

Cold Python environment setup and every later `uv run` apply the same cap to
`UV_CONCURRENT_DOWNLOADS`, `UV_CONCURRENT_BUILDS`,
`UV_CONCURRENT_INSTALLS`, and `CARGO_BUILD_JOBS` for Rust-backed Python
packages. Each uv pool is the minimum of the run plan, a positive inherited
environment value, and the matching project value from `uv.toml` or
`[tool.uv]`; `uv.toml` wins when both project files exist. Invalid or
unreadable concurrency authority fails closed instead of widening the run.
`pyproject.toml`, `uv.lock`, and any discovered `uv.toml` must resolve inside the
reviewed tree. An explicit in-tree `UV_CONFIG_FILE` replaces discovery, matching
uv's precedence. An enabled boolish `UV_NO_CONFIG` skips discovered `uv.toml`
and `[tool.uv]` concurrency values, while an explicit `UV_CONFIG_FILE` remains
authoritative even when both variables are set. The project manifest and other
metadata still require containment because uv and the selected Python tools
consume them independently of uv configuration discovery. `UV_PROJECT`,
`UV_WORKING_DIR`, and the legacy `UV_WORKING_DIRECTORY` are refused when they
redirect execution away from the exact reviewed root; an in-tree explicit
config remains the concurrency authority uv will actually use. User- and
system-level uv configuration is not currently reproduced by this project-level
resolver. When uv is unavailable, direct Ruff, Mypy, and Pytest invocations do
not parse `uv.toml`, `uv.lock`, `[tool.uv]`, or uv-only environment selectors;
those inputs cannot affect a process that will not consume them. Generic Python
metadata and the run's `CARGO_BUILD_JOBS` descendant cap remain in force.
Pytest also receives `PYTEST_XDIST_AUTO_NUM_WORKERS`; when project or inherited addopts request
xdist (`-n auto`, `logical`, or explicit `-n N`), prview caps only a dynamic or
too-large pool. An explicit smaller count and `-n 0` remain unchanged. A short,
isolated probe of the actual project pytest selects the matching supported
major/minor config-discovery rules; unsupported versions fail closed. Prview then
passes `-c` for the single highest-precedence config inside the reviewed root,
or an explicit empty config when none exists, and fixes `--rootdir` to that
root. Malformed, unreadable, non-UTF-8, or conflicting recognized config and
addopts are reported instead of silently skipped. A standalone `--` in config
or inherited addopts is rejected because it would disable the later isolation
and worker-cap arguments. Xdist `--tx` and `--px` gateway options are rejected
because their process fan-out is independent of `-n` and cannot be bounded by
the numeric worker override. Parent-directory pytest
options therefore cannot change test selection or worker count. If the operator
already set a lower positive uv/Cargo value, prview preserves that stricter
limit. Arbitrary third-party PEP 517 backends can
still own private worker controls that no portable parent setting can infer;
they remain serialized as one Exclusive parent rather than being claimed as a
universally capped child pool. The same boundary applies to executable project
`conftest.py` code and third-party pytest plugins: Pytest remains Exclusive, but
prview does not claim to infer arbitrary plugin-created processes or xdist hook
mutations.

Before checks start, the human preflight prints the requested/effective budget,
parent and child caps, expensive tools, and the cheap-first execution schedule.
The envelope is conservative; it does not pretend to predict exact future peak
memory.

This envelope covers review generation and its check/context subprocesses. The
explicit mutating `prview fix` command still invokes formatter/fixer toolchains
synchronously and is not yet governed by `--resource-budget`; do not treat the
review envelope as a product-wide cap for that separate command.

### Test selection

`--tests-pattern PATTERN` is runner-aware rather than one portable regex:

| Runner | Selection contract |
|--------|--------------------|
| Vitest | Regular expression passed to `--grep` |
| Cargo/libtest | Literal substring only; regex metacharacters and values beginning with `-` are rejected before Cargo starts |
| Pytest | Not currently filtered by this flag |

A Mixed JS/Rust review runs both Vitest and Cargo with the same value, so the
portable contract is their literal intersection. A regex-specific value makes
the Cargo check `ERROR`; use a literal substring common to both runners, omit
the shared selector, or run runner-specific test commands separately.

A filtered Cargo run is `ERROR`, not `PASS`, unless standard libtest summaries
prove that at least one selected test executed. This prevents both zero-match
false greens and option-shaped values such as `--no-run` from turning a test
gate into compile-only success. Custom Cargo harnesses without a verifiable
libtest summary therefore require an unfiltered run or a separate runner-aware
profile.

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
| `--tests-pattern PATTERN` | Filter Vitest by regex or Cargo/libtest by literal substring; Mixed uses the literal intersection and Pytest remains unfiltered |

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
On Unix, a `latest` symlink points at the most recent **completed** run in
`$PRVIEW_HOME/runs/<repo>/<branch>/latest` or
`$HOME/.prview/runs/<repo>/<branch>/latest` when `PRVIEW_HOME` is unset.
A cancellation observed before the durable publication commit writes
`00_summary/INCOMPLETE.json` and does not update `latest` or the run index. If
cancellation is observed after `latest` has
already been retargeted, the previous completed run is restored. A cancel
during index registration rolls the index file back and does not prune older
runs. Once both advertisements cross the durable commit point, the run is
completed; a signal arriving after that point does not retroactively relabel the
published verdict as cancelled. Unix publishers serialize `latest` and
`index.jsonl` as one transaction, and a durable publication journal reconciles either side after a
process crash **when all publishers are 0.8 or newer**. A journal whose pack is
missing, malformed, or has a mismatched `RUN.json` identity is never trusted to
retarget `latest`: prview preserves it as a uniquely named
`publication-transaction.invalid.*` quarantine file and permits the next clean
publication instead of permanently denying every later run. A valid journal is
not discarded when the durable index is unreadable: transactional readers
reject any invalid JSONL row, preserve the journal, and fail the run rather than
reconstructing or saving a partial ledger. Likewise, failure to commit the
finished pack into the index is fatal; a pack is not reported as completed when
`state`/`verdict` could not discover it. The compatibility lock
serializes the index critical section with pre-0.8 binaries, but it cannot
serialize their earlier `latest` update. Before installing or starting 0.8,
drain and exclude every pre-0.8 publisher that shares `PRVIEW_HOME`; concurrent
mixed-version publication is unsupported. If prview reports a **stale legacy
index lock**, first establish that no pre-0.8 publisher is still running, then
remove exactly the reported legacy sentinel; prview deliberately does not take
it over automatically. Retention
candidates are staged atomically in
`$PRVIEW_HOME/prune-trash`; physical cleanup occurs before the next index
registration and may therefore lag one completed run. If a custom output path
cannot be staged atomically, prview keeps every index row and skips that prune
with a warning. Recovery validates a staged payload's owned regular
`00_summary/RUN.json` identity against the manifest before moving or deleting
it; missing, invalid, or mismatched transaction metadata leaves a payload
preserved in prune-trash without blocking a clean publication. An I/O failure
after recovery begins still aborts publication. If restoration of the previous
index cannot be confirmed, prview leaves the outer publication journal intact
and fails the run so the next lock owner can reconcile durable state. Relative
`--output-dir` values are converted to absolute paths before pack creation, so
restart recovery is independent of a later process's working directory. An
explicit output path must not already exist: it is atomically claimed for one
immutable pack and cannot be reused for a later run. This prevents stale files
and multiple history rows from pretending that one mutable directory contains
several historical packs. `--watch` and `--tui` therefore cannot be combined
with an explicit `--output-dir`: each watch iteration and each interactive TUI
rerun emits a separate immutable pack through the default unique allocator. MCP
keeps the same invariant
through a private, one-shot reservation that only its child process can claim;
ordinary CLI callers cannot adopt an existing directory.
On Windows, every reparse-point directory or file (including junctions and
mount points, not only symlinks) is treated as linked storage. Retention refuses
such an index path and never recursively traverses it during cleanup. These
path checks prevent accidental or state-at-rest link traversal; the retention
store is not a security boundary against a same-user attacker concurrently
swapping directory entries between validation and rename/read.
Parent-directory fsync supplies the stated rename ordering on Unix/macOS. Other
platforms retain atomic/process-crash recovery but do not claim the same
power-loss durability for directory entries.

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
public modules, library crates, and Cargo features. An implicit library target
is present whenever a live `src/lib.rs` exists unless `package.autolib = false`;
explicit targets and the package edition do not turn it off. Absence is not a
`MissingLibRoot`, while an explicit unavailable `[lib]` root remains unknown. A
tracked symlink in the implicit root position is not followed and remains non-neutralizable typed
uncertainty on both sides. Cargo-valid keyword package and library names that
can be addressed as raw identifiers remain part of the census, as do Cargo-valid
special identities `self`, `crate`, `super`, and `Self`; these remain manifest
identity strings rather than synthetic Rust paths. Ordinary Rust
items are projected only for Rust-linkable `lib`/`rlib`/`dylib` outputs. Real
Cargo binary roots are discovered from `src/main.rs`, both supported `src/bin`
layouts, and explicit `[[bin]]` tables with Cargo's `autobins`, edition, path,
and `required-features` rules. For edition 2015, only explicit `[[bin]]` metadata
disables implicit binary discovery by default; unrelated explicit
examples, tests, benches, or libraries do not. An exact tracked binary-root symlink fails
closed; symlinked parent directories such as `src/` or `src/bin/` are not
separately classified by this discovery layer. Each binary has a target-scoped
analysis identity separate from a same-named library. The identity preserves
the manifest target name: digit-prefixed targets are accepted, Cargo's reserved
build-directory names remain invalid, and names such as `foo-bar` and `foo_bar`
remain distinct in prview despite normalizing to the same Rust crate name.
This is an evidence identity only; it does not publish binary internals as Rust
dependency API. Proc-macro exports remain separate, and
native-only `cdylib`/`staticlib`/`bin` targets retain binary-export and target
uncertainty, including exported associated functions in inherent and trait
impls, without pretending that their internal `pub` items are dependency API.
Native export signatures are bound to local type semantics so an alias-only ABI
change cannot neutralize as unchanged evidence. Direct and associated function
bodies are excluded from native ABI evidence; static initializers remain part
of the observable native contract. Native-producing targets
(including a mixed `rlib + cdylib`) also retain typed potential-export evidence
when a custom associated attribute or item-position macro can generate the
native symbol; an
internal associated macro in an `rlib`-only private owner remains outside the
external contract. Exported declarative macro contracts
bind the effective direct, workspace-inherited, or library-target edition; an
edition change without such a macro does not create a synthetic break.
Tuple-constructor privacy,
`repr(C)` named-field order, private field types under every repr, primitive
integer enum reprs, and exhaustive-enum variant additions are observable
changes. `repr(Rust)`, `repr(transparent)`, and standalone
`repr(packed)`/`repr(align)` private named-field order is not; their attributes
and private field types remain observable through layout and auto traits.
Inherited and restricted field visibility are equivalent to an external caller.
Appending a fieldless
variant to an otherwise unchanged `#[non_exhaustive]` enum is informational
unless an ABI-sensitive repr makes layout observable. A new payload-bearing
variant, a fieldless variant inserted before an existing implicit discriminant,
a field added to an existing variant, or a field added to an existing public
struct remains a parent `Changed`: non-exhaustive syntax prevents exhaustive
construction and matching, but does not prove stable auto traits or numeric
discriminants.
Direct private-field types remain in that confirmed parent contract. When a
public item instead reaches a transitive non-public local type, private alias,
or local trait implementation whose compiler-derived effect cannot be proven
from source, prview emits guard-aware `PrivateTypeDependency` uncertainty. It
does not promote that evidence to a confirmed breaking change.
Expression-position include macros retain included-byte digests.
Transforming-attribute unknowns, including recursively nested `cfg_attr`, are
collected before visibility filtering and bound both to their annotated input
and to revision-backed transformer provenance. Derives are additive: confirmed
input-item changes remain visible,
while custom generated output stays Unknown; custom/helper tokens are not
duplicated as confirmed breaking semantics, builtin-looking names shadowed by
imports remain conservative, and associated transforms require an externally
reachable inherent owner. Builtin `Default` helpers follow matching nested
  conditional predicate lineage; relationships that cannot be proven remain
  typed uncertainty. Lock-backed external candidates use reachable manifest,
  effective Cargo config from every reachable member context, and lock identity;
  their external source, registry checksum or precise Git commit, and locked
version must satisfy the declared package requirement. When a local proc-macro
is reachable, the conservative safety floor hashes every live
tracked entry by Git object identity (excluding redundant directory-tree
objects) so nonstandard crate roots, pinned gitlinks, and build assets do not
disappear. Only the effective product/workspace lock qualifies; a fixture lock,
stale lock missing a dependency candidate, or tracked symlink whose target bytes
are not revision-proven does not. Missing lock/candidate provenance and
unresolved manifest/config Cargo source replacement never neutralize, while the
local aggregate may over-report after an unrelated tracked-file edit. Custom
cfg predicates bind revision-backed build-script or Cargo-config authority when
present, including nested public contract positions. A declared build script
must be a live revision file; `build = true` selects the default `build.rs` and
`build = false` disables discovery. Only an effective repository-root config
whose legal build/target rustflags or a concrete-target link-override rustc-cfg
matching the package's `links` can define custom cfg qualifies. If both root
config filenames exist, Cargo's extensionless `.cargo/config` precedence is
preserved. The package must still own a live build script when it declares
`package.links`.
Child-process environment settings, nested configs, and lookalike keys in
unrelated tables do not qualify. The conservative digest may likewise over-report, but a missing,
invalid, included, or otherwise unresolved authority proof never cancels merely
because both sides have the same diagnostic text. Public trait method and
associated-const defaults are directional and structural: adding a default is
compatible, while removal, const value/type/cfg changes, or moving a default
between disjoint cfg-qualified members remains a confirmed contract change.
Body or private-helper changes affecting a
caller-observable `async fn` or return-position `impl Trait` produce item-local
`OpaqueReturnAutoTraits` uncertainty bound to canonicalized repo-backed Rust,
all other live tracked input identities, and effective lock data. This covers
nonstandard include/path/build inputs without reading every tracked blob. A
tracked symlink keeps this proof unresolved; a wholly new or removed opaque item
relies on its Added/Removed fact instead of a redundant Unknown, and adding an
async trait default is treated the same way. This conservative source closure
may over-report after an unrelated tracked input
changes, but it does not promote uncertainty to a confirmed break. Legal
non-include macro invocation token bodies remain opaque to source-level binder
normalization. Their invocations bind to a revision-backed implementation
substrate, and conditional `macro_export` declarations remain crate-root API.
Rust-AST opaque bodies alpha-normalize generic, parameter, irrefutable local
destructuring, closure, loop, and lexical-shadow binder spellings. Refutable
match/`if let`/`while let` pattern names remain spelling-sensitive without name
resolution; macro token bodies are not compiler-backed semantic proof. Private
binary symbols exported by direct or conditional `no_mangle`/`export_name`
produce guard-aware typed uncertainty. Legal non-UTF-8 Git paths emit
side-specific typed path uncertainty while valid sibling
files continue to be analyzed; their collision-free internal identity cannot be
forged by a literal UTF-8 surrogate filename. Multiple independent workspace
authorities in a rootless revision source emit `WorkspaceDiscovery` uncertainty
instead of unioning product and fixture crates. An unreadable, malformed, or
non-UTF-8 rootless manifest is also an unresolved authority and cannot be
discarded in favour of a parseable sibling. The same fail-closed rule applies to
a parseable `Cargo.toml` that defines neither `[package]` nor `[workspace]`;
`[workspace]` and `package.workspace` cannot coexist in one manifest.
When a root package points at another workspace with `package.workspace`, that
workspace's full member authority is used; unreadable, invalid, incomplete, or
non-reciprocal membership remains `WorkspaceDiscovery` uncertainty.
Unqualified imported trait impls on public owners remain typed uncertainty
until compiler-backed name resolution exists. Trait/owner alias spelling is
canonicalized at the resolved nominal pair, including reference, pointer,
slice, and array owners. Each resolved trait remains correlated with only the
owner cfg regions where that impl can exist; cfg-selected trait swaps therefore
cannot collapse into one global evidence set. Ordinary impl members are compared
as an unordered set. Declaring scope intentionally remains part of the proof because relative
paths inside an impl can change meaning when it moves modules. Aliases used only
inside generic arguments can therefore still produce a conservative warning;
they are not neutralized without compiler-backed name resolution.

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
