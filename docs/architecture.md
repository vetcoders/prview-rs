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
│   ├── ledger/
│   │   └── mod.rs       # Task ledger: one record per unit of work in a run
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
│   │   └── dashboard/     # HTML dashboard generation (mod.rs, sections.rs, assets.rs, tests)
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

Cargo-audit baseline comparison covers vulnerability findings and the
informational warning families (`unmaintained`, `unsound`, `notice`, and future
warning keys). Its identity is `(advisory id, package, locked version)`: an
unchanged lock makes current advisories pre-existing without another tool run;
a changed lock is compared against `cargo audit` over the base revision's
effective lockfile from the same Cargo root as the live check: a member lock if
present, otherwise the workspace-root lock. If that base audit is unavailable,
current vulnerability findings remain unclassified rather than being inferred
from manifest deltas. A multi-base run with more than one changed effective
lockfile is likewise left unavailable rather than choosing a favorable base. A
malformed current report fails its check, has the distinct
`current-unavailable` baseline status, and cannot manufacture a clean or
resolved result.
The decision caveat always enumerates counts for `new`, `pre-existing`,
`resolved`, and `unknown-baseline`, plus an explicit baseline status
(`not-required`, `available`, `unavailable`, or `current-unavailable`).

Semgrep's `errors[]` remains a completeness signal independently of findings.
Path-like `path`, `location.path`, and span `file` fields are collected into a
stable deduplicated list and emitted in decision caveats. A partial parser run
therefore cannot look like a complete clean scan merely because `results[]` is
empty. The operator-facing caveat shows at most ten paths and reports the
remaining count; this display bound does not truncate the underlying error set.

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

One window stays open deliberately. Because the losing review marks outside the
lock, its mark can land between the sweeper's final re-read and its
`remove_dir_all`. Closing it would mean marking under the lock, i.e. every
off-`HEAD` Python review waiting on another process's housekeeping. The window
is one `remove_dir_all` wide, opens only for an environment that is idle for a
day, outside the working set, and being started at that instant, and its
consequence is a loud `uv` failure on one gate — never a verdict attributed to
the wrong substrate. Marking under a bounded-wait acquisition is the fix if that
failure is ever observed.

Nothing is pre-created — uv rejects an existing directory that is not a valid
environment, so the directory tree only ever comes from uv itself.

The tools read the project **files**, not the directory, so `plan_python_run()`
also refuses a `pyproject.toml` or `uv.lock` that resolves outside the tree being
judged — the Python counterpart of the Cargo manifest guards. A reviewed commit
that tracks either as a link to an external file would have ruff, mypy and pytest
configure themselves, and uv resolve dependencies, from another project, while
provenance recorded an exact `snapshot` scan and the cache filed the verdict
under the reviewed commit (`uv run` is given neither `--no-project` nor
`--locked`, so nothing downstream re-asks). Metadata linked to a real file inside
the tree resolves back inside and passes: escape is the target, not symlinks.

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

The lone survivor of that third step must also **prove it is the configured
project**: its `[package] name` — or, for a virtual workspace root that defines
no crate, its member list — has to match the manifest at the local cargo root.
Being the last manifest standing is not evidence of having moved. A commit that
deletes the Rust project while keeping an `examples/demo` crate within two levels
otherwise had every cargo gate run against the demo and file a green verdict for
a project that commit no longer contains — a commit that, checked out normally,
profile detection would not call a Rust project at all. No identity to compare
against (no local manifest, one that does not parse, one that defines neither)
is a skip with a reason, not a guess.

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
the reviewed tree. It resolves three paths, not one: the directory, its
`Cargo.toml`, and its `Cargo.lock`. Cargo follows a symlinked lockfile even
under `--locked`, so a reviewed commit tracking its lock as a link to an
external file had the entire dependency graph resolved from another project's
pins while the pack reported an exact `snapshot` scan. Canonicalisation is the test only; the path itself is passed
through unchanged, so provenance keeps reporting the directory as the run saw it.

What the contained manifest *declares* is the next step out.
`dependency_paths_stay_in_snapshot()` reads the resolved cargo root's manifest
and resolves every local `path` it names — dependencies, dev- and
build-dependencies, `[workspace.dependencies]`, `[target.*]`, `[patch]` and
`[replace]` — against the snapshot. An absolute path dependency, or a relative
one that climbs out or passes through a symlink, has cargo compile source the
reviewed commit does not contain while provenance reports `snapshot`, so the run
is refused with the dependency named. Only off-`HEAD` runs are held to this: a
local review is about the working tree as it stands, where a path dependency on
a sibling checkout is an ordinary setup and no claim is made about a commit's
contents.

`cargo check` at a workspace root builds its members, and a member declares its
own dependencies, so every manifest within three levels of the cargo root is
read the same way — a bounded directory walk that never enters a symlinked
directory (the snapshot links `node_modules` in) and skips `target/` and
`.git/`. A member manifest that is itself a link out of the snapshot is refused
with them. What the walk still does not cover is a member outside that subtree,
a `[patch]` in `.cargo/config.toml`, and anything a build script does: it
refuses what it can prove escapes rather than pretending to be complete, because
resolving the true graph means `cargo metadata`, a network-capable second
resolve for each of six gates.

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

Existence is not a pin, so the lockfile is also checked against the manifest:
every dependency the cargo root's `Cargo.toml` declares must already appear in
the lock's package list — renames (`package = "..."`) followed to the name the
lock records — **and** the locked version must still satisfy the requirement the
manifest asks for, parsed with `semver`, cargo's own parser. A target that adds
a dependency without regenerating `Cargo.lock`, or bumps `serde = "1"` to `"2"`
over a lock still pinning 1.x, sends cargo to the registry exactly as a missing
lock does — no cargo command here passes `--locked`, which is what would assert
otherwise — and now gets the same day stamp. The test deliberately
under-reports: it does not read a workspace member's own manifest. It can also
over-report, when a `[patch]` or `[replace]` redirects a dependency outside its
stated requirement. Under-reporting is the behaviour that was there before;
over-reporting costs one extra cache miss a day, so anything that does not parse
— manifest, lock, requirement or locked version — counts as covered.

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

**A check that skips itself after reading keeps its substrate too.** The error
fallback above covers the `Err` branch only, so a check that *handles* its own
failure and returns `Skipped` bypasses it. Cargo geiger does exactly that twice:
its ten-minute timeout is degraded to a skip rather than a gate error, and a
virtual workspace manifest is detected up front by a `cargo metadata` probe. In
both cases a cargo command has already read the reviewed tree, so the rows are
built from the directory it ran in — only `exit_code` stays `null`, because that
is the one thing genuinely unknown. A `null` substrate is reserved for the case
where nothing was read at all (semgrep, when the snapshot could not be
materialised).

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
  is not held in memory), `symlink:<sha256 of the link target path>:<fingerprint
  of what it reaches>` for a symlink — the link's own identity is the path it
  names, which is what git stores, but everything the checks read through it
  lives at the far end, so the resolved file is fingerprinted too (`dir` for a
  directory, which is never descended into because an absolute link can leave
  the repo, `absent` for a dangling link, `special` for a device or fifo, which
  is never opened) —
  `gitlink:<head>:<clean|dirty:<sha256>|unknown>` for a nested repository (git
  never recurses into one, so a submodule is a single status entry — its own
  `HEAD` and, when it is dirty, a recursive digest of its own dirty subset are
  what tell two of them apart; the recursion stops after three levels of
  nesting and falls back to a bare `dirty`), `dir` for an ordinary directory, `absent`
  when the path is gone, `unreadable` on an IO error. Paths alone
  identify *which* files are modified, not *how*; two runs that touch the same
  files with different content are different substrates and must not share a
  digest. The path itself comes from git's raw bytes, not from the UTF-8 view:
  a name that is not valid UTF-8 is written as `<non-utf8:<sha256 of the
  bytes>>` and its content is still read through an OS-native path, because the
  single placeholder they shared before collapsed every such entry onto one line
  with an `absent` body. Only the dirty subset is hashed, and the reading is
  **bounded**: one capture may hash 256 MiB in total (measured: ~1 s in a release
  build), shared across every entry and every nested repository it descends
  into. The digest is taken before the first check starts, so an untracked
  dataset or vendored bundle in the dirty subset would otherwise put gigabytes of
  reading in front of the run. A file that does not fit what is left is described
  instead of read, as `stat:<len>:<mtime nanos>` — a deliberately different word
  from `blob:`, because it is not a content hash; two runs where an oversized
  file changed while keeping both its size and its mtime do collide, which is a
  far narrower window than a constant "too big" marker that would have made every
  large file equal to every other. A refused read leaves the allowance intact, so
  the entries after a huge one are still hashed. Entries are ordered before their
  content is read, so which of them the allowance covers — and therefore the
  digest of an unchanged tree — does not depend on the order git reports them in.
  It is a stable fingerprint, not a capture of a specific
  `git status --porcelain` stdout;
