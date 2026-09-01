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
│   ├── governor/
│   │   └── mod.rs       # Bounded execution: weighted budget + child registry
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
supervise_startup_stage(…) ─── temporary startup governor owns Config probes
    │
    ▼
Config::from_cli(&cli)    ─── resolves PR metadata and builds Config
    │
    ▼
App::from_config(config)  ─── opens Repository + creates the run governor
    │
    ▼
with_cancellation(app.run(), run governor)
    │
    ├─► resolve_target()       ─── resolves the target branch
    ├─► resolve_bases()        ─── resolves bases (repo default plus tool fallbacks)
    ├─► generate_diffs()       ─── git2 diff with per-file stats (Patch API)
    ├─► checks::run_all()      ─── parallel checks (tsc, cargo, ruff...), bounded
    │                              by the run's ResourceGovernor
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
- root or nested `tsconfig*.json` declares a JS/TS product component, except
  under fixture, `node_modules`, `target`, or `vendor`; generic directory names
  such as `build` and `dist` remain valid package names when they contain a
  project config
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

`share_target_snapshot()` decides whether that snapshot is materialised at all,
and the condition is **not** "some gate needs it". It is:

> a runnable check is in `uses_shared_scan_dir()`, **or** the reviewed target is
> off-`HEAD` (`off_head_target_commit()`).

The second arm exists because the gates are not the only stage that reads the
tree: the context stage plans and produces the whole of `30_context` from
`ledger.scan_dir()`. An off-`HEAD` run can have nothing snapshot-backed to run —
for example, the fast remote-only preset, where those gates skip and only
semgrep remains, or a profile whose complete runnable set has sound cache hits.
TypeScript, ESLint, Stylelint, Ruff and Mypy do not currently take that path:
they opt out of persistent replay and remain runnable. Tying materialisation to
the runnable set left an empty-run case with no scan dir, so
`cargo tree`, the SBOMs, `tauri info` and the entry-point probes read the
operator's local checkout while the diffs and `MERGE_GATE.json` described the PR's
commit, and `RUN.json` looked identical either way
(`PRV-CONTEXT-SNAPSHOT-PROVENANCE`). A warm all-cacheable `--pr` run therefore
pays for one `git worktree` its gates do not need: a correct pack outranks a
saved checkout.

When the target **is** the checked-out `HEAD` and no runnable check wants a
snapshot, nothing is materialised — there the repo root genuinely is the reviewed
tree, and the artifact stage's fallback to `config.repo_root` is the right answer.
The call therefore sits *outside* the dispatcher's "anything to run" guard, since
a run with an empty runnable set is exactly the case it exists to cover.

Once either arm requires a snapshot, materialisation failure aborts the run.
Letting each check create an independent temporary tree would not give the
later context and artifact stages a verified root; their fallback to
`config.repo_root` could then combine the target commit's gate results with the
operator checkout's context in one pack. A missing snapshot is therefore a
provenance failure, not an optimization failure.

Materialising also resolves the run-wide substrate
(`ledger.set_substrate_keyed`), which adopts the first pass's skips and cache
replays off the unknown substrate they were necessarily recorded under. That is
the quiet half of the same bug: a warm `--pr` run used to report its own
decisions as being about no particular tree. The run-wide substrate is resolved
with an **empty** consumable-scaffolding list, so it reports `snapshot` and never
`snapshot-borrowed-deps` — with no command to name, nothing at that point can
consume the linked `node_modules`; a command that does resolve through the link
reports that for itself.

The adopted ENTRIES are keyed differently, and must be: they each name a tool,
and the substrate a later stage computes for that tool is that tool's own reading
of the tree. `adopted_substrate` re-keys each one through
`consumable_scaffolding(entry.key.tool)`, caching one `git status` per distinct
consumable set (there are two, and the run-wide resolution seeds the first).
Filing them all under the run-wide key instead put a JS repo's ESLint skip under
`snapshot` while the context stage went on to resolve `snapshot-borrowed-deps`
for the same directory: the exact-key lookup missed, `lookup_tool`'s
unknown-substrate fallback had just been spent by this very adoption, and the
context stage read `Uncovered` and re-ran the gate's work in full — on both
scenarios the shared snapshot exists for. `consumable_scaffolding` therefore
normalises its argument through `check_id_from_name`, since a ledger entry
carries only the id (`tsc`, `tests`) and never the display name.

The Python checks add one step on top of that symlink. `uv run` synchronises the
project environment before executing, so a reviewed commit whose dependencies
differ from the local branch would install into — and remove packages from — the
operator's active `.venv` through the snapshot symlink. `plan_python_run()`
therefore sets `UV_PROJECT_ENVIRONMENT` to `Config::uv_env_dir_for()`
(`~/.prview/uv-env/<repo>/<target-sha>`) for off-`HEAD` runs: the reviewed
dependency set is still installed and judged, in a prview-owned environment kept
warm across runs. A local review sets no override and uses the checkout's own
environment exactly as before.

The cold `uv sync` pre-step is resolved only after the run-wide target snapshot
exists, through that same `plan_python_run()`. Its cwd and
`UV_PROJECT_ENVIRONMENT` are therefore identical to the later gates; it never
syncs an off-HEAD dependency set into the operator checkout. uv download/build/
install pools and Cargo-backed PEP 517 builds inherit the run's child limit.
`CARGO_BUILD_JOBS` is then resolved through the direct Cargo gate's exact-cwd
configuration path, so a reviewed repository `[build].jobs` value remains a
ceiling for both uv and directly launched Python tools.
Before exporting higher-precedence `UV_CONCURRENT_*` values, the plan reads the
project-scoped authority selected by uv's explicit/discovery precedence: an
in-tree `UV_CONFIG_FILE`; otherwise, when boolish `UV_NO_CONFIG` is enabled, no
discovered uv configuration; otherwise `uv.toml`, then `[tool.uv]` from
`pyproject.toml`. An explicit config remains authoritative together with
`UV_NO_CONFIG`, matching uv. Each pool takes the
minimum of that project ceiling, inherited environment, and the run plan;
malformed, unreadable, non-UTF-8, wrong-type, or non-positive authority fails
closed. User- and system-level uv config remains outside this deliberately
project-scoped resolver. The cap remains on every later `uv run`, so an
unsuccessful pre-sync cannot retry outside the envelope. A direct Ruff, Mypy,
or Pytest fallback selected because uv is unavailable skips uv-only files and
environment selectors altogether; it still contains the generic Python
metadata it reads and retains the Cargo descendant cap. Before collection, a
plugin-disabled, null-config `--version` probe uses the same pytest launcher,
reviewed cwd, and bounded environment as the real check. Its actual major and
minor select the pytest 6.0-7.1, 7.2-8.0, 8.1-8.x, or 9.x discovery contract;
unsupported versions fail closed instead of guessing.
Pytest is then explicitly bound to the one highest-precedence config inside the
reviewed root (including pytest 9 TOML and hidden variants), or to an empty
config when the root has none, so it never walks into an ambient parent project.
Pytest 7.2-8.x recognizes `.pytest.ini` as a candidate but does not select an
empty hidden file unconditionally; that behavior begins with pytest 9. The
versioned discovery model preserves this distinction instead of treating every
recognized basename as an automatic winner.
Existing but unreadable, non-UTF-8, malformed, or conflicting recognized config
is an execution error, not absence. Pytest-xdist gets the same upper bound
through its auto-worker environment and a final CLI override only when the
effective shell-tokenized config/environment request exceeds that bound or is
dynamic; an explicit smaller count or zero remains unchanged. A standalone
`--` in config or inherited addopts fails closed because it would turn the
later isolation and worker-cap arguments into positional values. Xdist's custom
and proxy gateway options (`--tx` and `--px`) also fail closed: those paths can
create execution environments independently of `-n`, so a numeric override
cannot prove the run-wide child bound. Unknown build
backends remain one serialized Exclusive parent; the governor does not claim to
discover every third-party backend's private thread knob. Pytest itself also
remains Exclusive: arbitrary project `conftest.py` code and third-party plugins
can create private processes or mutate xdist hooks, which a portable parent
cannot infer or truthfully claim to cap.

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
also refuses a `pyproject.toml`, discovered `uv.toml`, or `uv.lock` that resolves
outside the tree being judged — the Python counterpart of the Cargo manifest
guards. A reviewed commit
that tracks either as a link to an external file would have ruff, mypy and pytest
configure themselves, and uv resolve dependencies, from another project, while
provenance recorded an exact `snapshot` scan and the cache filed the verdict
under the reviewed commit (`uv run` is given neither `--no-project` nor
`--locked`, so nothing downstream re-asks). Metadata linked to a real file inside
the tree resolves back inside and passes: escape is the target, not symlinks.
An enabled `UV_NO_CONFIG` removes only discovered `uv.toml` from that boundary;
`pyproject.toml` and `uv.lock` remain independently consumed metadata. Invalid
or non-UTF-8 boolish values fail loud, as uv would. Ambient `UV_CONFIG_FILE`,
`UV_PROJECT`, `UV_WORKING_DIR`, and legacy
`UV_WORKING_DIRECTORY` are checked at the same boundary. A config file may stay
inside the tree, but a project or working-directory redirect must resolve to the
exact reviewed root; prview reports a planning error instead of silently
neutralizing an operator setting and certifying a different execution.

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

