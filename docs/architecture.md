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
    pub fn resolve_diff_bases(&self, target: &ResolvedRef, bases: &[ResolvedRef], quiet: bool) -> Vec<ResolvedRef>;
    pub fn generate_diffs(&self, target: &ResolvedRef, bases: &[ResolvedRef], quiet: bool) -> Result<Vec<Diff>>;
    pub fn commit_patch(&self, commit_id: &str) -> Result<String>;
}
```

Artifact diffs use `resolve_diff_bases()` before `generate_diffs()`, so the
review pack matches GitHub's three-dot "Files changed" model: branch names stay
displayable as bases, while each diff is anchored at the target/base merge-base.

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

#### Where checks run

Checks must judge the *reviewed* commit, not whatever happens to be checked out
locally. `plan_check_run()` resolves the working directory for every language
check: when the resolved target equals `HEAD` (the ordinary local review) it
returns `repo_root` unchanged; when they differ (`--pr`, `--remote`, or an
explicit target) it materialises a detached `git worktree` at the target commit
and returns that path. `node_modules` and `.venv` are symlinked into the
snapshot, so tests and linters keep their installed environment without a
reinstall.

The Python and JS checks (`Ruff`, `Mypy`, `Pytest`, `TypeScript`, `ESLint`,
`Vitest`, `Stylelint`) share **one** run-wide snapshot rather than each creating
its own — see `uses_shared_scan_dir()`. `SemgrepCheck` is the single deliberate
opt-out: it manages its own worktree because it also needs a baseline commit.

The Python checks add one step on top of that symlink. `uv run` synchronises the
project environment before executing, so a reviewed commit whose dependencies
differ from the local branch would install into — and remove packages from — the
operator's active `.venv` through the snapshot symlink. `plan_python_run()`
therefore sets `UV_PROJECT_ENVIRONMENT` to `Config::uv_env_dir_for()`
(`~/.prview/uv-env/<repo>/<target-sha>`) for off-`HEAD` runs: the reviewed
dependency set is still installed and judged, in a prview-owned environment kept
warm across runs. A local review sets no override and uses the checkout's own
environment exactly as before.

That environment is per reviewed **commit**, not per repository. `uv run` syncs
before executing and releases the environment lock while the child command runs,
so two concurrent prview processes reviewing different commits of one repo would
take turns installing incompatible dependency sets into a shared directory, each
resynchronising under the other's running pytest. Runs of the *same* commit still
share, which is the reuse that pays for the cache.

Per-commit directories are bounded, or a busy repository would leave a
virtualenv behind for every commit ever reviewed. Each run refreshes a
`.prview-used` marker inside the environment it uses (reuse only writes deep
inside, so directory mtime alone would understate activity), and
`prune_uv_envs()` drops what is outside the working set: the three most recently
used always survive, and nothing used within the last 24 hours is removed, so a
concurrent or slow review cannot have its environment deleted mid-run.

Age alone would not be enough, because the reviews are concurrent: one process
can read an environment's timestamp just before another refreshes it and then
delete the directory once that other review's `uv run` has begun. Marking and
pruning are therefore a single critical section, serialised across processes by
a `.prview-prune.lock` file at the root (the same atomic
create-new-plus-liveness lock the run index uses). The lock is opportunistic —
pruning is housekeeping, so a root held by another live review is left to it,
and this run only records its own use. A mark that lands outside the lock still
wins: each candidate is re-read immediately before it is removed.

Nothing is pre-created — uv rejects an existing directory that is not a valid
environment, so the directory tree only ever comes from uv itself.

The cargo checks (`Cargo check`, `Clippy`, `Rustfmt`, `Cargo test`,
`Cargo audit`, `Cargo geiger`) run in the snapshot as well, but with one extra
step. A snapshot is a throwaway temp dir, so its in-tree `target/` would force a
full dependency rebuild on every run — the reason these checks were originally
pinned to the local checkout. `plan_cargo_run()` instead points
`CARGO_TARGET_DIR` at `Config::cargo_build_cache_dir()`
(`~/.prview/cargo-target/<repo>`), a per-repo build cache shared across runs and
kept out of the operator's own `target/`. The variable is passed to the cargo
child process only; a local review sets no override at all and uses the working
tree's `target/` exactly as before. Cargo checks are already serialized on a
single-permit semaphore (`is_cargo_target_check()`), so they never contend for
that cache within a run; two concurrent prview runs on different commits rely on
cargo's own build-directory locking.

Which directory inside the snapshot is resolved from the **reviewed commit's
tree**, not from the local layout. `config.profile.cargo_root` describes the
LOCAL checkout, so `resolve_reviewed_cargo_root()` takes it only as the first
candidate and tries three in order: the mapped root, the repo root (a workspace
root still checks its members), and — for a crate the reviewed branch moved (a
root crate pushed into `backend/`, a member renamed) — exactly one directory
within two levels that carries a manifest of its own. Several such directories
resolve to nothing, with a reason naming what could not be chosen between: which
crate a review is about is not something to guess. Without this, a moved crate
left the run with nowhere to go and cargo's "could not find `Cargo.toml`" became
the reviewed crate's verdict.

A `cargo_root` configured **outside** the repository has no such mapping — a
snapshot of this repo can never contain it. Off-`HEAD` runs then **skip** the
cargo checks with a reason naming the unreachable root
(`unreachable_reviewed_cargo_root()`), instead of quietly analysing the
operator's unrelated checkout and filing the result under the reviewed commit.
No verdict is the honest answer where a foreign tree's verdict was the bug.

That refusal is lexical, and the reviewed commit controls the tree: it can turn
an in-repo root such as `backend/` into a **symlink** to an external directory,
which carries no `..` and passes the component check. Resolving from the git
tree closes that by construction — trees are not traversed through symlinks, so
such a root simply has no entries to find — and `contained_in_snapshot()` settles
containment on the resolved paths for the cases git cannot answer (an injected
scan dir, an unreadable repo), refusing rather than earning a verdict outside
the reviewed tree. Canonicalisation is the test only; the path itself is passed
through unchanged, so provenance keeps reporting the directory as the run saw it.

Whether cargo applies at all is decided by the **reviewed** commit, not by the
local profile. `config.profile.has_cargo` describes the checkout, so reviewing a
branch that dropped its last `Cargo.toml` from a Rust checkout used to run every
cargo gate and report cargo's own "could not find `Cargo.toml`" as the target's
verdict. Eligibility asks the same resolver — the snapshot carries exactly the
target commit's tree, so no worktree is materialised to answer it — and skips
with a reason when no candidate resolves. When git cannot answer at all
(unreadable repo, unresolvable ref) nothing is skipped: an unverifiable claim
may no more become a skip than a verdict.

A manifest must also be a **regular file**. Git stores a symlink as a blob, so a
`Cargo.toml` the reviewed commit replaced with a link to an external manifest
satisfied a plain tree lookup, resolved as a cargo root, and let cargo build a
foreign crate under the reviewed commit's cache key — the directory-symlink hole
one path component deeper. `regular_file_at_commit()` answers `false` for a
symlink, matching what manifest discovery already did, and the containment check
resolves the manifest alongside the directory for the paths git cannot reach.

Python eligibility follows the same rule for the same reason. `runs_python_checks()`
reads the local profile, so a target that removed its last `pyproject.toml` and
Python sources still scheduled the Python gates — and pytest exits 5 for "no
tests collected", a blocking failure for a target the check no longer applies to.
`missing_reviewed_python_project()` asks the reviewed tree: a `pyproject.toml`
settles it alone, otherwise the tree is walked for runtime Python source. That
walk is deliberately unbounded, unlike depth-limited cargo root discovery,
because it concludes *absence* — a bounded search cannot prove absence, only
manufacture confident false skips for deep layouts. Every step fails open.

Cache keys follow the same substrate. Cached results are looked up **before**
the shared snapshot exists, so cargo cache keys resolve the target commit
directly (`off_head_target_commit()`) and key on the commit id whenever it
differs from `HEAD` — otherwise a `--pr` run would hit an entry a previous local
run stored under a working-tree hash and serve the local checkout's verdict. The
commit is not the whole substrate, though: the same commit checked from the
workspace root and from a configured member yields different results, so the
repo-relative cargo root travels in the key beside it — the discriminator the
local hash path already carried.

A commit is a permanent key only for what the commit CONTAINS. When the
reviewed tree carries no `Cargo.lock`, cargo resolves the dependency graph as it
runs, against a registry that keeps moving, so a semver-compatible release can
change what builds while an entry keyed on the commit alone replays the old
verdict until eviction. `unlocked_substrate_stamp()` appends the day whenever no
lockfile is proven present (in the reviewed tree via git, in the working tree via
the filesystem) — the shape `Cargo audit` already uses for advisories, which age
the same way. Repeated runs within a session still hit; tomorrow's run resolves
again. A lookup git cannot answer counts as locked, so an unrelated failure does
not churn the key.

A cache key is a **file name**: `Cache::set` writes `<cache_dir>/<check>/<key>`
and creates only the check-level directory. The root therefore travels hashed
(`commit-<sha>-root-<hash>`, `-root-self` for the repo root) rather than
verbatim. A nested root such as `crates/core` used to put a separator straight
into the key, so every store targeted a directory nothing had created: the write
failed, the lookup missed, and the most expensive gates in the tool recomputed
on every review of a workspace member. The same encoding removes the colon these
keys carried, which is an illegal file-name character on Windows.

**Known limitation — submodules.** `create_worktree_snapshot()` runs
`git worktree add` only, so gitlink directories stay empty. A Cargo workspace
whose member or path dependency lives in a submodule therefore reports a missing
manifest in an off-`HEAD` review, even though the reviewed commit builds in a
checkout with its submodules initialised. Materialising them in the snapshot
means a `git submodule update --init` per run — network-capable, unbounded, and
writing into the superproject's module store while the operator works in it —
so it is deliberately deferred rather than smuggled into a review path.

#### Check provenance

Every check records a `CheckProvenance` alongside its result: `command`,
`tool_version`, `cwd`, `exit_code`, `started_at`/`finished_at`,
`hard_fail_signatures`, `cache_key`, plus the substrate it read:

- `target_sha` — commit whose tree the check scanned (the snapshot's detached
  commit, or the repo `HEAD` for an in-place scan).
- `tree_state` — which tree, and whether it still matches its commit:
  - `snapshot` — a tree materialised **from this repository's objects** at the
    reviewed commit, unmodified: the ephemeral worktree the language checks
    share, or the `git archive` extraction the Loctree heuristics scan;
  - `snapshot-dirty` — the same worktree after the run wrote into it (a
    generated `Cargo.lock`, a tool writing into the checkout), so the scanned
    bytes are **not** exactly `target_sha`. The dependency symlinks prview
    itself creates (`node_modules`, `.venv`) are excluded — they are the tool's
    scaffolding, not a change to the reviewed tree;
  - `snapshot-borrowed-deps` — the reviewed commit's tree, unmodified, but with
    its dependencies borrowed. `create_worktree_snapshot()` links the operator's
    `node_modules`/`.venv` into the snapshot instead of installing what the
    target's lockfile pins, so the reviewed **source** is exactly `target_sha`
    while the compiler, plugins, type definitions and runtime the tools loaded
    came from the local checkout. A dependency-changing PR is where those
    differ, so such a run is not reported as an exact snapshot scan. Reported
    only when the links actually exist **and the check reads them** — a repo with
    no local dependency tree links nothing and stays `snapshot`, and so does a
    cargo or Semgrep run in a mixed repository that merely happens to have
    `node_modules` linked. The JS checks (TypeScript, ESLint, Vitest, Stylelint)
    resolve their toolchain through `node_modules`; the Python checks resolve
    theirs through the per-commit `UV_PROJECT_ENVIRONMENT` prview points uv at,
    never the linked `.venv`. Installing the target's own dependencies instead is
    a network operation of unbounded cost and is not attempted;
  - `local-clean` — repo working tree, nothing uncommitted;
  - `local-dirty` — repo working tree with uncommitted changes — the scanned
    bytes are **not** exactly `target_sha`;
  - `foreign` — a directory that is neither this repository's working tree nor
    one of its worktrees. Being outside `repo_root` is not proof of a snapshot,
    and labelling a different checkout `snapshot` would certify its verdict as
    the reviewed commit's. Position does not settle it in the other direction
    either: repository identity is checked **first**, so a vendored checkout, a
    submodule or an in-repo symlink to another clone is `foreign` even though it
    sits below `repo_root` — `target_sha` there is the other project's `HEAD`.

Both are resolved from the directory the command actually ran in, by the single
`resolve_scan_substrate(cwd, repo_root)` helper, so a change in where a check
runs is reflected in its provenance without per-check bookkeeping. Both are
optional and additive: they are absent from packs generated before they existed,
and when the scan directory is not inside a git repository. `tree_state` is also
absent when the working-tree status cannot be read at all (an index lock, a
permissions error, a malformed repository) — an unverifiable tree stays visibly
unknown instead of being recorded as clean. They surface in
`20_quality/<gate>.result.json`, `20_quality/full-checks.log`,
`00_summary/RUN.json` (`checks[]`), `00_summary/PROVENANCE.json` and
`report.json` (`checks[]`).

**A check that errors keeps its substrate.** A command that times out or crashes
returns an error, and that row used to carry no provenance at all — null `cwd`,
`target_sha` and `tree_state` for exactly the rows a reviewer most needs to
place. The error path now reconstructs the directory the check was about to read
without materialising anything: the run-wide shared snapshot is already on disk,
and a review whose target is the checked-out `HEAD` reads the repo root. Two
absences stay absences rather than being filled: `command` is an explicit
`<no command recorded>`, and an off-`HEAD` check with no shared snapshot keeps a
`None` provenance, because its own worktree is gone by then and naming the local
checkout would name the one tree it was *not* reading.

**Cache hits carry provenance too.** When a check result is cached, its status,
output and provenance are stored as **one JSON entry** (`<key>` under the
check's cache directory) and replayed together on the next hit. The replayed
record describes the run that *populated* the entry — its `cwd`, `started_at`,
`target_sha` and `tree_state` — and `cached: true` on the result is what marks
it as a replay rather than a fresh execution. Without this the fastest runs
(all-cache-hit) were the only ones with no audit trail at all.

The entry is written to a `<key>.tmp-<pid>-<nanos>` staging file and published
with a single `fs::rename`, so a concurrent reader sees either the old entry or
the new one — never a result paired with another run's provenance. Staging files
are excluded from lookups and from `cleanup()`. Older packs wrote the status,
the `.log` output and the `.prov.json` provenance as three separate files; those
legacy triples are still read (so a warm cache survives the upgrade) and are
removed the first time the key is rewritten. An entry whose blob no longer
parses replays with no provenance instead of failing the run.

#### Pack-level provenance — `00_summary/PROVENANCE.json`

The per-check rows answer "what did *this gate* read". `PROVENANCE.json` answers
"what did *this pack* judge", once, for a reviewer holding only the artifacts:

- `target_sha` — commit whose tree the pack judges;
- `bases[]` — every baseline the pack's patches were generated from, as
  `{name, sha}` in diff order. Each `sha` is the **merge base** taken from its
  diff (`Diff.base_commit_id`), not the tip of the base branch. The two differ
  whenever the base moved ahead of the branch point, and the patch, the
  changed-file list and every diff-scoped gate are all computed against the merge
  base — so recording the tip would describe a comparison the pack never made. A
  multi-base run (`--base a --base b`) produces one patch per base, so it gets
  one row per base; with no diff at all the resolved base refs are the only
  baselines there are and fill the array instead;
- `base_sha` — the first entry's `sha`, kept for consumers that predate
  `bases[]`. It is derived from that array, so the two cannot disagree;
- `head_sha` — commit checked out locally (equal to `target_sha` for an ordinary
  local review, different under `--pr`/`--remote`);
- `worktree.clean` — whether the local tree had uncommitted changes, frozen
  **before** any check ran or artifact was written (R4-19). `null` when the
  status could not be read at all (an unreadable or malformed index): the two
  failure modes are not the same, and only one of them is safe to answer
  permissively. Without a git repository nothing can be uncommitted and such a
  run has no diff baseline either, so it stays `true`; a repository whose status
  simply could not be read establishes nothing, and reporting `true` there would
  both publish a fact nobody checked and let the pre-existing downgrade silence
  findings on a tree that was never inspected. The downgrade requires a proven
  `true`, so unknown suppresses it;
- `worktree.status_digest` — `sha256:<hex>` over a canonical rendering of the
  working-tree status, from the *same* read as `clean`. Each line is
  `XY <path>\0<content>`, where `<content>` fingerprints the file the entry
  points at: `blob:<len>:<sha256>` for a regular file (streamed, so a large file
  is not held in memory), `symlink:<sha256>` over the link target,
  `gitlink:<head>:<clean|dirty:<sha256>|unknown>` for a nested repository (git
  never recurses into one, so a submodule is a single status entry — its own
  `HEAD` and, when it is dirty, a recursive digest of its own dirty subset are
  what tell two of them apart; the recursion stops after three levels of
  nesting and falls back to a bare `dirty`), `dir` for an ordinary directory, `absent`
  when the path is gone, `unreadable` on an IO error. Paths alone
  identify *which* files are modified, not *how*; two runs that touch the same
  files with different content are different substrates and must not share a
  digest. Only the dirty subset is hashed. It is a stable fingerprint, not a
  capture of a specific `git status --porcelain` stdout;
- `checks[]` — one row per check: `{id, cwd, target_sha, tree_state, started_at,
  cached}`, with `null` fields for a check that produced no provenance. The
  synthetic `heuristics_loctree` row is included: Loctree runs in-process rather
  than as a subprocess (`command` is `loctree (in-process)`), but it still reads
  a tree — the `git archive` extraction of the target commit in snapshot mode,
  or `repo_root` when no snapshot could be made — and a gating signal whose
  substrate is unstated is unauditable.

The worktree state is frozen **per run**, and a `--watch` iteration is a run:
each iteration re-reads the working tree before its checks, so the pack it emits
describes the tree that iteration analysed rather than the tree as it looked
when the watcher started. (With an in-repo `--output-dir` this means later
iterations legitimately report *dirty* — they can see the previous iteration's
artifacts. The default output root is `~/.prview/runs`, outside the repo.)

The file is purely additive: no other pack file changed shape for it. It is
hashed by `MANIFEST.json` like any other artifact and listed in the sanity
`required_files` check.

#### Known limitation (deferred to 0.8)

Provenance is an **observation a check writes**, not a constraint the type
system enforces. The `Check` trait still hands each implementation a `&Config`
and trusts it to resolve its own directory and record what it read; nothing
prevents a new check from running somewhere and reporting something else, or
from returning no provenance at all. Making the substrate a *parameter* — a
`CheckContext` carrying the resolved scan dir and substrate, with `run()` unable
to look elsewhere — is the 0.8 cut. Until then the guarantee is "every check
reports where it ran", not "no check can run anywhere else".

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
- `00_summary/`: `RUN.json`, `PROVENANCE.json`, `FAILURES_SUMMARY.md`, `MANIFEST.json`, `SANITY.json`, `MERGE_GATE.json/md`, metadata
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
pub fn rust_hash(root: &Path) -> String {
    // Cargo.toml/Cargo.lock hash + Rust source hash, 16-byte digest segments
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
