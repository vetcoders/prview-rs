# prview architecture (repo prview-rs)

This document is the system map.
For CLI usage, see `docs/usage.md`.
For the contributor workflow, see `docs/development.md`.

## Overview

`prview` is a CLI tool for PR analysis, developed in the `prview-rs` repo. Its
main properties:
- single binary (easy distribution)
- fast git operations (git2/libgit2)
- typed code (easier maintenance)
- parallel checks (rayon/tokio)

## Directory structure

```
prview-rs/
├── Cargo.toml           # Dependencies and metadata
├── src/
│   ├── main.rs          # Entry point, CLI parsing
│   ├── lib.rs           # App struct, top-level orchestration
│   ├── cli/
│   │   └── mod.rs       # Clap derive structs
│   ├── config/
│   │   └── mod.rs       # Config, profile detection
│   ├── policy/
│   │   └── mod.rs       # Policy parser + blocking semantics
│   ├── git/
│   │   └── mod.rs       # Git operations (git2), per-file stats via Patch API
│   ├── checks/
│   │   ├── mod.rs       # Check trait, runner
│   │   ├── typescript.rs
│   │   ├── cargo.rs
│   │   └── python.rs
│   ├── heuristics/
│   │   ├── mod.rs       # HeuristicsResult, runner
│   │   └── loctree.rs   # Loctree heuristic (universal)
│   ├── artifacts/
│   │   ├── mod.rs         # Core: layout, patches, merge gate, ZIP
│   │   ├── report.rs      # ReportJson struct + serialization
│   │   ├── parsers/       # Tool output parsers producing LintFinding
│   │   │   ├── mod.rs         # LintFinding struct, is_generated_path
│   │   │   ├── cargo_test.rs  # cargo test output parser
│   │   │   ├── clippy.rs      # cargo clippy JSON parser
│   │   │   ├── eslint.rs      # ESLint JSON parser
│   │   │   └── stylelint.rs   # Stylelint JSON parser
│   │   ├── signal/        # Domain-specific signal generators (16 modules)
│   │   └── dashboard.rs   # HTML dashboard generation
│   ├── mcp/               # MCP server over stdio (agent integrations)
│   │   ├── mod.rs         # Tool router + tool handlers
│   │   ├── run.rs         # run_review: spawn prview subprocess, quick/deep
│   │   ├── read.rs        # Run resolution, decision normalization, artifact reads
│   │   └── types.rs       # Schema version, error classes, fail-loud helpers
│   ├── scope/             # Scoped review packs (filter by files/commits)
│   ├── paths.rs           # Repo-bounded path validation utilities
│   ├── proc.rs            # Subprocess hardening (process groups, timeout kill)
│   ├── check_id.rs        # Stable check identifiers
│   ├── regression/        # Regression detection (diff, perf, deps, score)
│   │   └── mod.rs         # (+ diff.rs, perf.rs, deps.rs, score.rs, tests.rs)
│   ├── storage/
│   │   └── mod.rs         # Persistent run storage ($PRVIEW_HOME, default: $HOME/.prview)
│   ├── state/             # Incremental state (hot reload, repo tree, diffs)
│   │   └── mod.rs         # (+ diff.rs, hot.rs, repo.rs, tree.rs)
│   ├── cache/
│   │   └── mod.rs       # Hash-based caching
│   ├── tui/
│   │   ├── mod.rs       # TUI mode entry
│   │   ├── types.rs     # TUI state types
│   │   ├── keys.rs      # Keybindings
│   │   └── ui.rs        # Rendering
│   └── output/
│       └── mod.rs       # Terminal output formatting
├── tools/
│   ├── validate_merge_gate.py  # CI validator for MERGE_GATE.json
│   └── shell/
│       └── prview-aliases.zsh  # zsh aliases
└── docs/
    ├── architecture.md  # This file
    ├── usage.md         # Usage guide
    ├── development.md    # Developer guide
    ├── mcp.md           # MCP server reference
    └── contracts/
        └── merge_gate.md # MERGE_GATE contract
```

