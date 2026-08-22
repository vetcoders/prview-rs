# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

> Entries prior to 0.4.0 document development that predates this repository's
> public debut. 0.4.0 is the first public release, so only versions from 0.4.0
> onward have git tags and comparison links.

## [Unreleased]

### Added

- `00_summary/PROVENANCE.json` — a pack-level record of *what was analysed*,
  next to the per-check rows that record *where each gate ran*. It carries the
  `target_sha` the pack judges, the `base_sha` it diffed against — the merge
  base the patch was actually generated from, not the tip of the base branch,
  which differ as soon as the base moves ahead of the branch point — the
  `head_sha` checked out locally, whether the working tree was clean when the
  run started (frozen before any check ran) with a `sha256` digest
  fingerprinting what was dirty, and one row per check — `{id, cwd, target_sha,
  tree_state, started_at, cached}`. The digest covers the *content* of every
  dirty path, not just its status code and name, so two runs that modify the
  same files differently are distinguishable — including a nested repository,
  which git reports as a single entry and which therefore fingerprints by its
  own `HEAD` and, when dirty, by a recursive digest of its own dirty subset
  (three levels of nesting deep) rather than by the bare fact that a directory
  is there; each run freezes its own state, and under `--watch` every iteration
  re-reads the tree it is about to analyse.
  The file is listed in `AI_INDEX.md`'s reading order, right after the gate
  verdict it explains — and in the documented contract for it
  (`docs/contracts/ai_index.md`) and the artifact-pack inventory in `README.md`,
  so a consumer implementing the contract can discover that the file is required
  and where it belongs. `worktree.clean` is nullable: a status that could not be
  read is reported as unknown rather than as a clean tree. `bases[]` names every
  baseline the pack's patches were produced from as `{name, sha}`: a multi-base
  run (`--base a --base b`) generates one patch per base, each with its own merge
  base, and a single scalar left every patch after the first unplaceable.
  `base_sha` remains, derived from that array's first entry, so existing
  consumers keep working and the two cannot disagree. `checks[]` covers gates
  that never ran: a check ruled out during eligibility (tests disabled, a tool
  missing) was omitted entirely, which reads exactly like a gate that was never
  part of the run. Such a check now gets a row with every substrate field null
  and a `skipped` reason; rows for checks that ran carry `skipped: null`. A
  reviewer holding
  only the artifacts no longer has to reconstruct the run's substrate from
  scattered gate files. Purely additive: no existing pack file changed shape,
  the manifest hashes it like any other artifact, and the sanity
  `required_files` check now requires it.
- Check provenance now records the tree each gate actually scanned: `target_sha`
  (the commit whose tree the check read) and `tree_state` (`snapshot`,
  `snapshot-dirty`, `snapshot-borrowed-deps`, `local-clean`, `local-dirty` or
  `foreign`). Previously `cwd`
  was the only substrate
  signal, so an artifact pack could not prove whether a gate saw the reviewed
  commit or an operator's uncommitted working tree. Both fields are resolved
  from the directory the command ran in and surface in
  `20_quality/<gate>.result.json`, `20_quality/full-checks.log`,
  `00_summary/RUN.json` and `report.json`. They are additive and optional:
  consumers of older packs (and of checks that ran outside a git repository)
  keep parsing unchanged, so no artifact `schema_version` bump is required.
  The synthetic `heuristics_loctree` gate is covered too: it runs in-process
  rather than as a subprocess, but it still scans a tree — the `git archive`
  extraction of the target commit, or `repo_root` when no snapshot could be
  made — and its `PROVENANCE.json` row used to be entirely null, leaving one of
  the pack's gating signals unauditable. `HeuristicsResult` now carries the
  commit its analysis root was extracted from along with the scan's start and
  end times (all additive and optional).

### Fixed