- `checks[]` — one row per configured check: `{id, cwd, target_sha, tree_state,
  started_at, cached, skipped}`, with `null` fields for a check that produced no
  provenance. `skipped` is `null` for a check that ran and carries the reason for
  one that did not: a gate ruled out during eligibility (tests disabled, a tool
  absent) never reaches RUN.json's `checks[]`, and omitting it here too made a
  deliberate skip indistinguishable from a gate that was never part of the run.
  Those rows have every substrate field `null`, because nothing was read. Their
  `id` comes from the same canonical mapper as every executed row
  (`check_id_from_name`): a skipped gate is `tsc`, `cargo` or `tests`, never the
  slug of its display name, so a consumer can pair the skip with the gate it
  belongs to. `REPORT.json.checks_skipped[]` carries the same id. The
  synthetic `heuristics_loctree` row is included: Loctree runs in-process rather
  than as a subprocess (`command` is `loctree (in-process)`), but it still reads
  a tree — the `git archive` extraction of the target commit in snapshot mode,
  or `repo_root` when no snapshot could be made — and a gating signal whose
  substrate is unstated is unauditable.

The three check inventories are projections of the same policy evaluations,
but intentionally answer different questions. `00_summary/RUN.json.checks[]`
contains only checks that produced a `CheckResult`, so `outcome.checks_run` is
the number of executed (including cache-replayed and runtime-skipped) checks,
not the number configured. `00_summary/MERGE_GATE.json.checks[]` additionally
contains checks ruled out before execution, with their policy state and reason.
The legacy root `checks-status.json` is a compact id-to-status projection of
that same complete evaluation list. It never reruns eligibility while artifacts
are being written; doing so could report a different reason from the run that
actually happened. Pre-run skip provenance remains in `PROVENANCE.json` as
described above.

`MERGE_GATE.json` schema 2.3 adds a typed enforcement layer beside those stable
decision axes. `decision.enforcement_disposition` is emitted from effective
policy evaluations, legacy breaking ratchets, and the repo-backed Rust
`ApiDelta`; it never re-ranks canonical `PASS` / `CONDITIONAL` / `BLOCK` or
`allow_merge`. Consequently a pack with only warnings can remain a canonical
`PASS` while carrying `warnings_only`. Pure Rust API additions are neutral;
confirmed/potential breaking facts (when the existing escalation knob is on)
and unknown/degraded analysis raise `review_required`. The Rust backend keeps
using the language-neutral `RevisionFileSource` substrate described below, and
the legacy JS/TS analyzer remains unchanged.

The 2.3 proof is deliberately complete at the artifact boundary. Each check row
states execution, outcome, class, policy conclusion, confidence, merge impact,
severity, and blocking; `inline_findings` additionally states its effective
class and per-source enforcement disposition. Typed quality-failure details
prove the one legal pre-existing downgrade of a raw failure. The validator and
one shared CLI/MCP reader cross-check those relations, so a missing or
contradictory additive field becomes review-required uncertainty rather than a
warning-only permission. Older packs remain readable, but a schema through 2.2
cannot opt into the new warning exception.

Exit policy is an adapter after that read boundary. `prview gate --strict`
accepts `clean` and `warnings_only`, rejects `review_required` with exit 2, and
keeps `block` at exit 1; its explicit `--fail-on-warnings` lane rejects the
canonical warning tally as exit 2. Top-level `prview --ci` intentionally keeps
the historical quality/block contract (exit 1 for Block or failed quality),
with its own `--fail-on-warnings` opt-in. The two commands therefore share typed
facts and parsing, but not an accidentally conflated enforcement mode.

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

### ledger/mod.rs

A run-wide record of every unit of work it considered: one `TaskEntry` per task,
stating what was resolved (`Run` / `Cached` / `Skipped` / `NotApplicable`) and
under which `TaskKey`.

The key is deliberately *semantic*: `TaskKey { tool, substrate }`, where `tool`
is normalised through `check_id::check_id_from_name` and `substrate` is the pair
(`target_sha`, `TreeState`) the task read. That makes "the same tool on the same
tree" one key regardless of which surface asked for it — the TypeScript gate and
a `tsc` context artifact are one task, not two — without a second alias table
free to drift from `check_id`'s. Both substrate fields are optional, mirroring
`checks::ScanSubstrate`: an unresolved substrate stays visibly unknown rather
than being certified as anything.

`TaskLedger` is shared across a run's concurrent tasks by reference; each field
sits behind its own `Mutex` and no lock is held across an `await`.

The ledger also **owns** the run's shared target snapshot
(`set_shared_snapshot` / `scan_dir`). Materialising it stays the check
dispatcher's job, but the handle lives here because the ledger outlives every
stage: a snapshot parked in it is still on disk when artifact generation asks
where the reviewed tree is, instead of having been dropped with the frame that
created it.

The ledger observes; it never runs, skips or caches anything itself.

`App::run` builds one ledger per run and hands it to `checks::run_all` /
`run_all_with_events`, which record every check as it resolves: a cache hit as
`Cached`, an eligibility skip as `Skipped`, an execution as `Run` with the
duration the result reports. A check that ran is keyed on the substrate its OWN
provenance names — the tree it actually read — so no second resolution can
contradict it; a check with no provenance falls back to the run's resolved
substrate, and to unknown when that is unset too.

A cache replay is keyed on the substrate of the run REPLAYING it (the cache key
is content-derived, so a hit is an answer about the tree this run is reviewing)
while its `origin` names the tree the ORIGINAL execution read, taken from the
provenance stored beside the entry. The two are deliberately separate: the key is
what a later stage asks about, the origin is what makes the replay auditable. It
also carries `cache_age_secs`, the age of the entry it replayed (see
`cache/mod.rs`), so a gate reported as passing off a stored answer states how
stale that answer is.

Skips and replays are decided in the checks stage's *first pass*, which
necessarily precedes `share_target_snapshot` — the runnable set is what decides
whether a snapshot is materialised at all — so they are recorded under an unknown
substrate. `TaskLedger::set_substrate` therefore **adopts** them: every entry
still keyed on an unknown substrate is re-keyed onto the resolved one, because
they were this run's decisions about the tree this run went on to read. Only the
key moves; a replay's `origin` is never overwritten with the current run's
substrate. An unknown key survives only where the run genuinely resolved no
substrate (nothing needed a shared snapshot), which is what
`TaskLedger::lookup_tool`'s fallback still covers.

Context commands that actually execute are recorded too, by
`artifacts::context_artifacts::record_context_runs`, once the runtime knows their
duration: `Run` for anything that started (including a failure or a timeout — the
tool read the tree either way), `Skipped` for a command that never spawned. A
command is recorded under the GATE it stands in for when one exists (`eslint
json` → `ESLint`, `tsc trace` → `TypeScript`), so one tool cannot land under two
ids depending on which surface resolved it; a command with no gate counterpart
(`cargo tree`, `tauri info`, `npm sbom`) is recorded under its own label, slugged
by `check_id_from_name`. The plan site hands that identity over in
`ContextCmd::gate` rather than leaving the label to be reverse-engineered, so
there is no per-command alias table.

#### The `ledger` view in RUN.json

`RUN.json` carries the entries as an additive `ledger` object: `schema` (its own
counter, currently `1`) and `entries[]`, one row per task with `tool`, `kind`
(`check` / `context_artifact`), `lifecycle` (`run` / `cached` / `skipped` /
`not_applicable`) and `substrate` (`target_sha` + the same `tree_state` strings
`checks[].tree_state` uses). Each lifecycle adds only the evidence it has:
`duration_secs` for a run, `cache_age_secs` + `origin` for a replay, `reason` for
a ruled-out task.

Everything the pack already reported — `checks[].cached`, `context_artifacts[]`,
`context_commands[]`, the top-level `schema_version` — is untouched, so a
consumer that ignores `ledger` cannot tell the section exists. That is why the
pack's `schema_version` does not move (the precedent `CheckProvenance` set) and
why the view versions itself instead.

#### One tool, one execution per run

The artifact stage reads the entries back through `TaskLedger::lookup_tool`,
which answers "did a gate already do this work on this tree?" before a context
generator repeats it.

The context stage used to derive that answer from the *results* list
(`checks_ran_eslint` and friends), which can only report what reached a result.
A gate ruled out by a preset leaves no result, so absence read as a gap: a fast
remote-only run excluded the lint gate on purpose and the context stage then
spent 23 s linting the whole tree by itself (`PRV-CONTEXT-WORK-DEDUP`).
The ledger holds the missing half — what was
deliberately ruled out and why — so the decision moved there:

- a gate that **ran or replayed a cache** covers the artifact, which is recorded
  as `Cached` naming the substrate that execution read. A gate that ran and
  *failed* still covers it: the tool read the tree and reported, and a second run
  buys the same answer at the same price;
- a gate that was **ruled out** leaves the artifact unproduced, recorded with the
  gate's own reason — `Skipped` when the reviewed tree could run the tool anyway
  (this run chose not to), `NotApplicable` when it could not (no switch would
  change that);
- a tool **no gate decided on** is still generated by the context stage, which is
  the one case where compensating was ever right.

The `tsc` trace answers to the same rule: a deep run used to compile the
reviewed tree twice, once as the TypeScript gate and once as
`tsc --noEmit --traceResolution`, so the trace is now deduped against a gate
that already compiled the same tree. One exception stands — a gate that failed
with module-resolution errors still forces the trace, because there the second
compile answers a question the gate's own output cannot: which candidate paths
the compiler tried. `tauri info` has no gate to dedup against and keeps its
behaviour; what changed is that a deferred one records its reason in the ledger
too, so the run has one account of what it did not do rather than two.

Both sides resolve the substrate through `checks::resolve_scan_substrate` with
the same `consumable_scaffolding` set, so a gate and its artifact describe one
tree identically instead of differing on `tree_state` alone. A lookup falls back
to an unknown-substrate entry for the same tool, which is what a run that never
resolved a substrate of its own leaves behind; it never crosses two *known*
substrates, which would be evidence of different work.

#### One reviewed tree per run

`share_target_snapshot` (in `checks/mod.rs`) resolves the run's scan directory
once, points the checks' config clone at it via `scan_dir_override`, records the
resolved substrate on the ledger, and hands the ledger the snapshot handle.
`artifacts::generate` takes the ledger in its `GenerateInput` and resolves the
context generators' root as `ledger.scan_dir()`, falling back to
`config.repo_root`.

This is what keeps a pack describing ONE revision. `scan_dir_override` is set on
a *clone* of the config inside `run_all`, so `App::run`'s own config never learns
about the snapshot; before the ledger owned the handle, the worktree was also
deleted when `run_all` returned. A `--pr` run therefore had its gates judge the
reviewed snapshot while `30_context/*` was produced from whatever the operator
had checked out locally (`PRV-CONTEXT-SNAPSHOT-PROVENANCE`). Every context
command's cwd and every filesystem probe that decides which commands to plan now
read the reviewed tree; a local review resolves to the repo root, which *is* the
reviewed tree, so its behaviour is unchanged. Cargo context commands resolve
their directory through `checks::planned_cargo_cwd`, the same resolution the
cargo gates use, so a workspace member is not collapsed to the snapshot root.

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
- `20_quality/`: per-check `*.result.json` + `*.log`, `full-checks.log`, `checks-errors.log`, `coverage-delta.txt`, `PUBLIC_API_DIFF.json/md`, `BREAKING_CHANGES.json/md`
- `30_context/`: optional `INLINE_FINDINGS.sarif`, `changed-tests.txt`, profile-specific (`cargo-tree`, `tsc-trace`, `eslint`, `vitest`)
- `latest` symlink in the parent dir

#### Stale-cache caveats (`MERGE_GATE.json.stale_cache_caveats`)

A verdict can rest on evidence the run never produced. In the Vista dogfood run
(`PRV-CACHE-STALENESS`) a `Cargo audit` result replayed from a cache written
before a reboot co-authored a `BLOCK`, and the pack said only `cached: true` —
nothing named the age of the evidence.