## Execution flow

```
main.rs
    │
    ▼
Cli::parse()              ─── clap parses arguments
    │
    ▼
App::new(&cli)            ─── builds Config + opens Repository
    │
    ▼
app.run()
    │
    ├─► resolve_target()       ─── resolves the target branch
    ├─► resolve_bases()        ─── resolves bases (repo default plus tool fallbacks)
    ├─► generate_diffs()       ─── git2 diff with per-file stats (Patch API)
    ├─► checks::run_all()      ─── parallel checks (tsc, cargo, ruff...)
    ├─► heuristics::run()      ─── loctree (universal structural signals)
    └─► artifacts::generate()  ─── numbered layout + signal generators
```

## Modules

### cli/mod.rs

Uses `clap` with derive macros:

```rust
#[derive(Parser)]
pub struct Cli {
    pub target: Option<String>,
    pub bases: Vec<String>,
    #[arg(long)]
    pub quick: bool,
    // ...
}
```

### config/mod.rs

Converts CLI → Config and detects the project profile:

```rust
pub struct Config {
    pub repo_root: PathBuf,
    pub profile: DetectedProfile,
    pub execution_mode: ExecutionMode,
    pub run_tests: bool,
    pub run_lint: bool,
    // ...
}

pub struct DetectedProfile {
    pub kind: ProfileKind,  // Js, Rust, Python, Mixed, Generic
    pub has_package_json: bool,
    pub has_cargo: bool,
    pub has_pyproject: bool,
    // ...
}
```

Profile detection inspects the **actual source files**, not just the presence of
manifests:
- `has_js_source` = `.ts/.tsx/.js/.jsx` files under `src/`
- `Cargo.toml` → Rust (also checks `src-tauri/`, `*_rs/`)
- `pyproject.toml` → Python
- combinations → Mixed

A Rust project with a `package.json` for tooling (e.g. pnpm for dev tools) is
detected as `Rust`, not `Mixed`.

### git/mod.rs

A wrapper over `git2` (libgit2 bindings):

```rust
pub struct Repository {
    inner: git2::Repository,
}

impl Repository {
    pub fn resolve_target(&self, config: &Config) -> Result<ResolvedRef>;
    pub fn resolve_bases(&self, config: &Config) -> Result<Vec<ResolvedRef>>;
    pub fn generate_diffs(&self, target: &ResolvedRef, bases: &[ResolvedRef]) -> Result<Vec<Diff>>;
    pub fn commit_patch(&self, commit_id: &str) -> Result<String>;
}
```

Advantages of git2 over the `git` CLI:
- much faster for batch operations
- no subprocess spawning
- better error handling

### checks/mod.rs

A trait-based system:

```rust
#[async_trait]
pub trait Check: Send + Sync {
    fn name(&self) -> &str;
    fn can_run(&self, config: &Config) -> bool;
    async fn run(&self, config: &Config) -> Result<CheckResult>;
    fn cache_key(&self, config: &Config) -> Option<String>;
}
```

Implementations:
- `TypeScriptCheck` - `tsc --noEmit`
- `VitestCheck` - `vitest`
- `CargoCheck` - `cargo check`
- `ClippyCheck` - `cargo clippy`
- `CargoTestCheck` - `cargo test`
- `CargoAuditCheck` - `cargo audit`
- `SemgrepCheck` - Semgrep JSON scan; default is diff-scoped with
  `--baseline-commit <merge-base>` when the git baseline is clean and available,
  while `--security-full` keeps a full-tree scan
- `CargoGeigerCheck` - `cargo geiger`
- `RuffCheck` - `ruff check`
- `MypyCheck` - `mypy`
- `PytestCheck` - `pytest`

In standard execution mode, tests and lint are enabled by default, unless a
preset (`--quick`, `--update`, `--ai-only`) or an explicit `--skip-*` disables them.