- Cached check results now carry provenance. A cache hit used to return
  `provenance: None`, so the fastest runs — the ones where every gate is served
  from cache — were the only ones with no audit trail at all: no command, no
  `cwd`, no `target_sha`, no `tree_state`. Status, output and provenance are now
  stored as a single JSON cache entry and replayed together on a hit, describing
  the run that populated it; `cached: true` on the result is what marks the row
  as a replay rather than a fresh execution. The entry is published with an
  atomic rename from a staging file, so parallel prview processes on the same
  cache can never pair one run's result with another run's provenance. Entries
  written by an older prview are still read in their previous multi-file form
  (no cache invalidation, no cold rebuild) and are collapsed into the new shape
  the first time the key is rewritten. A check that *errors* keeps its substrate
  too: a command that times out or crashes used to produce a row with a null
  `cwd`, `target_sha` and `tree_state`, which are precisely the rows where
  "which tree produced this error" is the first question asked. The error path
  now reconstructs the directory the check was about to read without
  materialising anything, while stating what it does not know — `command` reads
  `<no command recorded>`, and an off-`HEAD` check whose own worktree is already
  gone keeps no provenance rather than naming the local checkout it was not
  reading. Cargo checks report the directory they were actually headed for
  rather than the scan root: a workspace member, or a crate the reviewed commit
  moved, runs one directory down, and that resolution is now shared with the
  planner instead of collapsed away.
- `Pytest` now runs in the reviewed target snapshot instead of `config.repo_root`.
  When reviewing a PR or a remote branch, `repo_root` still points at whatever is
  checked out locally, so pytest executed the *local* branch's tests and reported
  their failures against the PR — a false failure from unrelated code, even when
  the PR's own tests were green. Ruff, Mypy and the JS checks were moved onto the
  target snapshot earlier; `Pytest` was the one check left behind, and is now
  registered as a shared-snapshot check alongside them. Local reviews, where the
  target resolves to `HEAD`, are unaffected. Its recorded `provenance.cwd` now
  reports the directory the run actually used. Whether the Python checks apply
  at all is decided by the reviewed commit as well: a target that removed its
  last `pyproject.toml` and Python sources is still reviewed from a Python
  checkout, and pytest exited 5 for "no tests collected" — a blocking failure
  for a target the check no longer applies to, with Ruff and Mypy passing
  vacuously beside it. All three now skip with a reason when the reviewed tree
  carries no Python, resolved from git without materialising a worktree, and
  fail open whenever git cannot answer.
- The cargo checks (`Cargo check`, `Clippy`, `Rustfmt`, `Cargo test`,
  `Cargo audit`, `Cargo geiger`) now run against the reviewed target snapshot
  instead of the local checkout. When reviewing a PR or a remote branch, they
  executed at `cargo_cache_root` — the working tree of whatever branch happened
  to be checked out — so a remote-only pack combined the target's diff with
  build, clippy, test and fmt verdicts from unrelated local code. The build
  cache that motivated that shortcut is preserved by pointing `CARGO_TARGET_DIR`
  at a per-repo shared directory (`~/.prview/cargo-target/<repo>`), passed to
  the cargo child process only, so a fresh snapshot does not recompile the whole
  dependency graph and the operator's own `target/` is never written to. Local
  reviews, where the target resolves to `HEAD`, are unaffected: same cwd, no
  environment override. Whether cargo applies at all is decided by the reviewed
  commit as well: a branch that dropped its last `Cargo.toml` is reviewed from a
  Rust checkout, and the cargo gates used to report cargo's own "could not find
  `Cargo.toml`" as that commit's verdict. The manifest is now looked up in the
  target commit's tree — no worktree materialised to ask — and the checks skip
  with a reason when the reviewed commit is not a cargo project. A crate the
  reviewed branch merely *moved* (a root workspace pushed into `backend/`) is
  found where it now lives, as long as exactly one directory within two levels
  carries a manifest; several candidates skip with a reason naming them rather
  than guessing which crate the review is about.