`generate_merge_gate` therefore reads the run's ledger (the only place that
carries `cache_age_secs`, see [ledger/mod.rs](#ledgermodrs)) and, for every gate
row with BLOCKING influence on the verdict — policy conclusion `Block`, or a raw
`failed`/`error` status, which gates `quality_pass` — emits one entry per row
whose replay is older than `STALE_CACHE_CAVEAT_MAX_AGE_SECS` (7 days, a constant
in `src/artifacts/merge_gate.rs`; a CLI knob is a follow-up):

```json
"stale_cache_caveats": [
  { "check_id": "cargo_audit", "check_name": "Cargo audit",
    "cache_age_secs": 806400, "threshold_secs": 604800 }
]
```

The field is **WARN-ONLY and additive**. It sits at the top level, deliberately
outside `decision`: that object is closed by contract and every field in it ranks
the verdict, so a report ABOUT the pack must not live there. A stale replay
changes no verdict, no exit code, and no other field — pinned by
`the_stale_cache_caveat_moves_no_other_field`, which diffs the whole `decision`,
`checks`, and `inline_findings` of a stale run against a fresh one. A stale
PASSING row raises nothing: only blocking evidence is worth dating.

### artifacts/signal/ (module directory)

Domain-specific signal generators, each producing an artifact **only when** it
has meaningful data. Originally a single 3400+ LOC `signal.rs` file, now split
into 16 focused modules under `src/artifacts/signal/`. The facade (`mod.rs`)
re-exports everything public, so callers continue to use `signal::*` unchanged.

The language-neutral revision substrate is the one intentionally public seam:
`prview::artifacts::revision_source`. It binds every exact-tree or tracked
working-tree-overlay entry/read to explicit provenance. Overlay inventory is the
path-sorted union of the target tree and overlay-only paths reported by tracked
Git status; unrelated untracked paths are neither inventoried nor readable.
The 0.8 Rust language backend is exposed narrowly as
`prview::artifacts::api_surface`; its production comparison seam is exposed as
`prview::artifacts::api_delta`, while the rest of the signal facade remains
crate-private. `snapshot_rust_api(&dyn RevisionFileSource)` creates one
`RustApiSnapshot` only. It does not compare revisions, write artifacts, affect
policy, or replace the production diff scanner.

`RustApiSnapshot` carries the source provenance and path-sorted records for
library crates, parsed module variants, reachable module aliases, externally
reachable items, explicit reexports, and guarded typed unknowns. A crate or
module receives `RustSourceCertainty::Confirmed` only after its exact live
regular UTF-8 source has been read through `RevisionFileSource` and the complete
file has parsed successfully. Active recursion and completed source outcomes are
separate state: successful variants may be reused, while failed reads, UTF-8
decodes, and parses stay failed on every later lookup and can never manufacture
a crate or module. `Added` and `RenamedFrom` roots/modules consume the exact
revision bytes like other live entries; there is no checkout or HEAD fallback.
Evidence paths, source states, private origins, and provenance remain traceable
but are separate from external semantic identity.

Library discovery matches the exact `Cargo.toml` basename. It validates every
consumed Cargo field (`package.name`, `[lib]`, `lib.name`, `lib.path`,
`lib.proc-macro`, and `package.autolib`) instead of inventing defaults for an
invalid schema. Package and explicit library names must also be non-empty valid
Cargo/crate identifiers; a TOML string alone is not semantic validation. A
valid virtual workspace is non-crate; an implicit library is
admitted only when `autolib != false` and its live default `src/lib.rs` can be
read and parsed. Repository-relative paths are normalized fallibly: absolute,
prefixed, non-UTF-8, and escaping paths become manifest/source unknowns rather
than being remapped. Missing, renamed-away, deleted, non-regular, non-UTF-8,
unreadable, or parse-failed manifests and roots remain typed unknowns.

Reachability starts at each library root. Ordinary inline modules and
`mod foo;` files (`foo.rs` or `foo/mod.rs`) are walked as whole syntax trees.
Candidate selection distinguishes live (`Present`, `Added`, `RenamedFrom`)
sources from unavailable old-side states, and a recursion stack is distinct
from stable `(crate, source, module path, cfg guard)` variant identity. Safe,
single-literal `#[path = "..."]` modules are source-backed. The walker keeps
the physical declaring-file directory separate from the logical module
directory: a direct `#[path]` in `a.rs` uses the file's directory, ordinary
`mod child;` uses `a/`, and an inline module's own `#[path]` becomes the base for
its children. Conditional `cfg_attr(..., path = ...)` cannot be selected without
a feature/target matrix, so it emits a guarded path unknown and suppresses an
arbitrary default candidate. Malformed, multiple, escaping, or unavailable
paths are likewise unknowns. Both ordinary module
candidates, neither candidate, non-regular/read-failed sources, parse failures,
and actual active cycles are likewise explicit unknowns. Rust compiler behavior
for `#[path]` on non-`mod.rs` external modules may evolve; the backend records
the currently tested declaring-file rule and does not speculate beyond an
executable current-rustc result.

A textual `pub` item is external only when every enclosing module is reachable.
An explicit public reexport can expose an item or the public-edge descendants of
a public child below a private implementation parent. Every module declaration
retains its parent-relative visibility separately from absolute reachability;
therefore a private module cannot be reexported illegally and a private child
cannot leak through a legal alias. Positive admission and private veto are
separate proofs. A public declaration contributes only when its guards are
contained in the symbol's effective guard lineage. Any private proof for the
same module segment vetoes that positive unless `guards_proven_disjoint` proves
the regions disjoint; a different feature predicate is potentially overlapping,
not a disjointness proof. An overlap whose residual public region cannot be
represented emits guarded `AmbiguousReexport` and no broad `Confirmed` record.
A public Unix declaration may therefore remain visible beside a private Windows
variant, while public `feature = "a"` and private `feature = "b"` do not produce
an overbroad positive when both features can be active. Item aliases, internal
resolver-only module aliases, externally reachable module aliases (including
nested `self as alias`), constructors, and chains through `crate`, `self`, and
`super` resolve in one finite stable-set closure. Internal aliases never enter
the semantic snapshot.
Declaration and every intermediate reexport guard are merged before an alias
enters the graph. Relative/root candidates that source-level resolution cannot
select uniquely become `AmbiguousReexport`, not two positives. Module/type,
Value, and Macro candidates for one `use` leaf are resolved independently, so
success or ambiguity in one namespace cannot erase a valid sibling namespace.
Ambiguous module and symbolic alias identities become monotonic tombstones. The
resolver retains the complete set of normalized origins for each module-alias
identity and for each symbolic identity in Type, Value, and Macro independently.
It records every overlapping conflicting pair in deterministic order; each
pair's guards describe that pair's overlap, rather than combining unrelated
alternative origins into one false conjunction. A tombstoned identity remains
eligible for origin collection but cannot contribute a positive, so later
declarations complete the conflict component without resurrecting the alias.

Each new tombstone restarts projection from separately retained, source-backed
items and use edges, clearing all derived module aliases, symbolic aliases,
projected items, and reexports first. Consequently invalidation follows the
causal proof graph rather than the output path: a result renamed to `Other` or
through multiple aliases disappears when its ambiguous root disappears. The
old shared 128-pass constant is not part of the contract. Completion is bounded
by a measure derived from the finite non-glob use-leaf graph: a leaf can own one
module-alias identity and one identity in each of the three Rust namespaces,
and a continuing pass must add a monotonic tombstone or a previously absent
leaf-to-leaf derived relation. If implementation drift ever exhausts that
graph-derived budget without the explicit no-rebuild/no-progress postcondition,
the resolver clears every derived positive and emits typed `ResolutionLimit`
instead of returning a partial or silently truncated API.

Guard regions are assumed to overlap unless the backend has an explicit proof
of disjointness. The currently proved family is the tested Unix/Windows target
family split; different feature strings and composite `all`/`any` expressions
are not disjointness proof. Conflicting overlapping origins suppress all
positives for that external key and emit deterministic pairwise guarded
ambiguities for the complete conflict component. Malformed or
unsupported cfg syntax inherits all already-proved outer guards, emits
`CfgPredicate`, and suppresses the affected item/module/reexport. Documentation
and lint-only `cfg_attr` branches are semantic no-ops; conditional cfg, shape,
ABI, path, and transforming branches retain their distinct meaning. Globs, true
cycles, external/prelude paths, `include!`, and unexpanded macro-generated items
remain typed unknowns. A reachable `pub extern crate` is likewise retained as
guarded `UnsupportedExternResolution` until external/prelude resolution exists;
private or unreachable declarations do not create external semantic surface.
Semantic proof comparison includes the public unknown's kind, crate/module
location, exact evidence, and guards, while continuing to exclude source paths,
provenance, and private reexport target/origin spelling.

Item identity is `crate + external module path + Rust namespace + NFC external
name`. Value, type, and macro namespaces are separate. Tuple and unit struct
constructors also occupy Value; named-field structs remain Type-only.
`macro_export` is projected to the crate-root Macro namespace, with docs,
rustfmt, and lint attributes normalized away. Proc-macro crate exports use their
external macro/derive names only for public functions declared at crate root;
private or nested declarations become precise unknowns. Unresolved transforming
attributes are checked on modules, impls and associated items, foreign
blocks/items, macro declarations, and ordinary public projections. They
suppress every dependent positive claim and emit exact `MacroGeneratedItems`
evidence. Foreign functions/statics inherit the parent ABI, safety, and relevant
attributes.

Contracts are emitted from normalized `syn` ASTs. Function/default bodies and
private member types are excluded, while ABI, qualifiers,
generics/bounds/where clauses, return types, public fields with structural tuple
indices, enum variants/discriminants, trait headers and associated items, type
aliases, public constants/statics, and relevant attributes remain. Inherent
impls are collected independently of module reachability, resolve owners through
same-crate `self`/`super`/`crate` paths, and retain self type, specialization,
impl generics/bounds/where clauses, and impl/item attributes before projection
through every reachable type alias. Unprovable owners are typed unknowns.
Documentation, rustfmt, and lint-control attributes are recursively discarded;
shape/ABI attributes remain. Raw identifiers and NFC-equivalent identifiers
share semantic names. Nested `cfg`/`cfg_attr` use the same recursive sorted and
deduplicated `all(...)`/`any(...)` canonicalization as top-level guards, without
evaluating host configuration.

#### signal/api_delta.rs — revision-backed Rust API production truth (0.8)

The W2-01 backend compares each exact base and target
`RustApiSnapshot` exactly once. `ApiDelta` is the single typed owner of added,
removed, changed, relocated, visibility-changed, and unknown facts. Identity is
crate + external module path + Rust namespace + normalized name + compatible
cfg region. Pairing is one-to-one and conservative: ambiguous identities or
relocations become typed unknowns, an unknown snapshot region suppresses a
confirmed removal/change, and a proven relocation cannot also appear as an
addition/removal. Parsed ordinary declarations include private counterparts
only as evidence for a proven public/non-public transition; externally
reachable `items` remain the API surface.

Exact identity is grouped on both sides before any fact is consumed: only a
`1 ↔ 1` component may become a confirmed change, while wider components are
consumed as deterministic typed ambiguity, including one-sided duplicate
components before the final add/remove pass. Cfg-region changes are paired only
when the guards may overlap. The comparison reuses the snapshot resolver's
single conservative disjointness proof (currently Unix versus Windows), so
different feature guards remain potentially co-active. One shared pair-certainty
check tests both identities and both source paths against the unknown regions
from both revisions before any exact, cfg, relocation, or visibility fact can
be confirmed. A glob, include, source-parse, or other relevant unknown therefore
blocks a contradictory confirmed fact at either the source or destination.
Standalone unknown findings retain their source side, source path, and revision
provenance. Finding IDs preserve Rust identifier case and serialize the complete
semantic identity, including both sides' cfg regions, contracts, and typed
unknown provenance; legal ambiguous input is data, never an assertion failure.

`compare_rust_api_revisions` constructs snapshots only from the exact
`Diff.base_commit_id` and `Diff.target_commit_id` Git trees. It never reads a
checkout, working-tree overlay, or patch fallback. Duplicate exact base/target
OID pairs are coalesced in stable first-seen order before either snapshot is
built; distinct multi-base comparisons each retain their own revision evidence
and comparison-qualified finding ID.
`breaking_changes_view` and `public_api_diff_view` are pure deterministic
projections over the same delta. Their shared counts, IDs, confidence, evidence,
unknown reasons, and provenance therefore cannot drift through independent
re-pairing.

Production keeps the existing artifact filenames and old
`PUBLIC_API_DIFF.json` added/removed/changed rows, then adds the complete Rust
view under `rust_api_delta`. `BREAKING_CHANGES.json` is the lossless Rust view;
both Markdown files are presentations of that same data. MERGE_GATE and
`report.json` serialize the same view directly. Confirmed Removed, Changed,
Relocated, and VisibilityChanged facts use the existing `breaking_escalation`
knob; Added-only is informational. Unknown facts degrade confidence and require
review without changing policy defaults or becoming a confirmed break.

`tests/fixtures/api_surface/phase_a_parity.json` is the deterministic,
byte-locked v3 machine ledger for all 32 W0 positive/mutant cells. Its test
executes both legacy Rust analyzers on the same normalized repo-shaped patch,
executes the repo-backed delta on the exact base/head fixture trees, and keeps
the complete structured facts rather than polarity labels: multiplicity,
namespace, cfg, contracts, before/after sides, provenance, evidence,
confidence, and unknown reasons survive serialization. Five controlled cases
byte-lock include-macro, glob-reexport, source-parse, namespace/cfg
multiplicity, and zero-fact revision provenance.

Operator-row relationships are derived from executed structured observations:
3 genuine legacy blind spots, 6 inaccurate historical-fixture transfers, and
23 current W0 fixtures that match their declared mapping. The exact six
transferred mappings use stable typed scenario IDs and one patch helper shared
by the original legacy regression test and the parity harness. Each ledger row
stores the historical scenario's expected fact kinds separately from its full
actual legacy facts, as well as the distinct current W0 legacy and repo-backed
facts. This is the deliberately bounded historical proof surface; the remaining
23 matches are derived from current W0 actual-versus-declared data. Recommended
disposition and the actual Phase B product effect remain separate operator
decision fields. Historical regressions are executed, not silently re-baselined
by the W0 ledger.

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

Legacy diff scan retained only for JavaScript/TypeScript API changes and the
separate non-API Rust environment-requirement signal:

- `BreakingRisk` enum (`High`, `Medium`, `Low`) — publicness heuristic based on file path depth and barrel/re-export file detection
- `BreakingFinding` struct with `BreakingKind` (`RemovedSymbol`, `RelocatedSymbol`, `ChangedSignature`, `NewEnvRequirement`)
- `analyze_js_ts_breaking_changes(patches)` — legacy API facts after structural JS/TS-only filtering
- `analyze_rust_env_requirements(patches)` — added-line env markers only; cannot emit Rust API facts
- `write_breaking_changes_with_api(...)` — writes lossless Rust JSON and the compatible Markdown view

Production Rust public symbols no longer enter this diff-only API analyzer.
JS/TS `export` removals/signature changes remain here. Rust env requirements are
preserved by a dedicated parser because they are not API surface facts. Only
code files are scanned (not tests, config, docs). The shared patch boundary is
side-aware for renames: JS/TS→JS/TS keeps both sides, Rust→JS/TS keeps only the
added JS/TS side, JS/TS→Rust keeps only the removed JS/TS side, and a non-JS
pair is discarded. Quoted Git paths are decoded structurally before the
normalized legacy header is emitted. The decoded `diff --git` paths own section
identity: `---`/`+++` markers must match them exactly, `/dev/null` requires the
corresponding new/deleted-file mode, and a hunk without both markers is
discarded fail-closed. Marker-free metadata-only rename/copy and mode-only
add/delete sections remain valid.

Remove + re-add pairing (applies to every `pub` symbol kind above, not just
functions): when a declaration is removed and re-added for the same name and
kind, an identical declaration line is a diff artifact and yields no finding,
while a changed one yields a single `ChangedSignature` — never a removal plus a
silent re-addition. A re-add in a *different* file is a module move and becomes
`RelocatedSymbol`, which is reported but deliberately excluded from breaking
escalation.

The old Rust limitation below is historical only: a diff-only scanner could not
see an enum variant, trait method, or public struct field removed beneath an
unchanged item opener. Production Rust now reads both exact repository trees,
so those contracts participate in `ApiDelta`. The bounded declaration-line
logic that follows remains relevant only to the retained JS/TS legacy backend
and test-only historical compatibility fixtures.

Declarations are compared on their FULL text, continuation lines joined, up to
`MAX_DECL_CONTINUATION_LINES` (32) — a runaway bound for static bodies and
generated data tables, not a display width. The bound used to be eight lines,
which cut inside the real distribution of `pub` signatures: two long
declarations agreeing on their opener and first eight lines finalized to the
same truncated text and paired as an unchanged re-add, so a parameter, bound or
return type changed below the cut produced no finding at all.

A hunk interleaves two texts, and the accumulators reconstruct both: the before
side is context ∪ removed lines, the after side is context ∪ added lines. So a
`-` line never touches the added accumulator, a `+` line never touches the
removed one, and a context line EXTENDS whichever side still has a declaration
open. Ending both accumulators at the first line from the other side truncated
every declaration a patch edits in place — `pub fn f(` and a shared `x: u8,`
followed by `-old: u16,` / `+new: u32,` finalized to two identical openers,
paired as an unchanged re-add, and the parameter change was reported nowhere.
Context lines only ever CONTINUE a declaration; a `pub` item that first appears
on one is unchanged by the patch and must not open an accumulator, which keeps
the reconstruction inside the hunk that emitted it. The bound is unchanged:
`MAX_DECL_CONTINUATION_LINES` still caps growth, and a hunk header or a new file
still finalizes both sides.

Where a declaration ENDS is decided on a separate, comment-resolved view of the
same lines, fed through the pending declaration's own `SourceScanner` one
physical line at a time. The joined text has no line breaks, so scanning it as a
whole let a `//` on any continuation line comment out every line appended after
it: the closing `)` and the body `{` were never seen and the accumulator ran on
into the body, so a body-only rewrite came out as a phantom `ChangedSignature`.
Feeding line by line ends a `//` where it really ends while the scanner still
carries an open literal or `/* … */` across the continuation lines.

`BREAKING_CHANGES.md` renders those declarations into markdown tables, so every
cell carrying source text has its `|` escaped as `\|`. Rust states bitwise or,
patterns and closures with that character, and a declaration like
`pub const MASK: u32 = READ | WRITE;` written verbatim opened new columns —
the row rendered as garbage exactly where the declaration was interesting.
GitHub's table parser splits on unescaped pipes before any inline markup runs,
so a code span is no protection. The span itself is fenced by a backtick run
LONGER than any inside the cell, because a declaration may state a backtick of
its own — `pub const TEMPLATE: &str = r#"`value`"#;` — and a single-backtick
span ends at the first interior one, leaving the rest of the declaration to
render as prose. When the content itself begins or ends with a backtick the
fence carries one space of padding, which CommonMark strips back off.

Display text and comparison identity are separate. `BREAKING_CHANGES.md` and a
`ChangedSignature` show the declaration verbatim, comments included, because a
reader shown a change should see the source as written; pairing COMPARES a
comment-free view of the same lines. A comment inside a declaration is not part
of the API, and rewording one used to come out as a `ChangedSignature` — a
breaking-change claim about text no consumer can observe. That view keeps string
and char literals verbatim: a literal is code, so `pub const GREETING: &str =
"hello";` and the same line ending `"bye";` must stay different declarations.
`SourceScanner` therefore offers both resolutions — `code_only` for the
delimiter trackers, which want a brace inside a string silenced, and
`code_with_literals` for callers comparing source.

The identity keeps a physical line break only where the break is part of the
value — that is, where the previous line left a string literal open. A literal
spanning two physical lines otherwise compared equal to the same literal
rewritten with a space in it, and a changed public constant paired away as an
unchanged re-add. Everywhere else the break is layout and the lines are joined
with a space: keeping it there made `pub type Alias =` followed by `u32;` a
different declaration from `pub type Alias = u32;`, so a purely cosmetic reflow
was reported as a `ChangedSignature` whose "before" and "after" were the same
string. By the same rule a line contributing no code is dropped from the
identity — a comment-only line says nothing about the API — unless a literal is
open, where a blank line is a blank line in the value.

Whitespace at a line's edges follows the same rule, one edge at a time: the
leading edge is kept when the PREVIOUS line left a literal open, the trailing
edge when THIS line does. Lines reached the accumulator already trimmed, so
re-indenting the inside of a multi-line public constant produced two identical
identities and the changed value paired away as an unchanged re-add. Trimming
neither edge would be worse in the other direction — every reflow would become a
phantom `ChangedSignature`, and a trailing comment's leading gap would make `a:
u8, // x` a different declaration from `a: u8,// y`. The per-edge rule keeps
whitespace only where a literal is open across it, which is exactly where it is
part of the value: measured over the local crates.io registry, of 200,553
multi-line public declarations only 640 continuation lines sit at a literal edge
at all, and 272 of those carry edge whitespace the old view dropped. No
formatter re-indents inside a string literal, because that changes the program.

A declaration ends at a `;` or a body `{` outside its brackets. Square brackets
count for the same reason parentheses do: an array type states its length with a
`;`, as in `pub const TABLE: [u8; 2] = [`, and reading that as the terminator
finalized both diff sides at their identical opener — the changed values below
produced no finding at all. Measured over the local crates.io registry, 719
public `const`/`static` declarations in 126 crates open a multi-line initializer
on a line whose type carries such a `;`.

A `{` in type position is not that body brace. `pub type Alias = Buffer<{` opens
a const argument, and finalizing there truncated both diff sides to the same
prefix, so a changed const expression below — a different public type — paired
away as an unchanged re-add. The scanner tracks the generic argument list
itself, so a const argument that is not the first one — `Buffer<u8, {`, where
the brace follows a comma — is recognized as well; a const generic is rarely the
leading argument, which made that the common half of the construct. `<` opens a
list only directly after an identifier, a closing `>`, or a `:`, `->` never
closes one, and `<<` is consumed whole, because `<` is also the shift operator
and the 4,666 public `const`/`static` declarations in the local registry that
state a shift on their own line must still terminate at their `;`. Measured over
that registry (59,946 files, 2,025 crates, 4,354,142 public declaration lines),
argument-list tracking and the narrower `<{` sequence rule it replaced judge zero
lines differently.

The `:` in that rule admits the TURBOFISH spelling of the same construct.
`pub fn run() -> Buffer::<{` is a valid return type — rustc accepts `Type::<…>`
in type position without a warning — but its `<` follows a `:`, so the list went
uncounted, the const block's `{` read as the item's body opener, and both diff
sides finalized at that identical prefix. They paired as an unchanged re-add and
a changed const argument, which is a changed public return type, was reported
nowhere: the direction that HIDES a break. Only `:` joins the openers, never
whitespace, so a comparison is still not a list. The widening is verdict-neutral
where it is not needed: `::<` appears on 557 public declaration lines in the
registry and a `:` immediately before a `<` on exactly one — inside a string
literal, which the code-only view never shows this scanner — and running both
rules over all 4,334,018 public declaration lines produces zero disagreements,
because a turbofish that closes on its own line (`size_of::<T>()`) nets to the
same depth counted or ignored. What changes is the list left OPEN at end of line,
which the accumulator carries into the next one; two public declarations in the
registry wrap a turbofish that way today.

INSIDE that const argument the same characters are operators, so the generic
depth is frozen there. `pub fn run() -> Buffer<{ 1 < 2 }> {` counted the
comparison as another opener, the argument list's own `>` closed only that
phantom level, and the depth was still above zero at the item's real body brace
— which therefore read as a further const argument and swallowed the body,
turning a body-only rewrite into a phantom `ChangedSignature`. Freezing costs
nothing, because whatever a const block states about generics is balanced
against itself: a turbofish (`{ size_of::<u32>() }`) and a qualified path
(`Uint<{ <Self>::LIMBS / 2 }>`, the shape crypto-bigint carries) close what they
open. Those are also the only shapes the corpus holds — across 58,614 files,
61 declaration or field lines put a const argument's braces around a `<` or `>`,
43 of them a turbofish and 18 a qualified path or a shift, and none a bare
comparison. The previous rule survived all of them by cancellation, the block's
stray `>` closing the outer list and the outer list's `>` then finding nothing
left; the freeze reaches the same verdict by construction instead.

Nor is the `{` of an initializer that body brace. After a top-level `=` the item
states a VALUE and runs to its `;`, so `pub const LIMIT: usize = {` and
`pub const ZERO: Self = Self {` open an initializer, not an item body — and a
`;` inside it terminates a statement, not the declaration. Finalizing at that
brace truncated both diff sides to their identical first line, they paired as an
unchanged re-add, and a changed expression inside the block produced no finding.
Only a TOP-LEVEL `=` counts: inside a generic argument list one states a default
(`struct Foo<const N: usize = 4>`) or an associated type
(`impl Iterator<Item = u8>`), and both are followed by a body brace that must
still end the declaration; `==`, `=>` and the compound assignments are excluded
too. This is the widest of the brace rules by frequency — measured over the
local registry (58,586 files, 1,960 crates, 4,334,320 public declaration lines),
2,465 lines change verdict, every sampled one a public constant whose
initializer is a struct literal or block spanning several lines. What such a
declaration accumulates is still bounded by `MAX_DECL_CONTINUATION_LINES`.

Inline module names keep their raw-identifier prefix. `mod r#type` and
`mod r#match` were both recorded as `r`, so two namespaces looked like one and a
removal from the first was cancelled by an unrelated addition in the second.

Pairing is scoped: two declarations pair only when their declaration site and
their `#[cfg(…)]` guard may be the same. The site is the inline `mod` path AND
the `impl` owner, tracked on one stack because they answer one question — which
namespace does this item belong to? An associated `pub const VALUE` moving from
`impl A` to `impl B` in the same file used to be an exact pairing on file, kind,
name and text, with an empty scope on both sides, so `A::VALUE` disappeared from
the report along with the removal. The owner is recorded as TEXT — everything
before the body brace, whitespace collapsed — and nothing about it is parsed.
The asymmetry is the whole rule: two KNOWN and different owners never pair, but
an owner the hunk did not show stays unknown and pairs with anything, which is
the accepted limit for an unseen opener and is not narrowed here. Over 211
commits of this repository the reports are identical with and without the owner;
over 708 crates.io release pairs removals move 30,555 → 30,694 and signature
changes 53,938 → 53,805, so the change mostly reclassifies a real owner change
from "signature changed" to "`A::x` removed". Its limit is the mirror of the
`cfg` one: the same owner written with a different path qualifier reads as two
(40 of 2,784 blocked pairings, all in one crate), and resolving that would mean
parsing types. The guard tracker resolves comments away
before it reads anything, with one per-side scanner reset with the guard, so a
block comment is not syntax on either count: `/** … */` standing between the
`cfg` and the item it guards no longer reads as a new item and clears the guard,
and `/* ))) */` inside a wrapped predicate no longer balances the attribute
early. The guard TEXT keeps literals, because `#[cfg(feature = "a")]` and
`#[cfg(feature = "b")]` are different gates and a literal-dropping view would
make them one; the DELIMITER COUNT drops them, from a second scanner walking the
same lines in step. Counting a literal's brackets as syntax broke the tracker
both ways. A `)` typed inside a multi-line `#[doc = r#"…"#]` balanced the
attribute early, and the literal's remaining lines then read as ordinary items
and cleared the pending `cfg`. The reverse cost more: the counter carried its
literal state per LINE, so a literal opened earlier was forgotten and its own
closing quote read as an opener — `#[must_use = "… \` continued onto the next
line swallowed the `]` after `…"`, the attribute never closed, and everything
below it, the real `#[cfg(…)]` included, was absorbed as continuation. Measured
over the local crates.io registry, of 237,368 `cfg`-guarded attribute runs
reaching a public declaration, 8,793 wrap across lines, 90 carry a literal
spanning the break, 13 of those a raw string, and 9 balanced wrongly under the
per-line state — all 9 of the line-continuation shape, in `rustix` and
`wit-bindgen`.