### mcp/

The MCP server (`prview mcp`) is a thin contract adapter over the prview core.
It adds no review logic: tools spawn `prview` as a subprocess to produce a pack
and read truth back from storage. Every tool takes an explicit `repo` path,
every response carries `schema_version`, and every failure is fail-loud. See
`docs/mcp.md` for the tool reference.

### artifacts/mod.rs

The core artifact generator. Builds the numbered directory layout
(`00_summary/`, `10_diff/`, `20_quality/`, `30_context/`):

- Root: `PR_REVIEW.md`, `dashboard.html`, `artifacts.zip`
- `00_summary/`: `RUN.json`, `FAILURES_SUMMARY.md`, `MANIFEST.json`, `SANITY.json`, `MERGE_GATE.json/md`, metadata
- `10_diff/`: `full.patch`, `per-commit-diffs/` (batching + thematic labels), `per-file-diffs/` (hotspots)
- `20_quality/`: per-check `*.result.json` + `*.log`, `full-checks.log`, `checks-errors.log`, `coverage-delta.txt`, `BREAKING_CHANGES.md`
- `30_context/`: optional `INLINE_FINDINGS.sarif`, `changed-tests.txt`, profile-specific (`cargo-tree`, `tsc-trace`, `eslint`, `vitest`)
- `latest` symlink in the parent dir

### artifacts/signal/ (module directory)

Domain-specific signal generators, each producing an artifact **only when** it
has meaningful data. Originally a single 3400+ LOC `signal.rs` file, now split
into 16 focused modules under `src/artifacts/signal/`. The facade (`mod.rs`)
re-exports everything public, so callers continue to use `signal::*` unchanged.

#### signal/mod.rs — re-export facade

Declares all submodules and re-exports their public API via `pub use`. Adding a
new signal module requires only two lines here: `mod new_module;` and
`pub use new_module::*;`.

#### signal/common.rs — shared types and helpers (lexer)

Types and functions used across multiple signal modules:

- `ReviewFileCategory` enum (`Code`, `Test`, `Config`, `Asset`, `I18n`, `NonCode`)
- `classify_review_file(path)` — categorizes a file path by its role
- `is_non_code_file(path)` — convenience predicate (everything except Code and Test)
- `parse_patch_new_start(line)` — extracts the new-file start line from a `@@` hunk header
- `is_identifier_byte(byte)` / `contains_token_match(haystack, needle)` — word-boundary aware substring matching
- `HOTSPOT_THRESHOLD` constant (80 lines of churn)

#### signal/checks_log.rs — filtered error/warning extraction

- `generate_checks_errors_log(dir, checks)` — produces `checks-errors.log` containing only
  error/warning lines (with +/- 2 lines of context) from failed check outputs.
  Compilation noise (`Compiling`, `Downloading`, `Updating`) is filtered out.

#### signal/breaking.rs — breaking changes detection

Heuristic scan of diffs for API-breaking changes:

- `BreakingRisk` enum (`High`, `Medium`, `Low`) — publicness heuristic based on file path depth and barrel/re-export file detection
- `BreakingFinding` struct with `BreakingKind` (`RemovedSymbol`, `ChangedSignature`, `NewEnvRequirement`)
- `analyze_all_breaking_changes(patches)` — returns all findings from multiple patch texts
- `write_breaking_changes(dir, findings)` — writes `BREAKING_CHANGES.md` if findings are non-empty

Scans for removed `pub` symbols (fn, struct, enum, trait, type, const, static),
JS/TS `export` removals, signature changes (same function name with different params),
and new environment variable requirements. Only scans code files (not tests, config, docs).

#### signal/coverage.rs — coverage delta computation

Cross-references changed source files with test files to estimate test coverage:

- `CoverageSignal` struct — canonical single source of truth for all consumers (`dashboard.html`, `MERGE_GATE.json`, `PR_REVIEW.md`, text artifact)
- `CoverageDelta` struct — legacy wrapper with `from_signal()` conversion
- `CoverageFile` struct — a single changed source file with its matched test files and coverage state
- `CoveragePair` struct — a matched (source file, test file) pair with the match strategy used
- `compute_coverage_signal(diffs, repo_root, repo)` — the canonical computation function
- `generate_coverage_delta(dir, signal)` — renders `coverage-delta.txt` from a pre-computed signal

Four-strategy filename heuristic matching:
1. Exact stem match: `foo.rs` <-> `foo_test.rs` / `test_foo.rs` / `foo.test.ts`
2. Path-mirrored: `src/foo/bar.rs` <-> `tests/foo/bar.rs`
3. Sibling tests module: `src/foo/bar.rs` <-> `src/foo/tests.rs` or `src/foo/tests/*.rs`
4. Keyword overlap: `core/audio/chunker.rs` <-> `tests/e2e_audio_chunker.rs` (shared path segments)

Import-based recovery (strategy 5): for still-uncovered files, reads test file content
from the target commit and greps for import statements referencing the source module.
Uses word-boundary matching to avoid false positives.

Confidence downgrade: reports "medium" confidence when Rust files are uncovered
(inline `#[cfg(test)]` modules are a known blind spot) or when import recovery was used.

#### signal/diffs.rs — per-file diff generation

- `generate_per_file_diffs(dir, repo, diffs)` — creates `per-file-diffs/` directory with
  individual `.patch` files for every changed file, plus `00-INDEX.txt` index.
  Files exceeding `HOTSPOT_THRESHOLD` (80 LOC churn) are tagged `[HOTSPOT]` in the index.

Uses injective `~XX` path encoding (`sanitize_path`) so that different source paths
always produce different filenames (collision-free). The `00-INDEX.txt` maps encoded
filenames back to original source paths.

#### signal/ghost_refs.rs — dangling references to deleted files

Detects references to files that were deleted in the PR but are still mentioned in
the remaining working tree (imports, requires, documentation links, etc.):

- `GhostRef` struct — path of the referencing file, line number, and the deleted path it references
- `detect_ghost_refs(diffs, repo_root)` — collects all deleted file paths from the diff,
  then scans the working tree for lines that reference them. Produces `GHOST_REFS.json`
  if any dangling references are found.

Covers common reference patterns: Rust `mod foo;` / `use crate::foo`, JS/TS `import … from`,
Python `from … import`, and generic string literals. Only source files are scanned
(config, assets, and generated paths are skipped).

#### signal/risk.rs — per-file risk scoring

- `FileRiskScore` struct — path, numeric score, and contributing factor labels
- `compute_file_risk_scores(diffs, coverage, breaking)` — computes composite risk
  scores from multiple signals. Scoring factors: deleted (+20), security-path (+15),
  hotspot (+10), breaking (+10), uncovered (+5), has-test (-10).
  Returns top 10 files sorted by score descending. Test files are excluded.

#### signal/i18n.rs — locale key parity analysis

- `I18nDelta` struct — missing keys per locale, per-locale key counts, locale count
- `compute_i18n_delta(diffs, repo_root)` — discovers changed i18n files (in `locales/`,
  `i18n/`, `translations/` directories), reads all sibling locale JSON files from disk,
  flattens nested keys with dot notation, and reports keys missing from any locale.
  Returns `None` if no i18n files were changed.

#### signal/patterns.rs — risky pattern scanning

Scans added lines in diff patches for 11 risky patterns:

- `PatternHit` struct — file, line number, pattern name, context snippet, test_code flag
- `generate_pattern_scan(dir, diffs, repo)` — produces `PATTERN_SCAN.json` with per-pattern
  aggregation, prod/test split counts, and sample contexts