A check returns no cache key when its complete effective input set cannot be
proved. TypeScript, ESLint, Stylelint, Ruff and Mypy currently opt out: their
answers depend on config, ignore rules, plugins and installed tool/dependency
state beyond the former source-only hashes. They still participate in same-run
`Run` to `Reused` context dedup; only persistent cross-run replay is disabled.

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
  synthetic `heuristics_loctree` row is included: Loctree cache creation runs in
  a private governed worker (`command` is `prview loctree worker (internal)`),
  while snapshot interpretation stays in-process. It reads a tree — the `git
  archive` extraction of the target commit in snapshot mode,
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
when the watcher started. Watch mode uses the default unique allocator for each
immutable iteration pack; combining `--watch` with one explicit `--output-dir`
is rejected instead of reusing or overwriting that path. The default output
root is `~/.prview/runs`, outside the repo.

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
created it. Because the artifact stage reads it too, an off-`HEAD` target is on
its own enough to materialise one, whether or not any gate had to run — see
*Where checks run*.

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
substrate. `TaskLedger::set_substrate_keyed` therefore **adopts** them: every
entry still keyed on an unknown substrate is re-keyed, because they were this
run's decisions about the tree this run went on to read. The new key is asked for
per entry, by tool id — one tree does not have one identity, and a snapshot that
carries a `node_modules` link is `snapshot` to a cargo gate and
`snapshot-borrowed-deps` to a JS one. The knowledge of which is which stays in
`checks` (`consumable_scaffolding`) and is handed in as a closure; the ledger
holds no table of tools. Only the key moves; a replay's `origin` is never
overwritten with the current run's substrate. An unknown key survives only where
the run genuinely resolved no substrate (nothing needed a shared snapshot), which
is what `TaskLedger::lookup_tool`'s fallback still covers.

Admission is not itself proof that a target command executed. If a non-security
check is admitted but its launcher returns a missing/unlaunchable-tool error,
its no-command provenance is recorded as `Skipped` with both queue timestamps.
A tool that did start, inspect the substrate, and later returns a runtime
`Skipped` result remains `Run`; status text alone cannot erase live coverage.

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
counter, currently `2`) and `entries[]`, one row per task with `tool`, `kind`
(`check` / `context_artifact`), `lifecycle` (`run` / `cached` / `reused` /
`skipped` / `not_applicable`) and `substrate` (`target_sha` + the same
`tree_state` strings `checks[].tree_state` uses). Each lifecycle adds only the
evidence it has: `duration_secs` for a run, `cache_age_secs` + `origin` for a
replay, `origin` for a same-run reuse, `reason` for a ruled-out task.
`queue_wait_secs` is emitted when both `queued_at` and `started_at` exist — the
gap between entering the budget queue and admission — so a slow tool is not
confused with a long resource wait.

Everything the pack already reported — `checks[].cached`, `context_artifacts[]`,
`context_commands[]`, the top-level `schema_version` — is untouched, so a
consumer that ignores `ledger` cannot tell the section exists. That is why the
pack's `schema_version` does not move (the precedent `CheckProvenance` set) and
why the view versions itself instead.

The context runtime also retains an internal admission fact. Two commands may
both display `timed_out` in the stable `context_commands[]` contract, but only
the one that actually spawned becomes ledger lifecycle `run`; a command whose
shared stage deadline expired while it was still queued becomes `skipped` with
the explicit pre-spawn reason.

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

- a gate that **ran** covers the artifact, recorded as `Reused` naming the
  substrate that live execution read. A gate that **replayed a cache** covers it
  as `Cached`, with the stored entry's age and the substrate of the ORIGINAL
  execution. A gate that ran and *failed* still covers it as `Reused`: the tool
  read the tree and reported, and a second run buys the same answer at the same
  price;
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

That fallback is valid only when the reviewed target is the checked-out
`HEAD`, where both paths name the same tree. If an off-`HEAD` snapshot is
required but cannot be created, the checks dispatcher returns an error before
pre-sync or gate execution; it never publishes a mixed-revision pack.

This is what keeps a pack describing ONE revision. `scan_dir_override` is set on
a *clone* of the config inside `run_all`, so `App::run`'s own config never learns
about the snapshot; before the ledger owned the handle, the worktree was also
deleted when `run_all` returned. A `--pr` run therefore had its gates judge the
reviewed snapshot while `30_context/*` was produced from whatever the operator
had checked out locally (`PRV-CONTEXT-SNAPSHOT-PROVENANCE`). Every context
command's cwd and every filesystem probe that decides which commands to plan now
read the reviewed tree. Static Tauri discovery, its source walk, and the
repo-relative mapping used to compare head commands with the base commit use
that same tree and repository view. A local review resolves to the repo root,
which *is* the reviewed tree, so its behaviour is unchanged. Cargo context
commands resolve their directory through `checks::planned_cargo_cwd`, the same
resolution the cargo gates use, so a workspace member is not collapsed to the
snapshot root.

### governor/mod.rs

Concurrency used to be decided per stage: the checks stage picked its own
fan-out, the context stage picked another, and nothing held the machine-wide
number. Two stages each behaving reasonably still oversubscribe a laptop, and the
tools are not equal — `cargo clippy` and `cargo test` each want the whole box
while reading a manifest costs nothing.

`ResourceGovernor` is that missing number, plus the registry that makes a run
killable. One governor per run, owned by `App` (and by `tui::run_tui`, which is
its own entry point) so BOTH stages that put load on the machine draw on the same
budget:

```rust
pub enum Weight { Light, Heavy, Exclusive } // semantic declaration

impl ResourceGovernor {
    pub fn new() -> Self;                                  // safe default
    pub fn for_resource_budget(ResourceBudget) -> Self;    // safe | balanced
    pub fn with_budget(total: u32, heavy_cost: u32) -> Self;
    pub fn plan(&self) -> ResourcePlan;                     // parent + child envelope
    pub fn cost(&self, weight: Weight) -> u32;
    pub async fn acquire(&self, weight: Weight) -> Result<GovernorPermit, Cancelled>;
    pub fn try_acquire(&self, weight: Weight) -> Option<GovernorPermit>;
    pub fn register_child(&self, key: impl Into<String>, pid: u32) -> bool;
    pub fn unregister_child(&self, key: &str);
    pub fn cancel(&self);
    pub fn cancelled_signal(&self) -> tokio::sync::watch::Receiver<bool>;
    pub async fn cancelled(&self);
    pub fn is_cancelled(&self) -> bool;
}

// Attribute a task's spawned children to a governor without threading it
// through `Check::run`.
pub async fn with_child_scope<F: Future>(g: Arc<ResourceGovernor>, label: &str, f: F) -> F::Output;
pub fn register_active_child(pid: u32) -> Option<ChildRegistration>;
```

- **Weights cost what the governor says.** `Light` is cheap metadata work;
  `Heavy` is a whole-project tool with an explicit descendant-worker cap;
  `Exclusive` consumes the entire budget for unsupported or unbounded child
  pools. The safe default therefore serializes every whole-machine tool.
- **The default is intentionally conservative.** `--resource-budget safe` uses
  one parent permit and one child worker. The opt-in `balanced` plan admits at
  most two capped heavy parents, never creates more parent permits than detected
  logical cores, caps each supported child pool at four, and caps the logical
  permit envelope at eight even on a large host. A one-core host therefore stays
  single-parent and single-worker. A one-minute load at or above `0.75/core` (or
  an unavailable load reading) backpressures a requested balanced run to the safe
  plan. This is a CPU/memory envelope, not a claim that future peak memory can be
  predicted exactly.