- Cargo check cache keys now name the substrate they judge. The cached-result
  lookup happens before the target snapshot is materialised, so a `--pr` run
  could hit an entry a previous local run had stored under the same working-tree
  hash and serve the local checkout's verdict as the PR's. Keys now use the
  resolved target commit whenever it differs from `HEAD`, together with the
  repo-relative cargo root (`commit-<sha>-root-<hash>`, `-root-self` for the
  repo root): the same commit checked from the workspace root and from a
  configured member produces different check/clippy/audit/rustfmt results, and
  keying on the commit alone let a later run serve the other root's verdict. A
  target that commits no `Cargo.lock` is not pinned by its commit at all — cargo
  resolves the dependency graph as it runs — so those keys (and the local
  working-tree keys, which have the same gap) carry the day: repeated runs in a
  session still hit, tomorrow's run resolves again, the way `Cargo audit`
  already handles ageing advisories. The
  root is hashed rather than spelled out because a cache key is a file name —
  `crates/core` written verbatim named a file in a directory nothing creates, so
  the store failed and the slowest gates in the tool recomputed on every review
  of a workspace member. The local member key drops its `:` separator for the
  same reason (illegal in Windows file names); existing entries miss once and
  are repopulated.
- A `cargo_root` configured outside the repository no longer makes an
  off-`HEAD` review scan an unrelated directory. A snapshot of the repo can
  never contain such a root, and the fallback ran cargo at the local path
  anyway — the reviewed commit's name on a foreign tree's verdict, the same
  false-verdict class the snapshot move fixed. Those runs now **skip** the cargo
  checks with a reason naming the unreachable root; local reviews are unchanged.
  The same refusal now survives a target-controlled `backend/` **symlink** into
  an external directory, which carries no `..` and passed the lexical check:
  resolving the root from the git tree cannot follow a symlink out of the
  reviewed commit, and a resolved path that still leaves the snapshot is refused
  instead of producing a foreign tree's verdict cached under that commit. The
  same holds one path component deeper, for a reviewed commit that keeps the
  cargo root and replaces `Cargo.toml` *itself* with a link to an external
  manifest: git stores a symlink as a blob, so a plain tree lookup accepted it.
  A manifest must now be a regular file, and the containment check resolves the
  manifest alongside the directory for the cases the tree lookup cannot cover.
  A **local** review is one of those cases and was reached by neither guard —
  the local plan returns before the containment check runs — so a checkout
  tracking `Cargo.toml` as a link to an external manifest had cargo build a
  foreign project while provenance recorded the local checkout. The manifest is
  now resolved against the cargo root before a local plan is returned; an
  externally configured `cargo_root` whose own manifest sits inside it is still
  a legitimate local setup and is unaffected.
- A cargo root that the reviewed branch moved (a root crate pushed into
  `backend/`, a member renamed) is no longer projected into the snapshot
  verbatim. The locally detected path does not exist there, so cargo failed on a
  missing manifest and the execution error was reported as the reviewed crate's
  verdict; the run now falls back to the snapshot root when it carries a
  manifest of its own.
- Python checks no longer synchronise the operator's virtual environment when
  reviewing another commit. The target snapshot symlinks the checkout's `.venv`,
  and `uv run` syncs the project environment before executing — so reviewing a
  branch with different dependencies installed into, and removed packages from,
  the developer's active environment. Off-`HEAD` runs now set
  `UV_PROJECT_ENVIRONMENT` to a prview-owned directory keyed by the reviewed
  commit (`~/.prview/uv-env/<repo>/<target-sha>`): the reviewed dependency set
  is still installed and judged, just never on top of the operator's. Per-commit
  rather than per-repo, because `uv run` syncs before executing and releases its
  lock while the child runs — two reviews of different commits sharing one
  directory would resynchronise incompatible dependency sets under each other's
  running pytest. Runs of the same commit still reuse a warm environment, and
  the growth is bounded: the three most recently used environments survive, and
  nothing used in the last 24 hours is ever removed. That age floor is enforced
  under a `.prview-prune.lock` file at the environment root, so a second review
  cannot read a timestamp just before this one refreshes it and then delete the
  directory out from under a running `uv run`; a root already locked by a live
  review is left alone entirely. Local reviews set no override. Python runs also
  refuse a `pyproject.toml` or `uv.lock` that resolves outside the tree being
  judged — the counterpart of the Cargo manifest guards. A reviewed commit that
  tracks either as a link to an external file had ruff, mypy and pytest configure
  themselves, and uv resolve its dependency set, from another project entirely,
  while provenance recorded an exact snapshot scan and the verdict was cached
  under the reviewed commit. Metadata linked to a real file inside the tree
  resolves back inside and still runs.