Spacing follows the same split. Whitespace outside the literals is formatting —
`#[cfg(feature="a")]`, `#[cfg(feature = "a")]` and the same predicate wrapped
across four lines are ONE guard, and reading them as three would report removals
that never happened. Whitespace inside a literal is value: `--cfg 'api="a b"'`
and `--cfg 'api="ab"'` are different configurations, so the two attributes are
different gates. The tracker stripped it from the whole attribute text, literals
included, which made those two one guard and paired a struct that really left
one configuration with its re-add under another. The strip is now the scanner's
own dense view, so it cannot reach inside a value while normalizing layout.
This one is a fix by construction rather than by frequency: of 524,530 gating
attributes in the local registry only 3 carry whitespace inside a value literal,
and none of them collide. Both directions still cost what they always did, and
the hiding one is not worth leaving open for a one-line rule.

Those were three findings of one shape — a delimiter, a space, a line break —
so the rule is stated once as an INVARIANT rather than patched a fourth time:
**bytes inside a literal traverse the whole attribute pipeline verbatim.** It
holds by enumeration, not by testing shapes, because the pipeline alters text in
exactly two places and both defer to the same `ScanState`. The delimiter count
runs on `code_only`, a view with no literal bytes in it at all, so trimming or
counting there cannot reach a value. The guard text runs on
`code_with_literals_dense`, whose only subtractive rule sits in the one `scan`
arm reached solely outside every literal and comment; each literal is emitted as
an unmodified slice. Everything downstream — `gates_the_item`, the sort, the
dedup, `cfgs_may_pair` — compares whole strings and transforms nothing. So the
line's raw bytes now reach the tracker (trimming at the caller ate a
continuation's indentation before it could be asked whether it was inside a
value), no `.trim()` survives inside the tracker (after the dense view there is
no whitespace left outside a literal, so a trim there could only eat value), and
the physical break is joined with a `\n` exactly when the previous line left a
literal open — gluing it unconditionally made `#[cfg(api = "a\nb")]` the same
guard as `#[cfg(api = "ab")]`. Measured like its siblings and just as rare: of
568,128 `cfg` attributes in the local registry, 4 carry a literal spanning a line
break, 2 of them gate an item, and none collide.