- **Python descendants use the same plan.** The pre-sync and Python gates share
  one reviewed snapshot and per-commit uv environment. uv pools, Cargo-backed
  package builds, and pytest-xdist are clamped to the child-worker limit. A
  third-party PEP 517 backend with its own undocumented pool is still serialized
  at the parent level but is not falsely advertised as internally capped.
- **A permit is held, never released by hand.** `GovernorPermit` returns the
  permits on drop, so an error path cannot leak budget.
- **Cancellation closes the semaphore.** That refuses a newcomer and a task
  ALREADY waiting alike — a task parked on the budget is work that has not
  started. `cancelled_signal()` is a `watch::Receiver` a dispatcher loop can
  `select!` on, and it starts at the current state so a late subscriber is not
  left waiting for a change that already happened.
- **`cancel()` force-terminates each registered process tree.** Unix uses
  immediate `SIGKILL` on the child's process group. Windows children are
  attached to Job Objects before execution; synchronous wrappers terminate the
  live Job Object and use native `taskkill /T /F` only as a fallback. After a
  successful tree kill, prview reaps the direct root through the raw child
  handle with a finite budget; it does not block on the Job Object completion
  port. If both mechanisms fail, cancellation returns without an unbounded
  `wait()` rather than turning a kill failure into a hung CLI. Windows-runner
  tests cover child+grandchild, root-exits-first, and cancellation paths.
  It is idempotent in the strong sense: the registry is DRAINED, so a
  second cancel signals nothing — a pid whose process died in between may by then
  belong to another program. Registration checks cancellation while holding that
  same registry lock: a process spawned after the drain is refused (`false`) and
  its process group is killed immediately instead of being inserted too late.
  Callers must `unregister_child` on exit for the same pid-reuse reason.
- **A spawned tree remains owned after its root exits.** Unix retains the
  process-group identity; Windows check and context runners attach the child to
  a Job Object before it starts. A success-shaped wrapper therefore cannot
  orphan background work by making its root PID unavailable to `taskkill`.
- **The command deadline includes output drain.** Async children are reaped,
  residual Unix process-group or Windows Job Object members are terminated, and
  the registry guard is dropped before buffered stdout/stderr are drained. Reader tasks share the
  command's original deadline and are aborted *and awaited* on every terminal
  error, so a background descendant holding a pipe cannot turn a bounded check
  into an unbounded join.

The budget remains `tokio::sync::Semaphore::acquire_many_owned` and the signal
remains `tokio::sync::watch`. Durable Windows ownership promotes `process-wrap`,
already present transitively through Loctree, to an explicit dependency so its
Job Object contract cannot disappear with an unrelated dependency change.

#### Who acquires, and in what order

- **Checks** (`checks::run_all`, `run_all_with_events`) take a permit per check
  before the process starts. The weight comes from `Check::resource_weight`,
  which defaults to `Exclusive`. Rustfmt opts into `Light`; Cargo/rustc, Vitest
  and Semgrep opt into `Heavy` because they receive `CARGO_BUILD_JOBS`,
  `--maxWorkers`, and `--jobs` respectively. Cargo test binaries additionally
  receive `RUST_TEST_THREADS`. TSC, ESLint, Stylelint, Python gates and other
  uncapped pools stay `Exclusive`. Cargo's build and libtest child caps are the
  minimum of the active resource plan, any valid inherited
  `CARGO_BUILD_JOBS`, and the effective Cargo `[build].jobs` visible from the
  exact reviewed cwd (including a remote snapshot). The resolver follows
  Cargo's nearest scalar and legacy-`config` precedence; unreadable, invalid,
  zero-valued, or include-dependent config fails closed to one worker.
  Inherited `CARGO_BUILD_JOBS` uses the same signed logical-core-relative
  interpretation; invalid and zero values also fail closed. Empty `CARGO_HOME`
  follows Cargo's operator-home fallback rather than resolving to the reviewed
  cwd.
  `RUST_TEST_THREADS` is bounded independently by the plan and inherited
  ceiling, so an operator's stricter limit is never raised. Vitest stays at one
  CLI worker in every plan: its CLI option
  overrides project configuration, so passing the wider balanced limit could
  raise a repository's intentional `maxWorkers: 1` ceiling.
  Test selection is runner-specific: Vitest receives a regex, while Cargo gets
  only a literal substring validated before snapshot planning. A filtered Cargo
  exit 0 becomes `Error` unless libtest summaries prove positive execution. In
  a Mixed JS/Rust profile the one shared selector is therefore restricted to
  the literal intersection; per-runner selectors remain a future contract.
- **Context commands** (`artifacts::context_artifacts`) take a permit before each
  spawn, via the synchronous `try_acquire` — `artifacts::generate` is a blocking
  pipeline with a poll loop and has nothing to `.await` on. The weight comes from
  `context_cmd_weight`: `tsc trace`, `eslint json`, `stylelint json` and
  `esbuild meta` are `Exclusive`; the metadata readers (`cargo tree`, `cargo sbom`,
  `npm sbom`, `tauri info`) are `Light`. The context-stage timeout is one
  deadline for the whole batch, not a fresh clock per admitted command.
- **The cargo `target/` lock stays.** It is a correctness lock — one writer per
  `target/` — and the budget is not that. A check that takes both takes them in
  ONE order: **cargo lock first, then budget**. Waiting for that lock races the
  cancellation signal, so a cancelled waiter exits immediately rather than
  waiting for an unrelated Cargo timeout. Same order everywhere is what
  makes two locks deadlock-free, and this direction also avoids parking half the
  budget on a cargo check that is still queueing for `target/`. Nothing acquires
  the cargo lock once it holds budget, so there is no cycle the other way.

#### Queued vs running

Admission is what makes the distinction real, so the run reports it:

- the progress line separates the two — `Running: X (12s) · Queued: Y, Z`;
- the ledger's `started_at` is the moment of admission, not the first poll of the
  check's future, so `started_at − queued_at` is time spent waiting for the
  machine;
- the PV-18 slow notice measures from admission. A check parked on the budget for
  ten minutes has not been slow, it has not started, and naming it would blame
  the tool for the queue;
- TUI mode gets the same split: `CheckEvent::Started` means "the run considered
  this check" (mapped to the `Pending` lifecycle) and the added
  `CheckEvent::Running` means a process began.

#### Cancellation path (Ctrl-C)

```
governor::with_cancellation(work, governor, CtrlC)
      │
      ├─ tokio::spawn(supervise)
      │        └── a SEPARATE task, drained by an explicit stop handoff
      │        │  first interrupt
      │        ▼
      │   governor.begin_cancel()
      │        ├─► semaphore.close()  ── refuses newcomers AND tasks already waiting
      │        ├─► watch::send(true)  ── wakes the dispatcher's select! arm
      │        └─► drain children into an owned termination batch
      │                 └─► blocking tree kill runs off the async interrupt owner
      │                      (SIGKILL -pgid / Job Object + taskkill fallback)
      │        │  second interrupt
      │        ▼
      │   Interrupts::abandon_run()   ── exit(130) without waiting for the unwind
      │
      └─ work.await
             │
             ▼
        checks::run_all returns Err(Cancelled)   ── or the context stage stops admitting
             │                                      ── or App::ensure_not_cancelled fires
             ▼
        App::run unwinds through `?`  ── Drop runs: ledger's shared WorktreeSnapshot,
             │                           heuristics AnalysisSnapshots
             ▼
        main exits `CANCELLED_EXIT_CODE` (130 = 128 + SIGINT)
```

**The supervisor is its own task.** It used to be an arm of the same `select!`
as the run, which works only while the run keeps yielding. `artifacts::generate`
is synchronous and polls its children with `std::thread::sleep`, so for the whole
of the longest stage of a review the task was never polled and NEITHER interrupt
arm could fire — and `tokio::signal::ctrl_c` had by then replaced SIGINT's
default disposition, so the terminal could not end the process either. Watching
from a separate task removes the coupling. `governor::blocking_stage` wraps the
`artifacts::generate` call in both headless `App::run` and TUI `run_analysis`
for the remaining edge, a runtime with a single worker thread: it tells tokio
the stage is about to block so the interrupt supervisor (headless) and the
event loop (TUI q/Escape) keep a thread to be polled on. The interrupt source
is the `Interrupts` trait rather than a direct `ctrl_c()` call, so the state
machine is testable without raising a real signal at the test harness.