Scanned patterns: `unwrap`, `println`/`print`, `dbg`, `todo`/`FIXME`/`HACK`/`XXX`,
`@ts-ignore`/`@ts-expect-error`/`@ts-nocheck`, `eslint-disable`, `console.log`/`error`/`warn`,
bare `catch`, `unsafe`, `#[allow(...)]`, `as unknown as`/`as any`.

Full-file `#[cfg(test)]` / `#[test]` context seeding: reads the complete file at the
target commit and builds a set of line numbers inside test blocks, using string/comment-aware
brace counting. This correctly classifies additions inside pre-existing test modules
even when the `#[cfg(test)]` annotation is outside the patch hunk.

#### signal/public_api.rs — heuristic public API surface diff

Heuristic diff of the public API surface exposed by changed files:

- `PublicSymbol` struct — name, kind (`Fn`, `Struct`, `Enum`, `Trait`, `Type`, `Const`, `Static`),
  file path, and whether it was added or removed
- `compute_public_api_diff(diffs)` — scans added and removed lines in diff patches for
  `pub` symbol declarations, then pairs added/removed names to identify renames and
  signature changes. Produces `PUBLIC_API_DIFF.json`.

Only considers Rust `pub` symbols at this time. Non-code files (tests, config, assets)
are excluded. Results are aggregated per file and sorted by kind for readability.

#### signal/deps.rs — dependency manifest diffing

- `DepsDelta` struct — `added`, `removed`, `changed` dependency name lists
- `generate_deps_delta(dir, diffs, repo)` — reads full manifest files from both base
  and target commits, parses dependency sections, and diffs them. Produces `DEPS_DELTA.json`.

Supported manifests:
- `Cargo.toml` — `[dependencies]`, `[dev-dependencies]`, `[build-dependencies]`
- `package.json` — `dependencies`, `devDependencies`, `peerDependencies`, `optionalDependencies`
- `pyproject.toml` — PEP 621 (`[project] dependencies`, `[project.optional-dependencies]`) and
  Poetry (`[tool.poetry.dependencies]`, `[tool.poetry.group.*.dependencies]`)

#### signal/unsafe_audit.rs — new unsafe blocks + SAFETY comment scan

Scans diff patches for newly introduced `unsafe` blocks and checks whether each one
is accompanied by a `// SAFETY:` comment explaining the invariant:

- `UnsafeHit` struct — file path, line number, surrounding context, and `has_safety_comment` flag
- `audit_unsafe(diffs)` — iterates added lines in all patches, identifies `unsafe {` block
  openings, and checks the preceding lines for a `// SAFETY:` annotation.
  Produces `UNSAFE_AUDIT.json` only when new unsafe additions are found.

Rationale: undocumented `unsafe` blocks are a common review gap; surfacing them with
explicit pass/fail per-block makes reviewer attention tractable.

#### signal/consistency.rs — cross-artifact consistency checker

Compares key counters between `MERGE_GATE.json`, `report.json`, coverage, breaking
changes, and inline findings to detect mismatches that would erode trust in the pack.
This is the independent side of the cross-check: it recovers counters from the
already-serialized artifacts on disk and flags any disagreement.

#### signal/semantic.rs — semantic cross-file rules

Domain-aware finding generation backed by multi-file evidence. The first rule
detects delete flows (e.g. a DB record removal) that lack a corresponding resource
cleanup (file/storage/S3 artifact deletion).

#### signal/tauri_commands.rs — Tauri command surface

`generate_tauri_commands(...)` analyzes the Tauri command surface exposed by the
changed files, for Tauri (mixed JS + Rust) projects.

#### signal/test_helpers.rs — shared test fixtures (`#[cfg(test)]`)

Test-only module providing mock constructors used across all signal submodule tests:

- `mock_check(name, status, output)` — creates a `CheckResult`
- `mock_diff(files)` — creates a `Diff` with fixed base/target IDs
- `mock_file_change(path, status, adds, dels)` — creates a `FileChange`
- `make_test_repo(files)` — creates a temp git repo with two commits (base -> target)
- `make_diff_with_ids(base_id, target_id, files)` — creates a `Diff` with specified commit IDs

