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
| `--ci` | Like deep, tuned for automation: no colors, strict non-zero exit |
| `--update` | Incremental rerun after new commits, skipping heavy checks unless forced |
| `--ai-only` | Minimal artifact pack for AI/review flows |

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

# 5) Pick up new commits without a full run
prview --update

# 6) Fast machine-readable repo state (branch, HEAD, dirty, latest run)
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
| `--ci` | CI mode: all checks, no colors, strict exit |
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
| `--with-security` | Raise the heavy security posture (does not add cargo-geiger) |
| `--skip-security` | Skip heavy security checks |
| `--security-full` | Full security tier: adds cargo-geiger's unsafe scan (slow; off even under `--deep`) |

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

In `--json --quiet` mode, stdout contains only the compact JSON summary
(no banners or text summaries). This payload is meant for scripts and agents:
it carries the verdict, `output_dir`, a short `checks_summary`, `top_failures`,
`context_artifacts`, and paths to artifacts. Full detail stays in the run files
on disk, especially the canonical `RUN.json` and `MERGE_GATE.json` pair, plus
`PR_REVIEW.md`.

## Output

Artifacts are written to `$PRVIEW_HOME/runs/<repo>/<branch>/<timestamp>/`
or, when `PRVIEW_HOME` is unset, to
`$HOME/.prview/runs/<repo>/<branch>/<timestamp>/` in an ordered numbered layout.
A `latest` symlink points at the most recent run in
`$PRVIEW_HOME/runs/<repo>/<branch>/latest` or
`$HOME/.prview/runs/<repo>/<branch>/latest` when `PRVIEW_HOME` is unset.

```
$HOME/.prview/runs/my-repo/feature-x/20260225-185357/

├── 00_summary/
│   ├── RUN.json             # Run metadata, execution mode, check inventory
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
│   └── BREAKING_CHANGES.md   # Removed pub symbols, changed signatures
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
| `BREAKING_CHANGES.md` | Removed `pub` symbols, changed signatures, new env requirements | When breaking changes are detected |
| `coverage-delta.txt` | Source-file-to-test mapping (multi-strategy matching) | When the diff contains source files |
| `per-file-diffs/` | Individual patches for files with `>=80` lines changed | When such hotspots exist |

When a generator produces no file, the CLI prints an `i` note explaining why.

#### How to read an artifact pack

- `00_summary/MERGE_GATE.json` is the canonical source of check statuses.
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