The guard is the WHOLE conjunction of
the attributes stacked above the declaration, sorted — `#[cfg(unix)]
#[cfg(feature = "x")]` and `#[cfg(windows)] #[cfg(feature = "x")]` are different
guards, while reordering the same two is not. An unseen scope or guard is
`None`, which pairs with anything: the diff may simply not have re-emitted the
context line on that side.

An attribute is read to its balanced close, not to the end of its first line. A
predicate wrapped as `#[cfg(any(` + feature lines + `))]` is one guard, equal to
its single-line spelling — whitespace and line breaks are formatting, not a
different gate. Reading only the opener left the continuation line looking like a
new item, which cleared the guard and let a declaration that really disappeared
for one configuration pair with its re-add under another. Any other wrapped
attribute (`#[derive(…)]`) is carried the same way so it cannot take the `cfg`
above it down with it; an attribute that never closes within
`MAX_ATTRIBUTE_CONTINUATION_LINES` falls back to the tolerant `None`.

`cfg_attr` counts as a guard exactly when it applies a `cfg`:
`#[cfg_attr(feature = "a", cfg(unix))]` gates the item as surely as `#[cfg(unix)]`
does, so it joins the conjunction, while `#[cfg_attr(unix, derive(Debug))]` decides
an attribute ON the item and stays out — inventing a gate there would split an
ordinary re-add into a phantom removal. The two families separate cleanly on the
whitespace-stripped substring `,cfg(`: of the 44,562 `cfg_attr` attributes in the
local crates.io registry, 189 apply a `cfg` (12 crates, the `portable-atomic`
idiom) and none of them carry that substring inside a string literal.