Cancellation has a synchronous truth boundary and a blocking cleanup half.
`begin_cancel()` publishes the cancelled state, closes admission, and drains
the child registry before the work future can finish. The owned termination
batch then runs outside the async signal owner, which keeps polling a second
interrupt while a platform tree killer is blocked. Headless completion uses the
same biased `InterruptSupervisor::stop().await` handoff as TUI startup: an
already-ready signal is drained, cancellation is re-checked, and no successful
work value can cross the verdict boundary after Ctrl-C.

`blocking_stage` makes the surrounding runtime responsive; it does not preempt
the in-process libgit2 closure itself. After the TUI's first quit request cancels
the governor, the cancel join therefore keeps polling raw input while it waits
for that closure to unwind. A second Ctrl-C aborts the task wait and returns
typed cancellation through `run_tui`, so terminal cleanup runs before the
established exit-130 path. The in-process closure itself continues until it
returns naturally or the process exits. Before the durable publication commit,
cancellation prevents a completion/verdict publication. After that commit the
pack, `latest`, and index row remain valid by design; a hard second interrupt can
therefore win the narrow return window and exit 130 even though that already
committed pack remains discoverable.

The first raw-mode Ctrl-C deliberately shares the TUI's existing cooperative
quit result with q/Escape: after the join and terminal cleanup it returns
success. A second Ctrl-C key event selects the typed forced-cancellation path.
When a terminal reports event kinds, reported repeat/release events from the
first press do not count. Without keyboard-enhancement negotiation, however,
some Unix terminals encode autorepeated ETX bytes as ordinary press events;
this bounded escape hatch cannot distinguish those bytes from a physically new
press.

Cancelling rather than aborting is the whole point: the supervisor could simply
drop the run future, but returning through the ordinary error path is what lets
the destructors on the way out remove the temporary worktrees a killed process
would leave on disk. Worktree creation arms a path-exact libgit2 rollback before
`git worktree add` starts, covering the interval in which Git has registered the
path but its governed child has not returned. The same in-process fallback
deregisters an already-created snapshot when a cancelled run refuses to spawn
`git worktree remove`; it never runs a global prune or touches a sibling
worktree. `TempDir` remains the filesystem owner. A **second** interrupt is the
operator declining to wait for the ordinary unwind and exits immediately.

**Cancel ⇒ never a verdict.** Only the checks stage watches
`governor.cancelled()` itself, and it is one stage of several: a cancel arriving
in the heuristics, in the artifact stage, or in a run whose gates all replayed
from the cache (an empty runnable set never builds that `select!` loop at all)
was previously ignored outright. `App::run` would go on to write a pack whose
context commands were every one of them recorded `cancelled`, return a report,
and let `main` compute an ACCEPT or a BLOCK from it. `App::ensure_not_cancelled`
now guards every substantial orchestration/artifact seam. External context tools
finish before merge/report generation. If cancellation reaches artifact
generation, success-shaped verdict/report/RUN/MANIFEST/SANITY surfaces are
removed and `00_summary/INCOMPLETE.json` records `status=incomplete`, the reason,
and the interrupted stage. The `latest` symlink and the run-index row are one
publication transaction under a global lock, not two best-effort writes. Before
retargeting `latest`, prview fsyncs a durable recovery journal containing the
predecessor; the next publisher reconciles a crash from the committed index and
clears the journal. Invalid journal state is quarantined without mutating the
advertised alias, so stale or tampered recovery evidence cannot deny all future
publications. The shared review worktree is explicitly removed before either
advertisement; cancellation during that governed cleanup therefore produces an
incomplete, unpublished pack rather than exit 130 after a published verdict.
A valid journal is preserved when `index.jsonl` cannot be opened or parsed.
Publication and recovery use a strict loader that rejects every malformed row;
they never turn partial input into a rewritten partial ledger. Failure to commit
the finished pack into that ledger is a fatal generation error, because a pack
that `state` and MCP cannot discover is not a completed publication.
A cancellation that wins after the alias swap performs a
short, uninterruptible consistency rollback while it still owns that lock. The
index append itself is abortable: the file is saved only while the run is still
active, rolled back if cancel arrives before commit, and retention candidates
are first moved
atomically into `$PRVIEW_HOME/prune-trash`. A cancelled transaction restores
those moves and the previous index. Committed tombstones are physically deleted
at the start of the next registration, before that run mutates its index, using
a cooperatively cancellable directory walk. This removes recursive deletion
from the current publication's irreversible window. The run ends in `Cancelled`
and exit `130`. If a custom output path cannot be renamed into prune-trash
atomically (for example across filesystems), registration keeps the new and old
index rows, emits a retention warning, and performs no destructive fallback.
The durable index/retention marker completion is the explicit commit boundary:
after it succeeds, caller layers return the completed report even if a signal
arrives before they render it. Treating that late signal as cancellation would
claim "no verdict" while `latest` and the index already expose one.

The publication lock uses a persistent v2 kernel lock plus the legacy
create-new pathname understood by pre-0.8 binaries. Together they exclude old
and new publishers from the **index critical section**, but not from the whole
publication: pre-0.8 binaries retarget `latest` before attempting the legacy
lock. A 0.8 rollout must therefore use a quiescent cutover that drains and
excludes every pre-0.8 publisher sharing `PRVIEW_HOME`; only 0.8-to-0.8
publication has the end-to-end transaction described above. A live legacy
owner blocks normally; a stale legacy sentinel fails
closed and is never rewritten automatically because an old process could have
observed it before pausing. An operator may remove that exact sentinel only
after ruling out old publishers. MCP branch activation preserves this rule:
stale `.active.lock` evidence becomes non-retryable `storage_locked` with
`recovery_required` and its exact path, rather than the false claim that a live
review will clear on retry. Unsafe or unreadable activation paths become
`storage_corrupt`; they are not folded into lock contention. Lock opens reject
symlinks/reparse points and, on Unix, shared hardlink inodes; journal/index/prune
manifests are published via owned unique temp files and atomic rename.
When the legacy claim fails, the contender explicitly releases its v2 kernel
lock before returning the recovery error; operator recovery can therefore retry
immediately instead of waiting for platform-specific close timing.
The prune manifest is not path authority by itself: before recovery moves or
deletes a payload, the payload root, its `00_summary` directory, and its
`RUN.json` must each be owned non-link components, and RUN must identify the
same artifacts root. Windows rejects every reparse point (including junctions
and mount points); authority files on Unix reject shared hardlink inodes.
Recursive
cleanup unlinks a nested reparse entry itself and never traverses its target. A
missing, invalid, or mismatched manifest/payload pair is
preserved fail-closed without denying the next publisher; an I/O failure after
recovery mutation begins still aborts publication. Rollback attempts the
predecessor moves and previous index independently. If the previous-index write
cannot be confirmed, the outer publication journal remains durable and the run
fails for restart reconciliation. Relative custom output paths are made
absolute before pack creation, so a later cwd cannot retarget recovery. The
final custom path is claimed with one create-directory operation and must not
already exist; one immutable path therefore maps to one pack and one index row.
MCP reserves that path before spawn through a create-new nonce sentinel; only
the child holding the nonce may consume the control-only directory once. These
metadata checks cover accidental and state-at-rest link traversal, not a
same-user adversary racing directory replacement between check and use.
Directory fsync makes the rename/manifest/index ordering a power-loss contract
on Unix (including macOS). The non-Unix implementation does not claim equivalent
directory-entry power-loss durability.

`--update` needs a gate of its own (`App::reuse_unchanged_run`), because it is
the one path that returns a report without reaching any of the others: an
interrupt during `prepare_refs` followed by a HEAD with no new commits used to
hand back the *previous* run's pack, and `main` computed an ACCEPT or a BLOCK
from that. Reusing a pack is still reporting a verdict. Ref preparation is now
inside the run scope and its Git child is registered, so cancellation can stop
the fetch rather than merely rejecting the eventual reuse.