- Provenance no longer certifies a tree it could not verify. A working-tree
  status that fails to read (an index lock, a permissions error, a malformed
  repository) recorded `local-clean` — the claim that the scanned bytes exactly
  match the commit, made precisely when nothing could be checked. It now records
  no `tree_state` at all, the same "visibly unknown" the non-git case uses. The
  pack-level `worktree.clean` had the same gap and is now `null` in that case
  instead of `true`: the value is published as a fact in `PROVENANCE.json` and
  decides whether out-of-diff failures are downgraded to pre-existing, so
  certifying an uninspected tree could silence real findings. A run with no git
  repository at all still reports `true` — nothing can be uncommitted without
  one, and such a run has no diff baseline to downgrade against anyway.
- A snapshot that a check wrote into is no longer recorded as an exact commit
  scan. `tree_state: snapshot` was assigned to any directory outside the repo
  root, so a generated `Cargo.lock` (or any tool writing into the checkout) left
  the artifact claiming bytes that had already changed, and an external
  `cargo_root` — a different checkout entirely — was labelled a snapshot of the
  reviewed commit. Snapshots are now verified against their own status
  (`snapshot` / `snapshot-dirty`, ignoring the `node_modules` and `.venv`
  symlinks prview itself creates), and a directory that is not a worktree of
  this repository is recorded as `foreign`. Those ignored symlinks are not free
  of consequence either: prview links the *operator's* dependencies into the
  snapshot rather than installing what the target's lockfile pins, so `tsc`,
  ESLint, Stylelint and Vitest read a compiler, plugins, type definitions and a
  runtime from the local checkout while the pack certified an exact target-tree
  scan — for a dependency-changing PR, the case where the two differ most. A
  snapshot that carries those links is now `snapshot-borrowed-deps`: the
  reviewed source is exactly the target, the dependencies are borrowed. A repo
  with no local dependency tree links nothing and stays `snapshot`, and the
  label is applied per check rather than per directory — a link only counts
  against a command that can read it. The JS checks resolve their toolchain
  through `node_modules`; cargo and Semgrep read nothing through it, so a mixed
  repository no longer downgrades their provenance, and the Python checks run
  against the per-commit `UV_PROJECT_ENVIRONMENT` rather than the linked
  `.venv`, so they stay `snapshot` too. Repository identity is now settled
  before position, in both directions: a check running in a vendored checkout, a
  submodule or an in-repo symlink to another clone used to be recorded as this
  repository's `local-clean`/`local-dirty` tree with the OTHER project's `HEAD`
  as `target_sha`, because sitting below `repo_root` was taken as proof. Such a
  directory is `foreign` wherever it sits.

### Changed

- Bumped the bundled `loctree` structural-analysis crate from `0.8` to `0.13.0`.
  The public API prview consumes (`analyzer::{cycles, dead_parrots, twins}`,
  `snapshot::{Snapshot, project_cache_dir, run_init, SNAPSHOT_SCHEMA_VERSION}`,
  `args::ParsedArgs`) is source-compatible — no call sites changed. The snapshot
  schema version is now decoupled from the crate version (pinned at `0.11.0`
  instead of tracking `CARGO_PKG_VERSION`); prview's `major.minor` schema gate
  handles the transition, so stale `0.8`-era caches are re-scanned automatically.
  loctree 0.13 also widens file-type coverage in the scan (markdown, shell,
  config, and other non-source files now count toward the snapshot), so the
  `LOCTREE` heuristics stats (`total_files`, `total_loc`, `by_language`) report
  higher, broader numbers than under 0.8 for the same tree.

## [0.6.0] - 2026-07-07

### Added
- add composite gate action
- add measured pre-push profile
- add gate subcommand with exit-code contract

