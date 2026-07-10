# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

> Entries prior to 0.4.0 document development that predates this repository's
> public debut. 0.4.0 is the first public release, so only versions from 0.4.0
> onward have git tags and comparison links.

## [Unreleased]

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