Reordering operands INSIDE one predicate is an accepted limit. Stacked
attributes are sorted, so `#[cfg(unix)] #[cfg(feature = "x")]` pairs either way
round, but `#[cfg(any(unix, windows))]` rewritten as `#[cfg(any(windows, unix))]`
is compared as text, does not pair, and reports a phantom `RemovedSymbol` under
an untouched declaration. Normalizing it means canonicalizing arbitrarily nested
predicates — a `cfg` parser — and the measurement says the parser would not earn
its risk. Across 708 consecutive-version pairs in the local registry, whole
releases and so far wider than any diff this scanner reads, 32 `cfg` attributes
were reordered at all, in 2 crates; across the 393 patch-level bumps among them,
the closest available proxy for a PR-sized change, zero. Of the 32, only 6 are
reachable by sorting an attribute's direct operands: the dominant real shape is
`not(any(a, b, c))`, with the reorder one level down. A bounded sort would close
a fifth of an already absent class while looking complete, which is worse than a
limit written down — and the error direction here is the tolerable one, a
phantom removal being visible in review rather than a real removal pairing away
in silence.

Both the module path and the perf tracker's test-context scope are counted over
CODE only, via the shared scanner in `src/rust_source.rs`. It resolves comments
and literals in ONE pass — `"http://x"` is a string and `format!("{}/*.{}")` is
a glob, not a comment — and carries an open `/* … */` **or an open string
literal** across lines, so a brace inside a multi-line template or JSON fixture
never reaches a delimiter tracker as syntax. Every raw form is recognized —
`r`, `br` and `cr`, with any hash count — because an unrecognized raw opener is
worse than an unknown token: its body is then read as code, and the first
interior `"` opens a phantom literal. That state is per side and per hunk: a
hunk boundary is where contiguity ends, and every consumer resets there.

What OPENS that context has to be provable, not merely suggestive. The marker
set is an exact `#[cfg(test)]`, a `#[cfg(all(…))]` with `test` among its
operands — which cannot hold unless `test` does — `#[test]` / `#[tokio::test]` /
`#[rstest]`, and
`mod tests`. Reading the bare token `test` anywhere inside a `cfg` predicate
instead made `#[cfg(not(test))]` — code compiled into every build EXCEPT the
test one — open test context and silently drop the production hits beneath it,
and did the same for `#[cfg(any(test, feature = "bench"))]`, which compiles
outside the test build whenever the feature is on, and for
`#[cfg(feature = "__internal-test")]`, a feature that merely has `test` in its
name. Measured over the local registry (58,586 files): of the 11,030 attributes
the old pattern read as test context, 83.62% are exactly `cfg(test)` and 6.76%
are `all(…, test, …)`; the remaining 9.62% are the ones it got wrong. `all` is
commutative, so the operand's position carries no meaning and reading only the
first one made `all(feature = "bench", test)` production while
`all(test, feature = "bench")` was test context; accepting it anywhere adds 72
attributes over that registry and removes none. The operand must be a DIRECT
one, so nothing before it may open a nested predicate — `all(not(test), …)`
proves the opposite of itself and `all(any(test, …), …)` proves nothing. That
also drops `all(not(windows), test)`, an under-detection kept deliberately
rather than growing a paren-matching parser. Everything
unproven is production, because the two errors are not symmetrical — an
unrecognized test context costs one extra finding a reader can dismiss, while a
claimed one that does not hold deletes a production finding nobody ever sees.

The pattern describes a complete `#[…]`, so it is matched against a complete
one. Attributes wrap — rustfmt breaks a long predicate over several lines — and
running the pattern per physical line meant a wrapped
`#[cfg(all(` / `feature = "bench",` / `test` / `))]` matched on no line at all,
leaving a test-only item read as production. An `AttributeAccumulator` joins the
lines of one attribute and matches once, on the line that closes it, counting
brackets on the same comment- and literal-resolved view the rest of the scan
uses so a `]` inside a string or a trailing comment cannot close it early. It
only ever CONTINUES: a wrapped attribute is bounded by
`MAX_ATTRIBUTE_CONTINUATION_LINES` (8), and one that never closes is dropped
rather than allowed to swallow the rest of the hunk — nothing was proven, and an
unproven gate is production. The shape is rare: 10 occurrences over the same
58,614-file registry, all of them genuine `all(test, …)` gates. It is a P2
because its error direction is the mild one — a phantom finding a reader can
dismiss, not a muted production hit.

An attribute's brackets are its own, and the brace scan skips them by tracking
attribute depth per character. `#[rstest]` stacked with
`#[case(Case { id: 1 })]` carries braces that belong to the attribute, never to
the annotated item, and letting them through made the `{` a body opener whose
`}` closed the test context on the same line — the test function below then read
as production. The plain shape survived by luck, because the attribute's `[` and
`(` hold `sig_depth` above zero; two clamping `>` comparisons
(`#[case(1 > 0, 2 > 1, Case { id: 1 })]`) drive it back to zero first and the
brace lands where the opener is accepted. Skipping the attribute outright
removes the class instead of the one shape that reaches it. This is separate
from the line-level `AttributeAccumulator` above and deliberately so: that one
answers "is this LINE part of an unterminated attribute" for the marker match,
while the brace scan needs "is this CHARACTER inside one". The depth persists
across lines, since attributes wrap; literals are already resolved away, so a
`]` inside a string cannot close one early.

The perf tracker's test context closes two ways, because not every test item has
a body. One that opens a brace closes when that brace balances again; one that
does not — `#[cfg(test)] mod tests;`, `#[cfg(test)] use crate::helper;` — closes
at the `;` ending the item the marker annotates. Waiting for a brace that never
comes left the context open for the rest of the hunk, and every production loop
and query below it was recorded as test-only and dropped from the signal.

Which brace opens that body is decided against the signature's bracket nesting,
not by taking the first `{`. A brace in type or pattern position —
`fn run() -> Buffer<{ LIMIT }>`, or the extractor idiom
`fn handler(Parameters(Req { field }): Parameters<Req>)` — balances before any
body exists, so reading it as the opener made the very next line look like the
item closing again: the context ended at the signature and the whole test body
was classified as production. Inside a signature `<` is reliably a generic
opener — but only where one can be: a `<` counts as opening a generic when it
FOLLOWS what it parameterises (`Buffer<`, `Vec<`, `fn f<`, `::<`), and a `<`
after whitespace is a comparison. `->` is excluded so a return arrow is not read
as a closing angle bracket; closers stay unconditional and the depth is clamped
at zero, so a `<` this rule misjudges — like a hunk starting mid-signature — can
only end the context early, never hold it open and mute production code.
Measured over the local crates.io registry: of 1,697,077 `fn` signatures, 1,191
carry a brace in that position and 715 place the body opener on a later line —
the shape that actually breaks the tracker — 59 of them test-annotated.

Spacing is where that rule stops being a boundary, so the boundary is drawn
around it: signature tracking is FROZEN inside a brace opened within those
brackets. A const argument holds an expression (`Buffer<{ 1 < 2 }>`) and a
destructured parameter holds a pattern, and in both `<` and `>` are operators.
The spacing heuristic reads the spaced spelling correctly and the compact
`Buffer<{1<2}>` — the same type, formatted without spaces — wrongly, because `<`
after a digit is indistinguishable from `<` after an identifier. Counting that
comparison left the depth stuck above zero, the real body brace read as another
type-level brace, and the context never closed, muting every production hit
after the test — the direction that HIDES work. Freezing costs nothing, because
what such a brace states about generics closes what it opens. Measured over the
same registry, of the 618 `fn` signatures whose brackets hold a brace, 6 put a
`<` or `>` inside it, all of them the qualified path
`Uint<{ <Self>::LIMBS / 2 }>` as crypto-bigint writes it, and none the compact
comparison. Those 6 reached the right verdict before only through the clamp —
the path's `>` closed the outer list, and the outer list's own `>` was then
clamped away — and now reach it by construction.