Compact review summaries (`PR_REVIEW.md`, `FAILURES_SUMMARY.md`, `AI_INDEX.md`) and
per-finding SARIF generation are orchestrated by `artifacts/mod.rs`, which calls into
the signal modules for data computation.

### artifacts/dashboard.rs

Generates `dashboard.html` — a visual summary of the PR with checks, findings, and
file stats.

### heuristics/

Structural code analysis:
- `loctree.rs` — universal heuristic (works with any profile): cycles, dead
  exports, unused symbols, exact twins across Rust/JS/TS/Python

### cache/mod.rs

Hash-based caching:

```rust
pub struct Cache {
    dir: PathBuf,  // $PRVIEW_HOME/cache/<repo>/ (default root: $HOME/.prview)
}

impl Cache {
    pub fn get(&self, check_name: &str, key: &str) -> Option<CachedResult>;
    pub fn set(&self, check_name: &str, key: &str, status: &str, output: Option<&str>);
}

// Key generation
pub fn rust_hash(repo_root: &PathBuf) -> String {
    // git_head_short + cargo_files_hash + src_files_hash
}
```

## Dependencies

| Crate | Use |
|-------|-----|
| `clap` | CLI parsing |
| `tokio` | Async runtime |
| `git2` | Git operations |
| `serde` / `serde_json` | Serialization |
| `rmcp` | MCP server (JSON-RPC over stdio) |
| `colored` | Terminal colors |
| `indicatif` | Progress bars |
| `rayon` | Parallel processing |
| `zip` | ZIP creation |
| `sha2` | Hashing |
| `toml` | TOML manifest parsing (Cargo.toml, pyproject.toml) |
| `anyhow` | Error handling |
| `async-trait` | Async traits |

## Extending

### Adding a new check

1. Create `src/checks/mycheck.rs`:

```rust
use super::{Check, CheckResult, CheckStatus};

pub struct MyCheck;

#[async_trait]
impl Check for MyCheck {
    fn name(&self) -> &str { "MyCheck" }
    fn can_run(&self, config: &Config) -> bool { ... }
    async fn run(&self, config: &Config) -> Result<CheckResult> { ... }
}
```

2. Register in `src/checks/mod.rs`:

```rust
mod mycheck;
pub use mycheck::MyCheck;

fn get_checks_for_profile(config: &Config) -> Vec<Box<dyn Check>> {
    // ...
    checks.push(Box::new(MyCheck));
}
```

### Adding a new signal generator

Signal generators live in `src/artifacts/signal/`. Each module owns one signal domain
and produces an artifact only when it has meaningful data to report.

1. Create `src/artifacts/signal/mysignal.rs`:

```rust
//! My signal — short description of the domain.

use anyhow::Result;
use std::path::Path;

pub struct MySignalData {
    // ...structured output
}

pub fn generate_my_signal(dir: &Path, /* inputs */) -> Result<()> {
    let data = compute(/* ... */);
    if data.is_empty() {
        return Ok(()); // No file = no noise
    }
    std::fs::write(dir.join("MY_SIGNAL.json"), serde_json::to_string_pretty(&data)?)?;
    Ok(())
}
```

2. Register in `src/artifacts/signal/mod.rs`:

```rust
mod mysignal;
pub use mysignal::*;
```

3. Call from `src/artifacts/mod.rs` in the appropriate numbered layout section and add
   status logging.

4. Add tests using `test_helpers`:

```rust
#[cfg(test)]
mod tests {
    use super::super::test_helpers::{mock_diff, mock_file_change};
    use super::*;

    #[test]
    fn my_signal_empty_diff() { /* ... */ }
}
```

### Adding a new profile

In `src/config/mod.rs`:

```rust
pub enum ProfileKind {
    // ...
    MyLanguage,
}

fn detect_profile(...) -> Result<DetectedProfile> {
    // Add detection
}
```