Every child that can be reached this way must be registered. Unix children lead
their own process group (`proc::harden` for async checks and `proc::harden_std`
for synchronous context commands); Windows check and context children belong to
a Job Object. One owned-tree operation therefore reaches `cargo → rustc → cc`
and `sh → pnpm → tool`, even when the wrapper exits first. Checks register through the
`with_child_scope` task-local rather than an argument: the governor is known at
the dispatcher, the pid at the single spawn point five frames below it behind
`Check::run(&self, config)`, and a trait method cannot grow a parameter without
every check and every `run_command_*` call site growing one it never reads. The
returned guard unregisters on drop, so the success, timeout and error paths all
leave the registry clean — a pid the governor still believes in is a pid it may
signal, and pids are reused.

An MCP `quick` review adds one cross-process ownership boundary around that
in-process registry. The adapter cannot treat the review root's Unix process
group as a recursive tree: checks intentionally lead distinct groups. It sends
Ctrl-C first so the review governor performs its normal drain, while a private
sidecar ledger mirrors group registration and completion to the MCP parent. A
nonce-bound header rejects an incomplete or stale capability. The forked child
first writes a provisional PGID in `pre_exec`, before it can run the tool;
governor registration then upgrades that evidence with the native process-birth
identity. The mode-0600 ledger descriptor remains CLOEXEC in the multi-threaded
MCP parent, becomes inheritable only inside the already-forked review root, and
is restored to CLOEXEC before repository discovery or startup helpers. It stays exclusively
locked by the review root and any fork still in pre-exec. Hard fallback stops
the root and accepts a finite local process-table census only when that same
snapshot reports the root in stopped state. Only process groups led by proven
direct children and committed native identities may be signalled; a provisional
PID never authorizes a signal by itself. After killing and reaping the root, the
MCP parent must acquire the lock before its final drain, so a child cannot
disappear into the spawn-before-registration gap. The descriptor closes at
tool exec, while every descendant is already contained by its tool group. The
review root also handles the macOS gap where a very short-lived group leader is
already waitable but no longer exposes its native birth identity: because the
owned leader remains unreaped, registration can safely terminate that exact
PGID and any surviving members before PID reuse becomes possible. The parent
then settles the provisional row without treating it as signal authority. The
sidecar is a control file beside the run directory, never an input to its
immutable manifest or ZIP. If the bounded unwind stalls, the parent terminates
every still-owned group before killing and reaping the direct review root;
tracker Drop repeats that cleanup. Confirmed cleanup unlinks the sidecar;
unconfirmed containment retains it and is surfaced in the MCP error contract.
Windows keeps native recursive `taskkill /T` and needs no mirror.

**`--watch` ends on the first interrupt.** One `App`, and therefore one governor,
is shared by every iteration, and `Semaphore::close` is one-way — so a cancelled
watcher can never grant work again. The iteration used to report any failure of
its quick run as an ordinary error and carry on, which turned that into a silent
degradation: every later edit produced a pack with an empty `30_context` under a
cheerful "Regenerated artifacts", until the operator interrupted a second time
and took the cleanup with them. A cancellation is now propagated out of the
iteration, and both watch loops (the filesystem watcher and the polling fallback)
carry a biased `governor.cancelled()` arm so a cancel arriving while the watcher
is idle ends it too. Each iteration captures provenance under the same
supervised synchronous stage, and its `rev-parse`/`status`/`diff` probes use
owned governed subprocesses rather than raw `Command::output` waits.

**Everything long the run spawns must be in a child scope.** The `uv sync`
pre-step was not, and outside a scope `register_active_child` is a no-op, so
`cancel()` had no pid to signal: a Ctrl-C during a cold venv build printed
"stopping running tools" and then waited out the full timeout with `uv` still
running. It is scoped now, takes an `Exclusive` permit, passes the run's worker
limit to uv's download/build/install pools, and refuses to start on an
already-cancelled run. The
Loctree cache creation is the exception: the synchronous third-party scan runs
in a private current-executable worker under its own child scope. Cancelling the
review therefore kills the scan process itself; it never relies on aborting an
already-started `spawn_blocking` closure. Snapshot interpretation remains
in-process and cooperatively checks cancellation.

`--tui` analysis is deliberately NOT wrapped by the headless signal supervisor:
once the terminal is in raw mode, Ctrl-C
arrives as a Control-C key event and the TUI routes it through the same
cancel-and-join path as q/Escape before wizard or panel handling. Its dispatcher is
nevertheless held to the same contract as the headless one — same
`presync_python_venv`, same biased `governor.cancelled()` arm — because it was a
copy that had drifted back into both of the bugs above while still claiming to
mirror `run_all`. Artifact generation on the TUI path uses the same
`blocking_stage` wrapper as headless, so a single-worker runtime can still
poll q/Escape while the pack is being written. After that first quit stops the
ordinary event loop, the cancel join remains a terminal-input owner: it waits
cooperatively for the analysis task but treats a second Ctrl-C as typed forced
cancellation. The initial repository/ref preflight is the exception: it runs
before raw mode under a temporary Ctrl-C signal supervisor, so a slow fetch
cannot enter a window where signals are disabled but the terminal event reader
does not yet exist. The supervisor stays alive until raw mode is enabled, then
completes an explicit biased handoff that consumes any already-pending signal
before key events take ownership. Both the post-stage and post-handoff checks
convert a late interrupt into typed cancellation.

**Operator surface.** `--resource-budget safe|balanced` selects the plan; preflight
prints requested/effective budget, parent permits, child-worker cap, current-load
decision, expensive tools, and cheap-first schedule before checks execute.

### mcp/

The MCP server (`prview mcp`) is a thin contract adapter over the prview core.
It adds no review logic: tools spawn `prview` as a subprocess to produce a pack
and read truth back from storage. Every tool takes an explicit `repo` path,
every response carries `schema_version`, and every failure is fail-loud. See
`docs/mcp.md` for the tool reference.

Synchronous quick reviews retain their hardened child through the bounded wait.
A timeout or child-wait error first requests the root's ordinary cancellation
unwind, then uses the parent-owned child-group sidecar described above before
hard-killing and reaping the direct root. Failed quick runs retain
`RUNNING.json` as diagnostic `Stale` state, which lifecycle readers do not treat
as an active run.

Deep reviews are asynchronous at the RPC boundary, not unowned processes. A
dedicated waiter thread retains each `Child` and reaps its direct root, including
an immediate failure. The review root inherits the same parent-owned child-group
sidecar used by synchronous quick reviews, so after root exit the waiter also
drains separately hardened Cargo, Semgrep, and other nested groups. It removes
`RUNNING.json` only when publication is complete and full containment has been
confirmed; otherwise the marker remains explicit diagnostic state. Residual
Unix root-group members are terminated as part of that proof, while Windows
retains the complete Job Object until wait.
The caller also captures the exact publication-index path before starting the
waiter, so background completion never re-resolves a different storage home.
Active-run discovery rejects markerless history and the completed-run `latest`
alias before lifecycle probing. A run without `SANITY.json` never reads the
global publication index; index lookup is reserved for proving that a finalized
pack committed durable publication.
`RUNNING.json` protocol v2 pairs the PID with the native
process creation identity. A successfully read mismatch proves PID reuse and
becomes stale, while a live PID whose token is absent or whose native identity
cannot be read fails closed as running. Legacy and unknown marker versions use
the same conservative live-PID boundary, then become stale after that PID exits.
The server returns `status: running` for a new review only after identity
capture, marker publication, and reaper installation all succeed.
Failure at any setup seam terminates the child tree, reaps the direct root, and
fails the RPC instead of publishing an untracked run.
Linux, macOS, and Windows are the supported MCP `run_review` targets because
they provide the native PID-reuse-safe identity required by this protocol. A
different source-buildable target is refused before activation locking or child
spawn; the ordinary CLI does not require a durable MCP liveness marker and
remains the direct execution surface there.

### artifacts/mod.rs

The core artifact generator. Builds the numbered directory layout
(`00_summary/`, `10_diff/`, `20_quality/`, `30_context/`):

- Root: `PR_REVIEW.md`, `dashboard.html`, `artifacts.zip`
- `00_summary/`: `RUN.json`, `PROVENANCE.json`, `FAILURES_SUMMARY.md`, `MANIFEST.json`, `SANITY.json`, `MERGE_GATE.json/md`, metadata
- `10_diff/`: `full.patch`, `per-commit-diffs/` (batching + thematic labels), `per-file-diffs/` (hotspots)
- `20_quality/`: per-check `*.result.json` + `*.log`, `full-checks.log`, `checks-errors.log`, `coverage-delta.txt`, `PUBLIC_API_DIFF.json/md`, `BREAKING_CHANGES.json/md`
- `30_context/`: optional `INLINE_FINDINGS.sarif`, `changed-tests.txt`, profile-specific (`cargo-tree`, `tsc-trace`, `eslint`, `vitest`)
- `latest` symlink in the parent dir (completed runs only)