### Changed
- test(checks): harden run_js_command local-bin test against ETXTBSY race
- test(gate): hide semgrep from exit-code fixtures
- test(gate): keep exit-three fixture outside parent repos
- test(gate): disable signing in git fixtures
- test(config): avoid manifest test self-spawn
- perf(checks): share one target snapshot across all checks in a run
- refactor(cache): use glob::Pattern::escape for repo-root glob escaping
- chore: declare rust-version (MSRV)
- test(gate): add end-to-end exit-code contract test
- refactor(githooks): collapse pre-push gate invocation to one line
- docs(gate): add rollout playbook and hook recipes
- docs(changelog): record loctree 0.13 adaptation
- build(deps): bump loctree 0.8 → 0.13.0

### Fixed
- fail fast without gate subcommand
- trust gate JSON sentinel for exit two
- prefer the live in-flight run for HEAD over a stale completed pack
- trust snapshot-backed linters in the pre-existing downgrade
- compute snapshot regression from the merge base
- fold workspace-root lockfile into member cargo cache keys
- warn on manifest read errors other than not-found
- unify [external]/ prefix across branches
- stop degrading clean semgrep scan on warning substring
- default RUNNER_TEMP in shadow gate workflow
- bash 3.2 safe array expansion
- distinguish usage error from conditional verdict
- point install/action defaults at released version
- handle analysis_status=incomplete explicitly
- clarify summary when failures degraded to advisory
- use identifier-boundary match for orphaned resources
- require module match in coverage stem strategy
- keep report.json verdict in sync with merge gate
- serialize run activation to close R2b TOCTOU
- add age signal to stale-lock detection
- fsync before rename in index save
- treat pid 0 as dead in liveness checks
- fail loud on unreadable MERGE_GATE in quick path
- surface in-flight runs in verdict without run_id
- skip corrupt index lines instead of truncating
- widen cache key hash to 16 bytes
- surface cargo audit informational warnings
- escape repo path in glob patterns
- distinguish rustfmt missing from formatting diff
- key rust checks by cargo_root manifest set
- run ruff/mypy/js checks against fetched target in remote mode
- populate range merge_base from diff base
- diff artifacts from merge-base
- default RUNNER_TEMP to /tmp in composite gate action
- add ~/.cargo/bin to PATH in pre-push
- fail-fast init phase in pre-push with set -eu
- treat mode-skip as caveat instead of blocking issue

## [0.5.0] - 2026-07-05

### Added