The item's own top-level `=` is the second such boundary, and by frequency the
larger one. After it the item states a VALUE, so both angle characters are
operators, and tracking is frozen for the rest of the item. A body-less item ends
at its `;` — the close that tests whether the signature's brackets are balanced —
so a counted comparison there could not be undone by anything: the context stayed
open and every production hit below the test was recorded as test-only, which
over-detects test context and HIDES work. `#[cfg(test)] const ENABLED: bool =
1<2;` is the reported shape, but the corpus idiom is the compact SHIFT, because
this tracker (unlike the declaration scanner) has no rule consuming `<<` whole.
Measured over the local registry on the same code-only view the tracker reads,
excluding lines with lifetimes, which this model cannot lex: of 2,206,540
single-line `const`/`static`/`type` declarations ending at their own `;`, 1,069
left the bracket depth stuck open under the old rule — dominated by
`const Reverse = 1<<8;` as objc2 generates its bitflags — and 64 still do. Those
64 are not a residual bug but the protection working: an array type wrapping to
the next line (`pub static X: [[u16; N];`) must hold its depth open so that `;`
is not mistaken for the end of the item.

#### signal/coverage.rs — coverage delta computation

Cross-references changed source files with test files to estimate test coverage:

- `CoverageSignal` struct — canonical single source of truth for all consumers (`dashboard.html`, `MERGE_GATE.json`, `PR_REVIEW.md`, text artifact)
- `CoverageDelta` struct — legacy wrapper with `from_signal()` conversion
- `CoverageFile` struct — a single changed source file with its matched test files and coverage state
- `CoveragePair` struct — a matched (source file, test file) pair with the match strategy used
- `compute_coverage_signal(diffs, repo_root, repo)` — the canonical computation function
- `generate_coverage_delta(dir, signal)` — renders `coverage-delta.txt` from a pre-computed signal
- `format_coverage_pct(Option<u32>)` — the one renderer for the percentage; `None` becomes `not measured`

**Unmeasured is not 100%.** `coverage_pct` / `CoverageDelta::pct` are
`Option<u32>` and are `None` whenever no changed source file was evaluated
(`total_source_files == 0`). Consumers must render that as "not measured" or
omit the coverage surface entirely — never as a percentage. A real `0/N`
(N > 0) stays a genuine `0%` measurement. In `report.json`,
`quality.coverage.heuristic_ratio` is `null` in the unmeasured case and is
paired with `measured: false` + `not_measured_reason`. That nullability — with
the loctree counters becoming omittable for the same reason — is why
`report.json` carries `schema_version: "2.0"`: a decoder written against `1.0`,
where the ratio was always a number, does not parse every pack.

`report.json`'s `gate.quality_failure_details[]` mirrors `MERGE_GATE.json`'s
`decision.quality_failure_details[]` field for field — `name`, `classification`
and `origin` (`"failure"` or `"warning"`). The origin is what makes
`introduced_quality_failures: ["Rustfmt"]` and `quality_pass: true` readable
together: the arrays admit warning-level baseline signals so the pre-existing
downgrade can be computed for them, and only a `"failure"` origin can fail the
quality gate. Emitting it in the gate artifact but not in `report.json` left the
two artifacts of one run disagreeing about what "failure" meant. The field is
additive and `report.json` stays `schema_version: "2.0"` — that major is
unreleased, so no consumer has ever seen a 2.0 without it.

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
  Returns every risky file sorted by score descending; presentation consumers
  may truncate that list, but `RiskHeatmap.total_risk_score` and the risk level
  always aggregate the full deduplicated PR surface. The original diff path is
  retained as the join key even when the displayed path is normalized. Test
  files are excluded.

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

Scanned patterns: `unwrap`, `println`/`print`/`eprintln`/`eprint`, `dbg`,
`todo`/`FIXME`/`HACK`/`XXX`,
`@ts-ignore`/`@ts-expect-error`/`@ts-nocheck`, `eslint-disable`, `console.log`/`error`/`warn`,
bare `catch`, `unsafe`, `#[allow(...)]`, `as unknown as`/`as any`.

Every needle is matched with word boundaries, and each side is bounded only
where the NEEDLE has an identifier edge — that is the only side a longer
identifier can swallow it from. `todo!(` is already right-bounded by its `(`
but must be left-bounded, so `mytodo!(…)` is not a TODO marker; `.unwrap()`
starts with `.` and must NOT be left-bounded, or `value.unwrap()` would stop
matching. Bounding only needles made entirely of identifier characters left
`todo!(`, `dbg!(`, `println!(`, `console.log(`, `unsafe {` and `as any` on raw
substring matching — the last of which reported every `has any` in a doc
comment as a type cast. The `eprint` family is listed explicitly because it used
to be caught by accident: `eprintln!(` CONTAINS `println!(`.

Full-file `#[cfg(test)]` / `#[test]` context seeding: reads the complete file at the
target commit and builds a set of line numbers inside test blocks, using string/comment-aware
brace counting. This correctly classifies additions inside pre-existing test modules
even when the `#[cfg(test)]` annotation is outside the patch hunk.

#### signal/public_api.rs — hybrid public API artifact writer

The writer preserves the legacy top-level JSON fields while embedding the full
repo-backed Rust `ApiArtifactView` additively. Rust findings are projected only
for old-reader compatibility; the embedded view is authoritative. Legacy
analysis receives JS/TS patch sections only.

- `PublicSymbol` struct — name, kind (`Fn`, `Struct`, `Enum`, `Trait`, `Type`, `Const`, `Static`),
  file path, and whether it was added or removed
- `compute_public_api_diff(diffs)` — scans added and removed lines in diff patches for
  `pub` symbol declarations, then pairs added/removed names to identify renames and
  signature changes. Produces `PUBLIC_API_DIFF.json`.

Confirmed Rust facts include namespace, cfg, before/after contracts, source
paths, provenance, confidence, evidence, and stable IDs. JS/TS remains a bounded
text heuristic. Non-code files (tests, config, assets) are excluded.

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

### artifacts/dashboard/

Generates `dashboard.html` — a visual summary of the PR with checks, findings, and
file stats. Split across `mod.rs` (layout/orchestration), `sections.rs` (panel
rendering), `assets.rs` (embedded CSS/JS (system font stack)), and `tests.rs`/`trends_tests.rs`.

### heuristics/

Structural code analysis:
- `loctree.rs` — universal heuristic (works with any profile): cycles, dead
  exports, unused symbols, exact twins across Rust/JS/TS/Python

**A zero-file scan is a skip, not a clean run.** Loctree can report
`available: true` while `summary.total_files == 0`. Every consumer treats that
as SKIP: `MERGE_GATE.json` and `20_quality/heuristics_loctree.result.json` emit
status `skipped`/`SKIP`, and `report.json`'s `quality.heuristics` emits
`status: "skipped"` with a `skip_reason` and omits `dead_exports`, `cycles`,
`twins`, and `unused_symbols` rather than writing zeros that read as results.

**A disabled scan is not a broken scanner.** `--quick` and `--no-heuristics`
short-circuit `heuristics::run_all` to a default result, which the caller still
passes on, so `report.json` used to describe an intentional skip as
`skip_reason: "loctree analysis unavailable"` — a tool failure that never
happened — and hand the reader a `log_path` pointing at a zero-filled stub. The
three skips are now distinguishable in `quality.heuristics`:

| `skip_reason` | meaning | `total_files` | `log_path` |
|---|---|---|---|
| `heuristics not run` | not asked for (`--quick`, `--no-heuristics`) | absent | absent |
| `loctree analysis unavailable` | asked for, scanner failed | present | present |
| `loctree scanned no files` | ran, measured nothing | `0` | present |

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

A hit also reports `age_secs`: how long ago the entry was published, read from
the entry file's mtime — an entry is published by a single `rename`, so its mtime
IS the moment the result became readable. Nothing changed on disk to carry it, so
a cache warmed by an older prview reports its age too (the legacy layout keeps
its status file as the entry, and the same mtime answers). `None` when the age is
unknowable — no metadata, or a timestamp in the future after a clock moved
backwards — never a fabricated zero. The age travels with the replay into the
ledger's `Cached` state and out through `RUN.json`'s `ledger` view as
`cache_age_secs`; it deliberately does NOT enter `CheckResult`, since it is a
property of the entry, not of the check's verdict.

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