#### Stale-cache caveats (`MERGE_GATE.json.stale_cache_caveats`)

A verdict can rest on evidence the run never produced. In the Vista dogfood run
(`PRV-CACHE-STALENESS`) a `Cargo audit` result replayed from a cache written
before a reboot co-authored a `BLOCK`, and the pack said only `cached: true` —
nothing named the age of the evidence.

`generate_merge_gate` therefore reads the run's ledger (the only place that
carries `cache_age_secs`, see [ledger/mod.rs](#ledgermodrs)) and emits one entry
for every gate row whose replay is older than
`STALE_CACHE_CAVEAT_MAX_AGE_SECS` (7 days, a constant in
`src/artifacts/merge_gate.rs`; a CLI knob is a follow-up). This includes stale
passes: unchanged source keys do not bind the compiler or every tool version,
so an old positive result can support a clean verdict the current toolchain
would reject:

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
`checks`, and `inline_findings` of a stale run against a fresh one.

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

Cargo target discovery matches the exact `Cargo.toml` basename. Library
discovery validates every consumed field (`package.name`, `[lib]`, `lib.name`,
`lib.path`, `lib.proc-macro`, `lib.crate-type`, `package.autolib`, and effective
edition) instead of inventing defaults for an invalid schema. Edition is
resolved from a library-target override, direct package value, exact
workspace-package inheritance, or Cargo's 2015 default. The crate contract
includes the normalized `crate-type` set (default `["lib"]`, or
`["proc-macro"]` for a proc-macro target) and `proc-macro`; the library root path
and edition stay in evidence/provenance rather than globally changing every
crate fact. Optional normal/build/target
dependencies without an explicit `[features]` entry become implicit Cargo
features unless suppressed through `dep:` references. Package and explicit
library names must also be non-empty Cargo-valid identities. Keywords addressable
as raw identifiers and Cargo-valid special values `crate`, `self`, `Self`, and
`super` remain string identities in the census; prview does not synthesize an
invalid Rust path from them. A TOML string alone is not semantic validation. A
valid virtual workspace is non-crate; an implicit library exists whenever its
live default `src/lib.rs` exists unless `package.autolib = false`, independently
of edition and unrelated explicit targets. Its absence is not a missing-root
error, while an explicit `[lib]` whose effective root is unavailable remains
typed `MissingLibRoot`. A tracked symlink at an implicit library root is not
followed: its compiler-visible source and module base are revision-ambiguous,
so each compared side retains non-neutralizable `MissingLibRoot` uncertainty.
Repository-relative
paths are normalized fallibly: absolute, prefixed, non-UTF-8, and escaping
paths become manifest/source unknowns rather than being remapped. Missing,
renamed-away, deleted, non-regular, non-UTF-8, unreadable, or parse-failed
manifests and roots remain typed unknowns.

Real binary-target discovery is separate from the library early-exit. It
recognizes Cargo's implicit `src/main.rs`, `src/bin/*.rs`, and
`src/bin/*/main.rs` roots plus explicit `[[bin]]` entries; applies the edition
2015 auto-discovery default per binary target category (only explicit `[[bin]]`
metadata disables implicit bins by default) and `package.autobins`; and validates
target name, explicit or inferred path, target edition, and `required-features`. Explicit
targets claim their roots so the same source is not also invented as an
auto-discovered target. Malformed, unavailable, duplicate, or ambiguous target
metadata remains typed manifest uncertainty. An exact binary root that is
itself a tracked symlink is retained as non-neutralizable typed uncertainty.
Discovery does not separately model a symlinked parent directory such as
`src/` or `src/bin/`; that bounded filesystem-shape residual remains outside
this contract. A binary target name is validated as Cargo target metadata, not
as a Rust dependency-crate identifier: a leading digit is valid, while Cargo's
reserved build-directory names remain invalid. Each binary
uses a stable target-scoped analysis identity (`<package>#bin:<target>`) that
preserves the exact manifest target name, so `foo-bar` and `foo_bar` remain
distinct while preventing
the common same-named library and default binary from sharing projection,
edition, cfg-authority, module-cache, or native-evidence state. These synthetic
identities are evidence keys, not Rust dependency crates and not additions to
the public crate census.

Target projection follows Cargo/Rust linkage semantics. `lib`, `rlib`, and
`dylib` outputs expose the ordinary downstream Rust item graph; proc-macro
targets expose only supported procedural macro entry points. A target whose
effective types are only `cdylib`, `staticlib`, or `bin` is not projected as a
Rust dependency surface. Its public and private native exports are still
scanned, including exported associated functions in inherent and trait impls.
Direct and associated native function evidence contains the normalized
signature and export attributes but not the implementation block; static
initializers remain observable because they can determine exported data.
For ordinary Cargo binaries this scan starts from every discovered binary root;
internal `pub` items remain absent from the dependency API surface, while native
export signatures retain typed uncertainty bound to their local type semantics.
In a native-producing target, including mixed `rlib + cdylib`, an associated
binary export carrying a transforming attribute binds the full macro-visible
member input, a separate normalized owner/ABI contract, and the revision-backed
transformer implementation. A custom associated attribute may synthesize the
`no_mangle`/`export_name` attribute during expansion; for `cdylib`, `staticlib`,
or `bin` output that possibility remains typed macro-generated native-export
evidence even when no export marker exists in the pre-expansion AST.
Item-position invocations backed by `macro_rules!`, plus `include!`,
`global_asm!`, and other opaque macro invocations, are native-export boundaries
when the crate produces one of those native artifacts, even when the target is
not Rust-linkable; their invocation, included source, or implementation proof
therefore cannot neutralize after the generated native surface changes. The
fallback is not applied to private owners in `rlib`-only crates, whose associated
transform evidence still materializes only after external Rust reachability is
proven. The native target remains typed uncertainty, so a transition to or from
a Rust-linkable target cannot be reported as falsely clean.

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
remain typed unknowns. An `include!` / `include_str!` / `include_bytes!` unknown
carries a digest of the included file when that path is readable. Its proof
walks the caller-observable contract — signatures, generics, field and alias
types, enum discriminants, trait members, public constants and inherent
associated items, plus observable trait-impl associated types/constants — but
not ordinary function bodies. It materializes only when
the owning declaration is externally reachable, directly or through a public
reexport. An unresolved or changed included source keeps the unknown active
instead of treating an identical invocation as unchanged. Unchanged terminal
`include_str!` and `include_bytes!` proofs may neutralize because the digest
binds their complete output. Plain `include!` remains review-required even when
its direct file is unchanged: that file may contain path-sensitive `file!()` or
nested relative includes whose transitive sources are not yet Merkle-proven.
A reachable
`pub extern crate` is likewise
retained as guarded `UnsupportedExternResolution` until external/prelude
resolution exists; private or unreachable declarations do not create external
semantic surface. A private `extern crate self as alias` is nevertheless a
same-crate module binding for dependency resolution: `alias::Hidden` is followed
back to the root `Hidden` declaration so its layout/auto-trait uncertainty stays
attached to the public owner that exposes it.

Custom cfg leaves are not assumed to be Cargo's built-in feature/target/runtime
predicates. An externally relevant custom predicate on an item, field, variant,
trait or impl member, or foreign item emits `CfgPredicate` evidence bound to a
revision-backed authority digest whenever an active build script or repository
Cargo config can supply `--cfg`. A declared build script must resolve to a live
regular revision entry; Cargo's `build = true` explicitly selects the default
`build.rs`, while `build = false` disables it. Only the effective
repository-root Cargo config qualifies; when both names exist Cargo's legacy
`.cargo/config` precedence over `.cargo/config.toml` is preserved. Authority is
recognized only at legal schema paths (`build.rustflags`, target-specific
`rustflags`, or a concrete-target link override's `rustc-cfg` when its key
matches the package's `links`). Declaring `package.links` without a live build
script is an invalid manifest, not a way to acquire config authority. Nested
fixture/member configs and lookalike keys in unrelated sections such as `[net]`
do not upgrade a proof. Config includes remain unresolved until their authority
graph is source-backed. The current conservative digest covers the complete live
revision inventory, so it may over-report after an unrelated tracked edit; it
never executes `build.rs`. With no complete revision-backed authority, the proof
is explicitly unresolved and cannot neutralize against the same text on the
other side. A definitely private untransformed free helper does not create a
standalone cfg unknown; any effect on an exposed opaque return remains covered
by that public proof's implementation digest.
Private non-function declarations with custom cfg currently remain
conservative `CfgPredicate` uncertainty before a complete reachability proof.
That is a known precision residual: it can add review noise, but it cannot
certify a conditionally exposed contract as clean.
Semantic proof comparison includes the public unknown's kind, crate/module
location, exact evidence, and guards, while continuing to exclude private
reexport target/origin spelling. Terminal `include_str!` / `include_bytes!`
proof matching also excludes the private donor source path because its public
path, normalized contract, invocation, and digest already bind the complete
caller-visible output. Plain `include!` remains source-path-sensitive.
A public reexport's
resolved origin is part of the compared contract, so retargeting
`pub use a::A as Public` to `pub use b::B as Public` when both donors remain
public is a `Changed` fact.

Item identity is `crate + external module path + Rust namespace + NFC external
name`. Value, type, and macro namespaces are separate. The snapshot also emits
explicit Module, Crate, and CargoFeature identities: public empty modules,
library-crate declaration changes, and removed or redefined Cargo features
therefore cannot disappear merely because no ordinary item changed. These
container namespaces do not participate in Rust `use`-leaf resolution. Tuple
and unit struct constructors occupy Value; named-field structs remain Type-only.
`macro_export` is projected to the crate-root Macro namespace, with docs,
rustfmt, and lint attributes normalized away. Its item-local contract includes
the effective edition of the defining library target because macro fragment
semantics are edition-dependent; editions do not otherwise manufacture a
crate-wide API change. Proc-macro crate exports use their
external macro/derive names only for public functions declared at crate root;
private or nested declarations become precise unknowns. Unresolved transforming
attributes, including recursively nested `cfg_attr`, are checked on modules,
impls and associated items, foreign blocks/items, macro declarations, and
ordinary items before visibility filtering. A private annotated input can expand
into public output, so it is not
discarded merely because the source item is private. Replacement-style
attribute boundaries suppress only claims for their annotated owner; an
unrelated confirmed change in the same module remains visible. Derives are
additive: the annotated input item remains a confirmed contract while custom generated
output emits `MacroGeneratedItems` evidence bound to the complete input and to
revision-backed transformer provenance. Custom derive/helper attributes are
excluded from the confirmed input contract, and an unqualified builtin-looking
derive is treated as custom whenever an import, glob, or `macro_use` can shadow
that name. Builtin `Default` variant markers are retained only when the enum has
a proven builtin derive, including matching nested conditional predicate
lineage. Singleton `all`/`any` wrappers are normalized; any remaining
unprovable helper/derive relationship emits typed `CfgPredicate` uncertainty
instead of disappearing. Custom helper attributes stay outside the confirmed
contract. Associated-item
transform evidence is materialized only after its inherent or trait owner
reaches the external API, directly or by reexport. Public type-alias chains are
resolved transitively and conservatively emit owner uncertainty because source
analysis does not prove generic specialization. Function-like associated macros
on externally reachable Rust owners, native-artifact item-position boundaries
(including private owners), top-level macro invocations, and nested
trait/trait-impl attributes bind both their invocation/input and the appropriate
revision-backed implementation substrate.
Conditional and nested `cfg_attr(..., macro_export)` declarations remain
crate-root macro API only for Rust-linkable targets. A lock-backed external
candidate binds all reachable product/path manifests, effective Cargo config
bytes, and lockfiles.
When a reachable local proc-macro exists, the current safety floor additionally hashes
the complete live tracked-entry inventory by Git object identity (excluding
redundant directory-tree objects), including nonstandard `lib.path`, `#[path]`,
gitlinks, and build assets outside a package directory. A lock-backed external
candidate must also appear by actual package name, external registry/git source,
and a version satisfying the declared requirement in the effective lock.
Registry entries additionally require a valid checksum and Git entries a
precise commit; a present but stale/empty lock or a same-name local workspace package does not
qualify. Cargo config discovery covers each reachable manifest directory and
its ancestors as well as the lock authority; running Cargo from a workspace
member therefore cannot hide a member-local source replacement. Tracked
symlinks, including working-tree regular-to-symlink type changes, remain
`unresolved` because their Git blob pins only the target path, not the bytes of
an outside-repository target. Exact attribute-to-crate resolution remains a
future precision improvement; the local
aggregate can therefore over-report after any tracked-file change. Missing
effective product/workspace lock data, no transformer dependency candidate, or
unresolved Cargo manifest/config source replacement (`patch`, `replace`,
`source`, or `paths`) produces an explicit unresolved digest that never
neutralizes. A lockfile owned only by an unrelated fixture cannot qualify the
proof. An unchanged transformer therefore cannot neutralize a changed item or
changed implementation substrate. Foreign functions/statics inherit the parent
ABI, safety, and relevant attributes.

Contracts are emitted from normalized `syn` ASTs. Ordinary function bodies are
excluded from confirmed item contracts, although private implementation inputs
can contribute to the conservative opaque-return digest described below.
Public trait method and associated-const defaults remain directional structural
contract facts. Adding a default is compatible; removing one can make
downstream impls incomplete, while changing a const default value/type/cfg
remains a confirmed contract change. Member slots retain order, attributes, and
canonical cfg identity, so a default cannot move between same-named disjoint
cfg branches. Trait-default opaque proofs carry the same member cfg key and do
not cross-cancel.
Bodies of caller-observable `async fn` and return-position `impl Trait` items
also carry item-local `OpaqueReturnAutoTraits` evidence because their hidden
types can change `Send`, `Sync`, and other auto traits without a signature edit.
The proof binds a canonical body/signature to the effective product/workspace
lock, canonicalized repo-backed Rust files, and cheap Git object identities for
every other live tracked input. This covers nonstandard `include!`/`#[path]`
files and build-script assets without rereading every blob; redundant directory
tree objects are excluded so they do not defeat Rust canonicalization. Tracked
symlinks keep the proof unresolved until their target provenance can be proven;
pinned gitlinks remain object-bound. Free identifiers from the whole body are
reserved before synthetic binders are allocated, and macro namespaces are not
rewritten as type-generic uses. Public opaque bodies are alpha-normalized inside
that substrate so generic binder
spelling plus parameter/local irrefutable-destructuring/closure/loop/shadow
binding names remain neutral; refutable match/`if let`/`while let` pattern names
stay spelling-sensitive unless name resolution can prove they are bindings.
Private helper changes remain observable. This conservative implementation
closure can over-report after an unrelated tracked
input changes. Missing lock-backed provenance or unresolved Cargo source
replacement never neutralizes.
Changed digests stay typed uncertainty rather than becoming a confirmed API
change, and follow public reexports/inherent origins without suppressing an
independent signature change. One-sided proofs for a wholly new or removed item
are suppressed because the Added/Removed fact already carries the compatibility
decision. Adding a trait-method default is compatible; removing one remains a
confirmed contract change. Ordinary named private member names/order are
excluded. Inherited and restricted field visibility are normalized to the same
external-private form. Their anonymized type multiset remains observable because
a private type can change public auto traits such as `Send`/`Sync`. Only
`repr(C)` fixes named struct-field declaration order. `repr(transparent)` and
standalone `repr(packed)`/`repr(align)` retain their semantic attributes and
private field types but canonicalize named private-field order. `repr(Rust)`
follows the same order-insensitive contract. Tuple-field position and privacy
remain structural because any private tuple element changes constructor
callability and arity. ABI, qualifiers,
generics/bounds/where clauses, return types, public fields with structural tuple
indices, enum variants/discriminants, trait headers and associated items, type
aliases, public constants/statics, and relevant attributes remain. Rust 2024
unsafe attribute wrappers are parsed structurally. Private functions or statics
exported through direct or conditional `no_mangle`/`export_name` remain typed,
guard-aware binary-symbol uncertainty rather than being omitted. Inherent impls
are collected independently of module reachability, resolve owners through
same-crate `self`/`super`/`crate` paths, and retain self type, specialization,
impl generics/bounds/where clauses, and impl/item attributes before projection
through every reachable type alias. Unprovable owners are typed unknowns.
Private trait-impl dependency evidence is keyed by the joint effective cfg of
each resolved trait/owner pair. Alternative trait targets are never flattened
into one owner-independent set, so exchanging cfg-selected traits changes the
public dependency proof even when the owner aliases stay fixed.
Documentation, rustfmt, and lint-control attributes are recursively discarded;
shape/ABI attributes remain. Raw identifiers and NFC-equivalent identifiers
share semantic names. Nested `cfg`/`cfg_attr` use the same recursive sorted and
deduplicated `all(...)`/`any(...)` canonicalization as top-level guards, without
evaluating host configuration.

Union members are canonicalized as an order-independent set even under
`repr(C)`: every member starts at offset zero, while names and types still
determine source compatibility, size, alignment, and auto traits. Named
enum-variant fields are order-sensitive for `repr(C)` and primitive integer
representations. They are order-neutral under `repr(Rust)`, `repr(transparent)`,
and standalone `repr(align)`; tuple-variant order is always
preserved.

Confirmed function contracts canonicalize parameter patterns to `_`. Generic,
const, and lifetime binders are alpha-normalized by declaration order across
free, trait,
inherent, foreign, and higher-ranked function signatures as well as public
structs, unions, enums, type aliases, and associated trait const/type members.
The mapping is reused at every bound, type, and default occurrence, so renaming
a binder is neutral while generic order, types, ABI, and lifetime relationships
remain part of the contract. Opaque macro invocation token bodies are not Rust
AST to `syn`; binder references that exist only inside those tokens remain a
source-parser limitation and are not presented as compiler-backed truth.
Source-only analysis does not pretend to resolve trait selection or coherence:
an impl whose trait and
owner are both externally reachable is retained as `TraitImplResolution`
uncertainty with its normalized source contract until compiler-backed resolution
exists, including impls written in a private helper module. Private/private
impls do not degrade the public surface. Declaring-module reachability does not
gate collection: Rust makes a public-trait-on-public-type impl globally usable
regardless of the helper module's visibility. An unqualified unresolved trait
path is retained conservatively because it may have entered scope through an
external `use`; the backend does not guess externality from a trait-name
allowlist.
Trait and owner aliases are reduced to canonical guarded nominal pairs before
evidence is compared. Top-level alias spelling and reference/pointer/slice/
array owner wrappers are canonicalized, and ordinary fn/const/type impl members
form an order-independent set. The declaring module and source remain part of
the proof because relative associated types and generic arguments resolve in
that scope. Moving an otherwise identical impl can therefore remain a
conservative unknown, and aliases nested only inside generic arguments are not
claimed equivalent until compiler-backed resolution exists. Finite alias
resolution exhaustion is a structural non-neutralizable proof state rather
than a diagnostic-text heuristic.

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

Named-struct field projection is policy-aware. A public field added to an
existing exhaustive struct is a `Changed` parent contract because downstream
struct literals and exhaustive patterns stop compiling. It remains a parent
`Changed` on an existing `#[non_exhaustive]` struct: callers cannot construct
or exhaustively match that type, but the field type can still remove
compiler-derived auto traits from the parent. A wholly new public struct remains
an added item. Public fields are selected by external visibility, not by the
legal identifier prefix used in the internal private-field projection, so a
user field named `__prview_private_field_*` cannot disappear from the field map.

Enum projection applies the corresponding exhaustiveness policy independently:
adding variants to an exhaustive public enum changes the parent contract, while
an otherwise unchanged public `#[non_exhaustive]` enum exposes an appended
fieldless variant as informational `Added`. A fieldless variant inserted before
an existing variant stays `Changed` because it shifts implicit numeric
discriminants; payload-bearing variants stay `Changed` because their field types
can change auto traits. ABI-sensitive
`#[repr(...)]` enums, including primitive integer reprs from `u8` through
`isize`, remain on the parent `Changed` path even when they are non-exhaustive,
because payload growth can change size or alignment. Adding a field to an
existing variant-level `#[non_exhaustive]` variant also stays on the parent
`Changed` path: `..` protects matching syntax, not auto-trait compatibility.
Exhaustive variants, field removals/type changes, and enum header/policy changes
remain conservative as well.

Exact identity is grouped on both sides before any fact is consumed: only a
`1 ↔ 1` component may become a confirmed change, while wider components are
consumed as deterministic typed ambiguity, including one-sided duplicate
components before the final add/remove pass. Cfg-region changes are paired only
when the guards may overlap. The comparison reuses the snapshot resolver's
conservative disjointness proofs for Unix versus Windows and for a direct cfg
atom versus its direct `not(atom)` negation. Other different feature guards
remain potentially co-active. One shared pair-certainty
check tests both identities and both source paths against the unknown regions
from both revisions before any exact, cfg, relocation, or visibility fact can
be confirmed. A glob, include, source-parse, or other relevant unknown therefore
blocks a contradictory confirmed fact at either the source or destination.
Standalone unknown findings retain their source side, source path, and revision
provenance. Before those findings are emitted, identical one-to-one unknown
proofs on base and target cancel out: kind, crate/module, cfg guard, evidence,
and provenance class must match, and each proof must belong to its own snapshot.
An unresolved custom-cfg authority proof is structurally non-neutralizable even
when its diagnostic text matches on both sides. A complete unchanged authority
digest may neutralize; a changed digest remains review-required uncertainty.
Source path must also match for every unknown kind except terminal
`include_str!` / `include_bytes!`, whose private donor file may move without
changing the bound public proof. Changed,
one-sided, duplicate, detached, or Git-tree-versus-overlay proofs remain typed
unknowns. Finding IDs preserve Rust identifier case and
serialize the complete semantic identity, including both sides' cfg regions,
contracts, and typed unknown provenance; legal ambiguous input is data, never
an assertion failure.

A legal non-UTF-8 Git tree component is represented by a deterministic internal
identity whose surrogate component starts with a NUL sentinel — a byte Git
forbids anywhere in real pathnames. At the artifact boundary every internal
sentinel is removed, including for a nested path, producing a printable
`dir/<git-path-bytes:...>` surrogate without embedding NUL in JSON or rendered
output. A legal UTF-8 file literally named like that surrogate therefore remains
a separate readable entry. The raw path emits a side-specific `PathNonUtf8`
unknown. The tree walk skips only descendants whose prefix cannot be represented
and continues through valid siblings. Unlike an unchanged parser/resolver
unknown, path uncertainty is deliberately not neutralized across revisions and
does not contaminate confirmed facts from independently parsed valid paths.

`compare_rust_api_revisions` constructs snapshots only from the exact
`Diff.base_commit_id` and `Diff.target_commit_id` Git trees. It never reads a
checkout, working-tree overlay, or patch fallback. Crate discovery follows
Cargo workspace `members`/`exclude` when a workspace exists, and otherwise only
the repository-root package — nested fixture and tool manifests are not product
API. A revision source intentionally rooted below the repository may expose one
package or one workspace authority. Multiple rootless packages/workspaces are
not silently unioned, and an unreadable, malformed, or non-UTF-8 rootless
manifest is itself an unresolved authority. A parseable manifest with neither a
top-level `[package]` nor `[workspace]` is invalid in the same way, rather than
being discarded as though it did not exist; a manifest that combines
`[workspace]` with `package.workspace` is also rejected. These cases emit
side-specific `WorkspaceDiscovery` and/or `ManifestParse` uncertainty and no
false confirmed product authority.
Private-field types stay in the parent contract (auto-trait effects such as
replacing `u8` with `Rc<()>`), and implementations of external/prelude traits on
a public type are typed `TraitImplResolution` unknowns. A transitive non-public
local type, private import/module alias, or local impl reached from public API
emits guard-aware `PrivateTypeDependency` uncertainty because its auto-trait,
layout, or inference consequence requires compiler resolution. A root package
that declares `package.workspace` is enumerated through that workspace's full
member authority; missing, invalid, incomplete, or non-reciprocal membership
emits `WorkspaceDiscovery` rather than certifying an isolated root package.
Duplicate exact base/target
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
changed files, for Tauri (mixed JS + Rust) projects. Its head-side directory,
filesystem walk, and changed-file mapping come from the run-wide reviewed tree;
the base side is read from the exact Git objects. An off-HEAD review therefore
cannot leak commands or layout from the operator's current checkout.

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