- `prview mcp --probe`: a fast stdio-server self-check that reports the server
  version, schema version, tool count, and response time (`--json` for
  automation). (#5)
- Curl-pipe installer (`install.sh`) that downloads checksum-verified (SHA256)
  release binaries, with a documented `cargo install --locked --force`
  fallback. (#4)
- `--security-full` flag: opt in to the full security tier, which adds
  `cargo geiger`'s unsafe-usage scan. Off by default (even under `--deep`).

### Changed

- **BREAKING:** Structural JS/TS heuristics are now served entirely by the
  built-in loctree signal (cycles, dead exports, unused symbols, exact twins).
  The `HeuristicsResult` no longer carries `madge`, `knip`, or `depcruiser`
  fields.
- **BREAKING:** `cargo geiger` is now opt-in via `--security-full` and is no
  longer part of the default `--deep`/`--ci` profile. It accounted for the bulk
  of deep-run wall time (minutes on large dependency trees) while source-side
  unsafe is already audited in-process. When not requested it is cleanly absent
  from the profile — not a skipped caveat — so it no longer affects the
  confidence or analysis status. `--with-security` still raises the heavy
  security posture but no longer pulls in geiger.
- Semgrep now scans only changed code by default: remote targets are scanned in
  an ephemeral worktree snapshot, with a full-scan fallback when the target is
  not checked out or more than one base resolves. This cuts a representative
  deep-run scan from ~24.5s to ~2.4s. (#8)
- MCP server now reports contract-honest state: running reviews surface as
  in-progress (not failed or complete), and the detected default base is
  validated and honored for remote targets. (#4)
- `run_id` allocation is shared between the CLI and MCP paths, uses the resolved
  target suffix, and retries on repo-wide allocation races to stay unique. (#5)
- Dashboard locale dictionaries are extracted to `locales/*.json` and loaded
  from there instead of being inlined in the renderer. (#7)
- crates.io publishing now uses OIDC trusted publishing instead of a stored
  API token. (#6)

### Fixed

- A missing `ruff` now reports as `Skipped` (with the spawn-failure reason)
  instead of `Failed`, matching `mypy`'s behavior. Previously any Python repo
  without ruff installed saw a false gate failure.
- Merge-gate pre-existing downgrade semantics hardened: the downgrade is gated
  on a clean-comparison signal and a resolved base diff, disabled under
  `--current-only`, scoped per-check on remote targets, blocked when a finding
  is unlocated or a tool's config is in the diff, and never applied to
  whole-project gate failures. Worktree cleanliness is now frozen before checks
  and artifact writes. (#8)
- `cargo audit` baseline is keyed by version so a vulnerable-version swap is no
  longer silently downgraded as pre-existing. (#8)
- rustfmt Diff-header parsing so out-of-diff formatting findings downgrade
  correctly. (#8)
- Dashboard cached locale JavaScript is now escaped. (#7)
- MCP hardening: probe child stderr is discarded, child review positionals are
  terminated, run-path ambiguity is canonicalized, ref-existence probes are
  qualified, and corrupt running entries are skipped. (#4/#5)
- `install.sh` falls back correctly on musl Linux and honors the requested
  install dir for the cargo fallback. (#4/#5)

### Removed

- **BREAKING:** Dropped the npx-based JS analyzers (`madge`, `knip`,
  `dependency-cruiser`) from the heuristics pack. Without an installed
  `node_modules` these tools always reported `not available`, so the promise
  was never backed by a signal; loctree already covers cycles, dead code, and
  twins for JS/TS in-process.

## [0.4.0] - 2026-07-02

### Changed

- **BREAKING:** Unify the merge-gate verdict vocabulary to `PASS` /
  `CONDITIONAL` / `BLOCK`. Legacy values (`ALLOW`, `HOLD`, `WARN`, ...) are no
  longer emitted; downstream consumers must migrate to the new set.
- **BREAKING:** Derive a single coherent decision surface from the verdict:
  `allow_merge` is now computed from the recommendation rather than tracked
  independently, and the process exit code follows the recommendation.
- **BREAKING:** `MERGE_GATE` artifact schema bumped to `2.1`.
- Unify licensing to `BUSL-1.1` across all surfaces (Cargo metadata, headers,
  docs).
- Deduplicate core logic paths (check-id derivation, process spawning, lexer)
  into shared implementations.

### Added

- MCP server: `prview mcp` subcommand exposing a stdio server with 6 tools over
  a normalized decision surface, including pid-liveness run status.
- `--no-color` now actually disables ANSI color output.
- Scope hardening: merge-base diffing, marker-gated deletion handling, and
  inclusion of untracked files under `--wip`.

### Fixed

- Close the spawn-hang class across production spawn sites (npx install prompt
  no longer blocks a run).
- Close the fail-open / self-signal class: loctree, geiger, `CONSISTENCY`,
  `PATTERN_SCAN`, `ghost_refs`, and sanity checks now report honest skips
  instead of silently passing, and relative `out_dir` is resolved correctly.
- Pack integrity: zip artifacts now carry correct metadata.
- Storage locking / TOCTOU races in the run store.
- TUI honesty: the Warnings state is reported truthfully.
- 33 + 11 review-thread fixes from PR review across the wave.

### Removed

- Dead CLI flags: `--open-summary`, `--use-bash-full`, `--verbose`,
  `--breaking-change`, and the `watch_mode` path.
- Unused dependencies: `tera`, `rayon`, `indicatif`.
- Duplicated githooks twin.

## 0.3.1 - 2026-05-04

### Added
- Add cache, loctree-suite, watch mode, HTML dashboard
- Implement Phase 1 and 2 Review Intelligence modules
- Introduce prview.toml for dynamic decouple and remove project-specific hardcodes
- Stabilize configuration architecture, fix drifting signals via CheckResult integration and integrate Faza 4 tests

### Changed
- refactor: split signal.rs monolith into signal/ module hierarchy
- docs: update architecture.md with Phase 1+2 modules and correct LOC counts
- docs: document prview.toml and .prview-policy.yml configuration
- docs: fix architecture.md gaps — missing modules, exports, patterns
- docs: align branch docs with main migration
- docs: add comprehensive documentation
- chore: ignore local dashboard ux artifacts
- chore(release): bump version to v0.3.1

### Fixed
- ghost_refs: match module references, not bare-stem substrings (P1-06) — eliminates false-positive flood when a common-stem file is deleted
- breaking: pair module moves and identical remove+readd so MERGE_GATE reports relocations/re-exports as non-breaking instead of mass removals (P1-08/P1-09/P1-10)
- unsafe_audit: exclude string literals, raw strings and comments (P1-07)
- unsafe_audit: credit SAFETY only from the comment portion, not raw line/string content
- coverage: count inline Rust tests and import-based matches as covered, requiring both `mod tests` and `#[test]` for inline coverage
- public_api: dedup symbols, classify `const fn` as function, gate JS exports to JS files, label `pub use` as re-export
- checks: degrade cargo geiger to skipped on timeout / virtual-workspace manifest and surface runtime skips in the gate (P2-09)
- checks: emit an honest skip reason when tsc is unresolvable at repo root
- artifacts: real AI_INDEX, structured Semgrep findings, introduced/preexisting inline split, exclude .DS_Store/Thumbs.db from manifest/ZIP
- semgrep: exclude vendored/minified/generated public_dist paths from scans
- bump rand 0.8.5 -> 0.8.6 (RUSTSEC-2026-0097 unsound advisory) (P1-05)
- unblock strict pre-push gate for signal artifacts
- clippy collapsible_if in ghost_refs test helper
- resolve 5 P1 and 4 P2 from deep audit, add 20 tests
- address Gemini/Copilot review — filename mismatch, strip_suffix, docs
- address Gemini review — dedup extension stripping, delegate deps extraction
- address PR review findings — i18n tests, visibility, plan accuracy
- remove unused CoverageFile import in risk.rs tests
- address split audit findings — dedup threshold, fix comments, mark plan done

## 0.3.0 - 2026-04-18

### Added
- CLI flag `--why-blocked` to explicitly explain merge gate decisions in the terminal.
- Enhanced `prview doctor` with unified branding (Vetcoders), monorepo detection, and profile-aware toolchain checks (pnpm, ruff, etc.).
- New security and quality rules to `semgrep.yml` (avoid-unwrap, path-safety).
- `make smoke-test` target to verify installation and binary health.
- `SemgrepCheck` integrated into `prview` checks, utilizing local `semgrep.yml` configuration.
- Artifact directory path displayed at the end of every run.

### Changed
- Unified project branding across CLI, `Makefile`, and documentation.
- `print_summary` now requires `Config` to respect output flags like `--why-blocked`.
- Stabilized the artifact pipeline and resolved naming inconsistencies.
- Enhanced descriptive text for merge gate verdicts.

## 0.2.0 - 2026-03-16

### Added

- Shell completions subcommand (`prview completions <SHELL>`) for bash, zsh, fish, elvish, powershell
- Import-based coverage matching for cross-layer test detection
- Consolidated `REVIEW_SUMMARY.md` artifact with gate + review + artifact map
- Narrative commit summary with thematic labels in per-commit diffs
- Twin symbol details and low test ratio warning in PR review
- Cargo audit/geiger specifics in `PR_REVIEW.md`
- User-friendly error messages with cause chain and resolution hints
- `PATTERN_SCAN.json` artifact (11 risk patterns: `.unwrap()`, `println!`, `dbg!`, `TODO`, etc.)
- `DEPS_DELTA.json` artifact (added/removed/changed deps from Cargo.toml/package.json)
- Unit tests for `extract_file_line_from_output`
- Code coverage CI job with cargo-llvm-cov

### Fixed

- SARIF fallback extracts source `file:line` instead of pointing to `full-checks.log`
- `warnings-only.log` filter uses per-check state instead of global accumulator
- Per-file diff uses `--` separators instead of URL-encoded `~2F`
- Source paths included in `00-INDEX.txt` for per-file diffs
- Update mode shows skipped-checks caveat when checks were not re-run
- `AI_INDEX.md` respects config flags for conditional sections
- Clippy `collapsible_if` warnings resolved
- 2 P1 and 4 P2 findings from self-review addressed
- Stylelint cache keyed on CSS/SCSS inputs instead of TS files
- CI pinned to Rust 1.92.0 to work around report-leptos query depth issue
- CI cross-build fixed: dropped `--all-features`, fixed x86_64-darwin target

### Changed

- CLI help texts improved with argument conflicts and value hints
- `is_test` renamed to `test_context_only` for PerfSuspect semantics
- Per-commit summary shows top-10 by churn when >50 commits (instead of skipping)
- Empty SARIF not generated; `report.json` `sarif_path = None` when no findings
- Heuristics loctree skips analysis when `total_files=0`

## 0.1.2 - 2026-03-14

Initial public release. This changelog covers the full feature set shipped in
v0.1.2, consolidated from 183 commits on the development branch.

### Added

- Core PR analysis engine with cross-language support (Rust, TypeScript, Python)
- 14 automated checks: clippy, cargo-audit, cargo-geiger, test runner, coverage
  delta, lint metrics, CODEOWNERS validation, dependency audit, and more
- Artifact Pack v1: structured output layout with numbered generators
  (`report.json`, `MERGE_GATE.json`, `RUN.json`, dashboard HTML)
- Interactive TUI with 6 panels: overview, diff preview, config editor, branch
  selector, check execution, and repo state
- HTML dashboard with sidebar navigation, severity badges, diff viewer,
  regression scores, and collapsible tiers
- `prview state` subcommand: fast repo probe (`--fast`, `--hot`, `--json`, `--tui`)
- Snapshot engine and regression detector for deterministic heuristics
- Policy system with Shadow/Warn/Block modes (`.prview-policy.yml`)
- Structured inline findings parsers (SARIF, clippy, eslint)
- Run storage at `$PRVIEW_HOME/runs/<repo>/<branch>/<ts>/`
  or `$HOME/.prview/runs/<repo>/<branch>/<ts>/` when `PRVIEW_HOME` is unset
- Remote mode for CI/headless environments (`--remote-only`)
- Update mode for iterating on open PRs
- Watch mode skeleton with git-status change detection
- Cache infrastructure for check results across commits
- `--quick` preset for fast local scans
- `--shell-setup` flag for alias onboarding
- loctree integration for structural analysis (cycles, dead exports, twins)
- Streaming check results with elapsed timer
- Deferred heavy diagnostics in fast runs

### Changed

- Migrated from bash prototype to pure Rust implementation
- Switched to Rust edition 2024
- Consolidated check orchestration with parallel execution
- Refined merge gate contract with verdict enum (Pass/Fail/Warn)
- Normalized all status values to lowercase in artifacts
- Improved heuristics naming: `unused_symbols` in user-facing output

### Fixed

- UTF-8 safe artifact truncation
- XSS prevention in dashboard (embedded JSON, copy button, search)
- Rename/copy detection in git diffs
- Coverage-delta matching with 4-strategy approach
- Per-file additions/deletions from git2 patches
- Python venv pre-sync before parallel checks
- TSan/UBSan false positive elimination in hard-fail signatures
- Cargo-geiger PascalCase output format for v0.13.0
- Watch mode change detection using full git status hash

[Unreleased]: https://github.com/vetcoders/prview-rs/compare/v0.6.0...HEAD
[0.6.0]: https://github.com/vetcoders/prview-rs/compare/v0.5.0...v0.6.0
[0.5.0]: https://github.com/vetcoders/prview-rs/compare/v0.4.0...v0.5.0
[0.4.0]: https://github.com/vetcoders/prview-rs/releases/tag/v0.4.0
