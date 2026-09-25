# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

> Entries prior to 0.4.0 document development that predates this repository's
> public debut. 0.4.0 is the first public release, so only versions from 0.4.0
> onward have git tags and comparison links.

## [Unreleased]

### Added

- Release preparation is now an operator-selected, expected-SHA-pinned draft PR
  workflow. A separate fail-closed merge validator creates the immutable tag
  only for a machine-marked release PR, while the existing signed publish
  pipeline now adds concurrency and public cold-install, source-SHA, signature,
  provenance, and crates.io verification. Manual dispatch remains a
  non-publishing producer-path dry run.
- `prview gate --base <REF>` reviews the current checkout against an explicit
  branch, tag, or commit SHA instead of the auto-detected
  `develop`/`main`/`master` base. An explicit base that does not resolve exits
  `3` with an error naming the ref, rather than silently reviewing an empty
  change. `docs/gate-playbook.md` documents passing `github.event.before` on CI
  `push` events.
- `prview gate --base <REF> --exact-base` reviews `<REF>..HEAD` literally
  instead of normalizing the base to its merge-base with the target. Merge-base
  normalization is the three-dot review model and stays the default for every
  base; it is wrong for one question only ("what did this push deliver?")
  because on a force-push the pre-push commit is no longer an ancestor of the
  new tip, so normalization would review
  `merge-base(before, after)..after`, a different and larger range. The flag
  requires `--base` and affects only the requested base. When the range really
  was rewritten — the flag is in force and the pinned base is not an ancestor of
  the target — `PR_REVIEW.md` and `REVIEW_SUMMARY.md` state that once beside the
  base, so a reader knows why the file list can carry a change no listed commit
  made; it is a statement about range semantics, not a caveat, and touches no
  verdict or quality signal. `Gate Shadow` (`.github/workflows/gate.yml`) passes
  the flag on `push` events and records the range mode, including whether the
  push was a force-push, in its job summary.
- `--full-tests` runs every test regardless of what changed, disabling
  change-scoped narrowing for that run. The pack states
  `full test run requested (--full-tests)` as the reason, because a run that is
  wider than the change requires is still a fact about what executed. The flag
  short-circuits before any `cargo metadata`. `--ci` pins the same full run and
  publishes `full test run requested (--ci)`, so an existing automation job that
  already ran the whole suite keeps running it without changing its invocation;
  narrowing is for the reviewer's machine, and a local `--deep` stays narrowed.
  `prview gate` is unaffected, because the gate profile runs no tests.
- MCP `run_review` accepts `full_tests: bool` (default `false`), which passes
  `--full-tests` to the review. A review over MCP runs on the caller's machine
  and narrows its test suites like any local run, `deep` included; the argument
  is how a caller asks for the whole suite anyway.
- `PR_REVIEW.md` and `REVIEW_SUMMARY.md` state the test scope in prose
  (contract §7): a `## Test Scope` section with one sentence per scope-owning
  check that ran, rendered from the same `ScopeReport` the gate rows publish.
  A reader who never opens `MERGE_GATE.json` is no longer left to assume the
  whole suite ran.
- Vitest's published `selected` count is test files, not changed sources.
  Contract §7 counts selected units, and a Vitest unit is a test file; the count
  now comes from the run's JSON reporter (`testResults`) instead of from the
  number of changed source files handed to `vitest related`. A narrowed run
  whose reporter could not be read publishes `0`, because nothing was proven
  collected. Cargo still counts workspace packages, and the `selector` is
  unchanged in both.
- An empty test selection publishes its own proof. A `Cargo test` / `Vitest`
  row that runs nothing because the change has no related test now records
  provenance with `command: "<no command recorded>"` and
  `executed_scope: {"mode": "nothing-selected"}` instead of leaving provenance
  blank. The task ledger reads it as a skip rather than a run of a few
  microseconds, the pack publishes `mode: change-scoped, selected: 0` only
  against that record, and the merge policy's empty-selection exception requires
  it: a required test gate that reports "no tests related to the change" while
  recording nothing about what it decided now blocks, where it used to approve.
  The artifact contract is unchanged — the new value lives on the check's own
  provenance, and `scope` keeps the shape `3.1` already defines.
- Test scope is decided from the change and reported. A new `checks/scope/`
  decides, per ecosystem, whether a change requires the whole test suite or a
  narrower run, and publishes the answer as the additive `scope` object on the
  owning check's row in `RUN.json`, `report.json` (schema `3.0` → `3.1`) and
  `MERGE_GATE.json` (schema `3.0` → `3.1`). Both schema versions stay readable
  and valid; `tools/validate_merge_gate.py` accepts `3.1` and validates `scope`
  when present. `Config::changed_paths` carries the run's `ChangeSet` as
  internal runtime state (never a CLI or manifest override), and Rust packages
  are resolved with one `cargo metadata --no-deps --frozen` per run, read from
  the cargo root inside the REVIEWED tree — metadata only, not the
  network-capable full resolve. Every doubt escalates to a full run with a
  stated reason. A dirty operator checkout is deliberately NOT such a reason when
  the checks read a snapshot; it IS one when the checks read that tree
  themselves.

- Changed paths are classified in three states for test selection: `relevant`
  (recognised Rust / JS-TS source), `non-participating` (a narrow, named class
  we can say is not an input) and `unknown` (everything else, which still
  escalates to a full run). The classification affects test selection ONLY — a
  non-participating file still appears in the diff, the artifacts, the signals
  and the verdict. The built-in rules are `root-changelog`, `root-license`,
  `docs-directory` (prose extensions under a top-level `docs/` or `doc/`) and
  `ci-workflow` (`.github/workflows/*.y[a]ml`). Translations, fixtures,
  `tools/`, the root `README` and Markdown outside a documentation directory are
  deliberately NOT neutral — a crate can pull its README into the build with
  `#![doc = include_str!("../README.md")]`, and locale files are routinely
  compiled in the same way. A repository can extend the list with
  `[scope] non_participating` in `prview.toml` or turn the built-ins off with
  `[scope] non_participating_builtins = false`, which restores strictly
  escalating behaviour. Every neutral path is published with the rule that named
  it, in the `scope` object and as a `## Test scope` table in `MERGE_GATE.md`,
  so the call can be challenged without reading the source.

- **Library API (source-incompatible for consumers).** `Config` is re-exported
  as `prview::Config` and its fields are public, so downstream code that builds
  it with a struct literal must now also set the fields this release adds:
  `changed_paths`, `scope_non_participating` and
  `scope_non_participating_builtins`. All three are runtime-only state, kept on
  `Config` deliberately — the same established pattern as `pinned_target`,
  `pinned_diff_bases` and `scan_dir_override`, joined in the same release by
  `test_scope`, `operator_worktree_clean` and `full_tests`. The next release is a
  minor version bump. Contract and rationale: `docs/architecture.md` ("How much
  of a test suite must run").
- `--deadline <TIME>` and `--no-deadline` bound a whole run, or remove the
  bound. The value needs a unit (`90s`, `45m`, `2h`); a bare number is rejected
  rather than guessed, as is a value that overflows or that the runtime timer
  cannot represent, and the two flags conflict. `--deadline` is also refused
  with `--watch`, which is a session rather than a run and is never bounded — an
  accepted-then-ignored budget is worse than no budget. The deadline is a second
  implementation of the existing `Interrupts` trait, so an expiring run takes
  the same path a Ctrl-C takes: admission closes, the child tree is killed, the
  worktrees are removed, and no verdict is published. Contract and rationale:
  `docs/architecture.md` ("Run deadline"), `docs/usage.md` ("Run deadline").

### Changed

- **`checks[].tree_state` has a third snapshot value: `snapshot-unproven-deps`.**
  A JS gate's provenance used to have two answers for three facts, so
  `snapshot-borrowed-deps` carried both "these bytes came from outside the
  snapshot" and "this closure could not be read". Since prview recognises no
  real `npm`/`pnpm`/`yarn` shim grammar, the second meaning swallowed the
  ordinary case and a fully target-owned toolchain reported borrowed. The new
  value says what is actually known — the reviewed source is exactly
  `target_sha`, the dependency closure is unread — and `snapshot-borrowed-deps`
  goes back to meaning a borrow that was positively observed. Read
  `snapshot-unproven-deps` as cautiously as `snapshot-borrowed-deps`: it is not
  an exact scan. Existing values are unchanged, and test/package selection
  treats the new state exactly like the other two exact-source snapshots.
- **A tool with no `#!` is no longer certified as an exact snapshot scan, and
  neither is one that merely opens with an object-file magic.** "No shebang" was
  standing in for "native binary, no indirection". It is the opposite: prview
  spawns through `Command`, hence `execvp`, and POSIX requires `execvp` to retry
  an `ENOEXEC` file through `/bin/sh` — so such a file is a shell script with
  unbounded indirection. Deleting one `#!/bin/sh` line was enough to flip a run
  that executed the operator's uncommitted bytes from `snapshot-borrowed-deps`
  to `snapshot`. Recognising a four-byte magic does not close that hole, because
  `ENOEXEC` is returned by the *loader*, after the whole header: prefixing the
  same launcher with `\x7fELF` — or even with the host's own `CF FA ED FE` —
  still reaches `/bin/sh`, and merely recognising the prefix turned silence into
  a false positive claim. Target-only closure is now proved only by a **fully
  validated platform header for the running kernel**: on macOS a complete
  `mach_header` with the host `cputype` and `MH_EXECUTE`, or a 32-bit universal
  binary in which **every** host-`cputype` slice is claimable; on Linux a
  complete ELF header with the host `e_machine`, `ET_EXEC`/`ET_DYN`,
  `e_phentsize` equal to `sizeof(Elf64_Phdr)` and a program-header table within
  the kernel's `56 * e_phnum <= 65536` bound; on any other Unix, nothing. A
  format this kernel has no loader for — ELF on macOS, Mach-O on Linux — is
  recognisable but not executable, so it is the fallback vector rather than
  evidence. The proof reads a bounded header window and so still runs before the
  script size bound, leaving a large compiled tool able to prove its own kind
  while an oversized file with no claimable header stays unproved.
- **Validating a header is not the same as predicting the loader's verdict, and
  the proof now says so.** An earlier draft of this change claimed a completely
  validated header leaves "only two futures … with no shell in the path". That
  was false where it mattered most: macOS grades fat slices (`arm64e` outranks
  `arm64`, `x86_64h` outranks `x86_64`, under one `cputype`), so accepting
  because *some* host slice validates certified images the kernel hands to
  `/bin/sh` — measured on macOS/arm64, a real `arm64` binary beside a bogus
  `arm64e` entry ran under the shell at exit 126, as did every `FAT_MAGIC_64`
  image with a real, working slice inside it. Both shapes reported `snapshot`,
  the state that promises the scanned bytes are exactly `target_sha`. So the
  accepted set is narrowed to what a kernel probe measured with zero fallback:
  all host slices claimable for fat, `FAT_MAGIC_64` recognised and refused, and
  on Linux the whole of `load_elf_phdrs()`'s arithmetic reproduced rather than
  half of it: `e_phentsize` matched for equality, and the program-header table
  refused when `56 * e_phnum` is zero or above 65536, because every one of those
  exits is the same `ENOEXEC`. Modelling only the lower half of that bound left
  `e_phnum` = 1171 claimed here and dropped to `/bin/sh` there. One thing is
  named rather than relied on: bash refuses a file carrying a NUL before its
  first newline, and every
  accepted macOS header happens to carry one, so no accepted file ran as a
  script even before this fix — that is an accident of the binary formats, not
  part of the contract, and `dash` makes no such promise.
- **Every run is now bounded in time.** A local review, `--tui`, and a detached
  MCP `run_review deep` get 30 minutes; `--ci` and `prview gate` get 60. (An MCP
  `quick` review keeps its own, tighter 120-second server budget.) The numbers
  come
  from measuring the heaviest workload the project runs on itself — a full
  `--deep --no-cache` review of prview-rs takes 616 s on a 14-core host and
  514 s on a 24-core one — and leave roughly a 3× margin. `--watch` and the
  startup preflight stay
  unbounded on purpose. A run that was previously able to hang forever now ends;
  a run that finished before still finishes.
- **A run stopped by its deadline exits `3`, not `130`.** `130` means the
  operator cancelled; a deadline is prview failing to reach a verdict in the
  time it was given, which is what `3` already means everywhere else. The
  governor remembers which of the two happened (first reason wins), and when a
  deadline reaches artifact generation `00_summary/INCOMPLETE.json` says so:
  `reason: "deadline exceeded"` plus `deadline_secs`, additively, with
  `schema_version` unchanged at `1.0`. An operator interrupt still writes
  `reason: "cancelled"`.

- Test execution now obeys the scope decision. `Cargo test` appends `-p <pkg>`
  for each selected package, before the optional positional test filter;
  `Vitest` switches to `related --run --maxWorkers 1 --passWithNoTests <inputs>`.
  A decision that selects nothing executes nothing at all and returns `Skipped`
  with `no tests related to the change` — never a pass, since no suite ran. A
  narrowed Vitest run is judged by Vitest's JSON reporter rather than by its exit
  code or its prose: the narrowed command carries
  `--reporter=default --reporter=json --outputFile.json=<temp file>`, zero
  suites with no collected file is `Skipped` for the same reason as above,
  executed tests hand the verdict back to the exit code, and a missing or
  unreadable report is an `error` (`could not verify that the narrowed Vitest run
  executed any test`) — never a pass and never a skip, so no output a test prints
  can spoof either. A full decision runs exactly the command it ran before, and
  `--tests-pattern` keeps filtering INSIDE the selection. Escalation continues at
  runtime and stays one-directional: a selector input missing from the tree the
  runner is about to read (checked now by `Cargo test` as well as `Vitest`), or a
  package name that cannot be spelled on a command line, runs the full suite with
  the reason stated.
  The decision is resolved once, immediately after the run's shared snapshot is
  settled and before any check runs, and only when a gate that owns a test scope
  is actually runnable.
- The reported scope describes what EXECUTED. `CheckProvenance` carries an
  additive `executed_scope` (`report.json` / `MERGE_GATE.json` stay schema 3.1;
  all changes are additive), and `mode: "change-scoped"` is published only
  against that evidence — a scopeable decision from a check that left none is
  reported as `full` with `scoped execution not confirmed by the check`. The
  published `selector` is a verbatim fragment of the command the check ran, and
  `tools/validate_merge_gate.py` now rejects a `selector` beside `mode: "full"`
  as well as a `selected` count, since either makes a pack contradict itself
  about whether the whole suite ran.
- A test gate skipped because the change touches nothing it covers no longer
  blocks a merge in a repository that requires it. `Skipped` carrying prview's
  own `no tests related to the change` is `Satisfied / Complete / Approve` — the
  check applies to the repository but not to this change, proved by a
  classification that escalates everything it cannot name. The exception is
  keyed on evidence, not on the sentence: only a check that owns an ecosystem's
  test scope, only from a real execution (never a pre-flight skip), and only
  when its own provenance agrees (no command at all, or a recorded narrowed
  run). A lint that prints those words, and a test check that ran the full suite
  before reporting them, keep blocking. The row still reads `skipped` with
  `outcome: skipped`; every other skip reason keeps its existing policy outcome,
  including a missing tool, which still blocks.
- A snapshot the ledger reports as dirty, or whose substrate cannot be
  identified, now escalates to a full test run: the bytes the gates read are
  then not the reviewed commit, and a selection drawn from a diff that does not
  describe them would be a silent narrowing.
- Test selection reads only the built-in half of the generated-output
  predicate. Folding in the operator's `[lint] ignore_patterns` was harmless
  while nothing narrowed; now it would silently stop testing a source file
  because someone chose not to lint it.
- Library API: `prview::GateArgs` gains the public fields `base` and
  `exact_base`, and `prview::Config` gains the public fields `required_base` and
  `required_base_exact`; code that constructs either with a struct literal must
  set the new fields. `prview::git::Repository::resolve_diff_bases` takes
  `&Config` in place of its trailing `quiet: bool`, since the range mode is read
  from the same runtime state as `quiet`. This is source-incompatible for
  library consumers, so the next release is a minor version bump.
- `install.sh` is fail-closed. It installs an official release binary or it
  installs nothing: the `cargo install` fallback is gone, along with every code
  path that could build, compile, or clone on the user's machine. `latest` is
  resolved to a concrete tag before downloading, the archive is matched against
  its exact `SHA256SUMS` entry, the archive must contain exactly one regular
  file named `prview`, unpacking happens in a temporary directory, and the
  binary is installed atomically with `install -m 755`. On macOS the binary must
  pass `codesign --verify --strict`, report Team ID `MW223P3NPX`, and be accepted
  by Gatekeeper's primary-signature assessment
  (`spctl -a -t open --context context:primary-signature -vv`), which must
  report `source=Notarized Developer ID`; a Developer ID signature without a
  notarization ticket reports plain `source=Developer ID` and is rejected. There
  is no bypass environment variable. The
  installed binary is then executed: `--version` must match the resolved tag and
  `--build-source-sha` must be a 40-hex commit. Consequences by design: on macOS,
  unsigned releases up to and including v0.7.0 are rejected (exit 5), and any
  release whose binary reports `--build-source-sha` as `unknown` is rejected
  (exit 6). New environment variables `PRVIEW_VERSION`, `PRVIEW_BASE_URL`, and
  `PRVIEW_MACOS_TEAM_ID` join `PRVIEW_INSTALL_DIR`; documented exit codes are
  0 ok, 1 tooling, 2 unsupported platform, 3 missing artifact, 4
  checksum/archive invalid, 5 macOS signature/notarization, 6 post-install
  verification. `docs/INSTALL.md` carries the full contract.
- Library API: `prview::checks::run_js_command` and
  `run_js_command_with_timeout` return `JsRun { program, output }` instead of a
  bare `std::process::Output`. The recorded provenance of a JS check is now
  built from `JsRun::command(&args)` — the program the OS was actually handed —
  so a pack no longer reports `pnpm exec eslint …` for a run that executed
  `node_modules/.bin/eslint` directly. The published command was previously
  reconstructed from a second, independent `which::which("pnpm")` probe that the
  runner never consulted, which could name a launcher the run did not use and,
  under `--target-sha`, a launcher outside the snapshot. Callers that only need
  the process result read `run.output`; this is source-incompatible for library
  consumers.

### Fixed

- Exact reviews now derive their check set and reported project profile from
  the pinned target tree. A dirty operator checkout can no longer hide JS,
  Python, or Rust checks by removing local project markers; ordinary local
  reviews continue to reflect the live checkout. Programmatic `App::from_config`
  callers keep a supplied `Config.profile` unless they set
  `requested_profile: Some(Profile::Auto)` to opt into target-derived detection.
  Changing the public profile after `Config::from_cli` also preserves that
  explicit programmatic override.

- **A snapshot never writes through an entry the reviewed commit owns.** The
  dependency merge decided whether the target already had `node_modules` /
  `.venv` with `Path::exists()`, which follows symlinks. A commit carrying
  `node_modules -> /some/writable/path` therefore looked like a plain directory,
  the merge opened, and every "missing" package was created **inside the
  operator's own filesystem** — outside the snapshot, at a location the reviewed
  branch chose. The decision now reads the commit's own entry with
  `symlink_metadata`: only a real directory receives borrowed entries, an absent
  entry receives one whole borrowed link, and anything else the commit spelled
  (a symlink, resolving anywhere or nowhere, or a file) is left exactly as it
  is — no merge, no borrow, no failure. `create_borrowed_link` additionally
  re-proves per link that the parent it is about to write into canonicalizes
  inside the snapshot root, because `strip_prefix` compares spelling, not
  identity.
- **A broken dependency symlink in the reviewed commit no longer aborts the
  review.** `node_modules -> nowhere` made `exists()` report `false`, the borrow
  was attempted anyway, and `symlink(2)` failed `EEXIST`, so snapshot creation —
  and with it the whole run — died on a repository that is merely unusual. Such
  a commit is now left alone and reviewed.
- **The exposure of top-level packages no longer depends on the operator having
  a `.bin`.** The merge was gated on `ambient_bin.exists()`, so an operator
  install without `node_modules/.bin` suppressed the package merge as well, even
  though what a shim resolves is `../<package>`. The two merges are now
  independent, and `.bin` is simply one more entry the top-level merge can
  expose.
- **A `.bin` shim that resolves inside the tree to an entry the commit does not
  contain is `Missing`, not `Unresolved`.** Following a repository-relative link
  lands on another path in the *same* tree, so the tree can answer for it.
  Reporting `Unresolved` made JS eligibility treat an absent tool as a target
  candidate; the check was scheduled and then failed at execution time with
  `resolved JS tool disappeared` instead of being skipped with a reason.
- **The recognized pnpm wrapper grammar admits only a literal payload.** The
  grammar accepted any text between the quotes of `exec node "$basedir/…" "$@"`,
  while a second, looser scan extracted the closure path. A wrapper reading
  `exec node "$basedir/$HOME/x" "$@"` therefore proved a closure whose contents
  the shell picks at run time, and the two passes could disagree — the extractor
  could record nothing while the closure stayed "proved". The payload must now
  match `[A-Za-z0-9._@+/-]+` (nothing the shell expands, splits or globs) and is
  taken from the very match that recognized the grammar, so the recognizer and
  the recorded closure cannot diverge.
- **On non-Unix, an invocation that exists is no longer certified as an exact
  snapshot scan.** The non-Unix closure proof answered `TargetOnly` for every
  path, so a Windows run published `tree_state: "snapshot"` — "the scanned bytes
  are exactly `target_sha`" — for a launcher this build cannot read at all
  (there is no header proof and no script grammar there). It now answers from
  canonical identity alone: nothing at the invocation path is `TargetOnly`
  (nothing will execute), an invocation resolving outside the snapshot root is
  `Borrowed`, and everything else is `Unproven`. All three variants are
  therefore constructed on every platform, which also removes the `dead_code`
  asymmetry that broke the Windows build.
- **Two claims about the proof were stated more broadly than the code
  supports, and are corrected in place** (`ClosureProof::TargetOnly` docstring,
  `docs/architecture.md`): `snapshot` never promised that "every statically
  visible byte is target-owned" — the ambient `node` runtime is outside the
  proof and always was, prview ships none; and the `/bin/sh` retry that
  motivates the platform-header proof happens on the `fork`+`execvp` path, not
  on Rust's default `posix_spawn`, which hands `ENOEXEC` straight back
  (`Exec format error (os error 8)`, measured on Linux CI). The header proof is
  justified by caution, not by a universal law. An unrecognized wrapper is
  `snapshot-unproven-deps`, never "borrowed".
- The `PR_REVIEW.md` PR Template checklist no longer claims what the checks did
  not prove. `Compiles / type-checks`, `Tests pass` and `No lint errors` were each
  ticked when ANY check of the category passed, so a failing ESLint hid behind a
  passing Clippy, and a run with no checks at all ticked `Compiles / type-checks`.
  A claim is now ticked only when at least one check of its category executed and
  every executed one passed, and Mypy counts toward `Compiles / type-checks`.
  `CONSISTENCY_CHECK.json` and `report.json`'s `quality.consistency` re-derive the
  three claims from the check statuses and report a rendered mark those statuses
  do not earn as a `pr_checklist.<item>` warning; a checklist line or serialized
  check entry that cannot be read (including a status outside the serialized
  vocabulary) is reported too, never skipped. Only the checklist inside the PR
  Template's fenced block counts, so text elsewhere in `PR_REVIEW.md` cannot
  stand in for it, and each item must be named by exactly one line there: a
  duplicated item is unreadable rather than read from its first copy.
  A heading-shaped line in earlier check evidence does not create a second
  template, and a present but unreadable `report.json` now withholds the
  checklist comparison with an explicit warning. An unreadable `PR_REVIEW.md`
  does the same instead of passing as an absent checklist.
  Template parsing now anchors to the final generated separator, so a
  newline-containing Git path cannot impersonate a second template; paths are
  rendered with escaped control characters on one line, including loctree twin
  pairs. A genuinely duplicated
  complete template remains unreadable. Cached
  check replays no longer count as execution toward an auto-ticked claim;
  their serialized `cached` flags are checked alongside statuses. Serialized
  check names are also matched to their canonical IDs and to the complete
  executed check set in `MERGE_GATE.json` (name, status and cache state), so an
  alias collision, omitted failed row, or missing gate cannot silently change a checklist
  claim. Custom check names remain valid when both artifacts agree.
- A `Cargo audit` failure whose advisories the baseline already proved
  pre-existing no longer blocks the merge because of unrelated uncommitted
  changes. The pre-existing downgrade for this one check now rests on lockfile
  provenance — the audited `Cargo.lock` is the analysed target's — instead of
  whole-tree cleanliness, which is evidence about source files and says nothing
  about an advisory that lives in `Cargo.lock` × the advisory database. A pack
  could previously carry `Cargo audit baseline: new=0, pre-existing=2` in its
  review caveats and `BLOCK … Cargo audit (Failed)` in its decision with nothing
  bridging the two. Dirt in the lockfile itself still revokes the downgrade, an
  introduced advisory still blocks, and a changed lock with no base audit is
  still unclassified rather than assumed clean. The proof also requires that the
  target tree actually carry a `Cargo.lock`: `cargo audit` resolves one from the
  registry when a crate has none and audits that, so advisories from such a run
  are real but concern a file no commit contains, and they now keep gating
  instead of being reported as unchanged. That lockfile is the one the audit
  reads — `Cargo.lock` in the cargo root itself, since `cargo audit` never falls
  back to a workspace root's — so a root lock beside a lock-less member, or a
  symlink committed in the lockfile's place, proves nothing; and a reviewed
  commit that moved its crate away from the configured cargo root withholds the
  proof, because cargo ran in a directory the lockfile questions were not asked
  about. The lock must also stay the committed one while the checks run: none
  of prview's cargo commands pass `--locked`, so a target that adds a dependency
  without regenerating `Cargo.lock` has the lock rewritten before the audit reads
  it. A snapshot run whose check-boundary observations saw the audited lock
  change, or could not be read, withholds the proof, and a local run reads the
  audited lock again after the checks. A repository whose committed `Cargo.lock`
  does not cover its manifest has it rewritten by every cargo run, so it no
  longer earns the pre-existing downgrade until the regenerated lock is
  committed; the gate says so as `Cargo.lock dirty or rewritten in the scanned
  tree`. A new `warnings`-category advisory (`unmaintained`, `unsound`,
  `yanked`) blocks the downgrade like a new vulnerability: it has no
  vulnerability row of its own, so it reaches the classifier as a dashboard
  note, and a changed lock that kept an old vulnerability while adding one no
  longer passes as pre-existing. A yanked crate is part of that set too:
  cargo-audit reports it with no advisory, and it used to be dropped from the
  comparison while the check status still counted it; it is now keyed as
  `yanked`, and any vulnerability or counted `warnings` item that cannot be
  keyed makes the report unreadable rather than invisible — a vulnerability
  missing its advisory id or locked version no longer shares a placeholder key
  with an unrelated malformed one in the base. Two items that share a key (the
  key names no package source, and rustsec's yanked check accepts both
  spellings of the crates.io index) make the report unreadable too, instead of
  shrinking the compared set. The base audit reads the base's copy
  of the lockfile the audit read: a member that gains its own `Cargo.lock` is
  no longer compared against the repository-root lock (a superset of every
  member's resolution), so an advisory the new member lock introduced is no
  longer classified as pre-existing — that baseline is unavailable instead. A
  change to the cargo-audit configuration (`.cargo/audit.toml` in the cargo
  root, whose base copy no audit reads) withholds the proof as `the
  cargo-audit configuration (.cargo/audit.toml) changed or is dirty in the
  scanned tree`, so dropping an ignored advisory no longer passes its failure
  off as pre-existing. So does a configuration in the checkout that is not the
  target's, whether staged, unstaged, untracked or ignored: it could ignore the
  advisory a change introduced while the pre-existing ones still fail and are
  downgraded. A configuration committed under another case (`.cargo/Audit.toml`,
  `.CARGO/audit.toml`), which a case-insensitive checkout reads, withholds the
  proof as `.cargo/audit.toml is committed under another case, which a
  case-insensitive checkout reads`. A lock that was staged and then reverted in
  the working file no longer reads as untouched: the local re-read checks the
  index and the working tree separately. Nor does a lock or configuration
  edited under a skip-worktree or assume-unchanged flag, which status and the
  index's view hide: the re-read also compares the working tree with the target
  commit directly, and the snapshot's check-boundary observations
  (`SNAPSHOT_INTEGRITY`) read such entries on disk the same way. A snapshot
  run whose target has no configuration withholds the proof when a check left
  one at the path in the snapshot, which no boundary lists as untracked. A
  relative Cargo home (`CARGO_HOME`, else `$HOME/.cargo`), inherited by the
  checks, resolves inside the scanned tree, where cargo-audit's fallback
  configuration and advisory database then live; it withholds the proof as
  `the Cargo home (CARGO_HOME, else $HOME/.cargo) is relative, so cargo audit
  read its fallback configuration and advisory database inside the scanned
  tree`. An absolute one that is, or lies inside, the checkout or the snapshot,
  however it is spelled or linked, withholds it the same way, as `the Cargo
  home (CARGO_HOME, else $HOME/.cargo) lies inside the checkout or the scanned
  tree, so cargo audit read its fallback configuration and advisory database
  from files there`. With `CARGO_HOME` unset, a `HOME` inside the tree counts.
  An external home whose `audit.toml` or `advisory-db` is a link into either
  tree, or a configuration (committed or fallback) whose `[database] path` is
  relative or leads into either tree, withholds the proof as `cargo audit's
  fallback configuration or advisory database is not shown to lie outside the
  checkout and the scanned tree (a link or a configured database path leads
  there, or it could not be read)`.
  The gate states which proof it
  applied: a downgraded audit reads `pre-existing: Cargo.lock unchanged by this
  PR (N advisories)`; a blocking one names the advisories it blocks on — all of
  them, counted and named from one set, so an `unmaintained` warning is no
  longer counted as a "vulnerability" nor silently left unnamed, and they are
  called "introduced" only while the lockfile proof holds — and an audit that
  blocks for want of the proof says which premise was missing instead of
  reporting a bare `Cargo audit (Failed)`. The dashboard now states the gate
  verdict as a `data-merge-verdict` attribute on the merge chip, so its parity
  with `MERGE_GATE.json` is assertable rather than assumed. An audit whose only
  items are pre-existing warnings-category advisories is downgraded the same
  way: each warning reaches the classifier with the origin the baseline counts
  give it, where it used to have no row and held the gate at CONDITIONAL. The
  proof describes only an audit this run executed, so `Cargo audit` replays
  only a `passed` result from the check cache; a failing or warning report,
  which the downgrade reads, always runs live, because the cache key does not
  bind `.cargo/audit.toml`.
  `docs/contracts/merge_gate.md` and `docs/architecture.md` carry the rule.

- `30_context/GHOST_REFERENCES.*` now audits the reviewed tree instead of the
  operator's checkout. For an off-`HEAD` target the scan walks the shared target
  snapshot the rest of `30_context/` is planned from, so untracked or dirty
  local files no longer surface as ghost findings that belong to no PR, and a
  file the PR deletes but the checkout still holds no longer passes for a
  relocation survivor that suppressed the real deletion. Only a local review
  (`target == HEAD`) scans the checkout, because there it is the reviewed tree.
  Both the relocation guard and the scan also skip `node_modules`: a snapshot
  links it in as a symlink the walk does not follow while a local review walked
  it for real, so a vendored file with the deleted file's name could silence a
  real deletion in one mode only.
- `prview gate --base <REF>` is pinned to a commit before the review starts. The
  review opens with `git fetch --quiet --prune origin`, and base resolution drops
  a ref it cannot resolve, so a `--base origin/<branch>` whose upstream branch
  had been deleted was pruned away mid-run: the review lost its base, reviewed an
  empty change, and passed. The resolved commit id is handed to the review
  instead of the ref name, and a pinned base that is still missing when the review
  resolves its bases exits `3` naming the ref the caller typed, rather than
  reporting a verdict. Pack headers are unaffected by the pin: `PR_REVIEW.md`,
  `AI_INDEX.md` and `report.json` keep the ref as it was written and now show the
  reviewed commit beside it.
- Annotated tags used as a base or target now resolve to the tagged commit.
  Ref resolution returned the tag object's id, so merge-base and diff lookups
  failed with a git error (for example `prview gate --base v0.8.0`). A ref that
  does not point to a commit, such as a tree or blob id, is reported as
  unresolvable.
- This repository's `Gate Shadow` workflow now reviews the change a push
  delivered. On a push to `main` the gate auto-detected `main` as the base while
  `main` was also the checked-out target, so it reviewed an empty change. Push
  runs now pass the pre-push commit (`github.event.before`) as `--base`, falling
  back to auto-detection for new-branch pushes or an unavailable pre-push commit;
  the job summary records which base was used. Pull request and manual runs are
  unchanged.
- The check progress line reported the STAGE's wall clock as the running
  check's elapsed time, so `Running: Vitest (2000s)` could mean a Vitest
  admitted thirty seconds ago behind half an hour of queue. Combined with a
  bare `Queued:` list, an ordinary `--resource-budget safe` run — one permit,
  fair-FIFO, every check admitted one at a time by design — read as a hang, and
  operators aborted healthy runs. Each running check now reports its own
  elapsed time, measured from admission (`Running: Vitest (312s)`), and the
  queue states that it is waiting on run resources
  (`Queued: waiting for run resources — Cargo check, Clippy`) without claiming
  which one, since a queued cargo check may be held by the shared `target/`
  lock rather than the machine budget. No timeout is printed beside the
  counter: elapsed runs from admission while the timeouts run from command
  spawn, and checks that probe first (Pytest, `cargo geiger`) would otherwise
  render impossible pairs such as `931s/900s`. The resource contract itself is
  unchanged: same budget, same weights, same child-worker caps.
- The curl installer no longer silently substitutes a locally compiled binary
  for an official one. Previously a failed download, a missing artifact, or an
  unsupported platform fell through to `cargo install prview --locked --force`,
  so a user who asked for a checksum-verified release could receive an
  unverified source build — or, with `cargo` absent, only learn about it at the
  end. Unsupported platforms now fail immediately with exit 2 and a message
  naming the two supported targets.
- The Windows CI job compiles the `json_contract` integration test in an
  untimed step before the contract-scanner proof runs under its 3-minute
  limit. On a cold cache after a `Cargo.lock` change, compilation alone took
  almost four minutes and the timed step failed before any test ran; the
  limit now covers only the test run, which takes a few seconds.
- The `ordinary-machine-resource-acceptance` CI job installed the Rust
  toolchain with `dtolnay/rust-toolchain` and no `components`, so `Clippy`
  failed in ~0.1s for a missing component and `Rustfmt` was skipped for a
  missing `cargo-fmt` — an environmental false signal that downgraded the
  acceptance verdict to `CONDITIONAL` on every run. The step now requests
  `components: clippy, rustfmt`, and `tools/bounded_runtime_acceptance.py`
  requires both checks to have their own live, non-cached `passed` row in
  `RUN.json`, matching the other six required checks.

## [0.8.0] - 2026-09-13

### Added

- The macOS release binary is now signed with a Developer ID Application
  certificate (Team ID `MW223P3NPX`) and notarized by Apple. The release
  workflow verifies the signature strictly, asserts the `TeamIdentifier`,
  requires notarization to reach `Accepted`, requires Gatekeeper to report
  `source=Notarized Developer ID` via
  `spctl -a -t open --context context:primary-signature`, and proves the
  archived binary is the notarized one by comparing code directory hashes after
  extraction. A
  credentials preflight job fails the whole run when a signing or notarization
  secret is missing, so no release can be produced unsigned.
- The release workflow accepts `workflow_dispatch` as a dry run. It executes the
  identical preflight, validate, build, sign, notarize and checksum jobs and
  uploads the archives plus `SHA256SUMS` as workflow artifacts; GitHub Release
  creation and the crates.io publish remain gated on a pushed `v*` tag.
- Release archives and `SHA256SUMS` carry a signed GitHub build provenance
  attestation, verifiable with
  `gh attestation verify <file> --repo vetcoders/prview-rs`.

### Fixed

- Release binaries no longer link Homebrew or system OpenSSL. `git2` is now
  built without its default `https`/`ssh` features — which drops `openssl-sys`
  and `libssh2-sys` — and with `vendored-libgit2`, so libgit2 is bundled rather
  than picked up from the build host. prview only reads local repositories
  through libgit2; every network operation already went through the `git` CLI.
  The previous macOS binaries linked `/opt/homebrew/opt/openssl@3/lib/libssl.3.dylib`
  and aborted on launch on any Mac without that exact Homebrew install: under
  the hardened runtime dyld refuses a non-platform dylib with a different Team
  ID, so even `prview --version` died with SIGABRT despite a valid signature,
  notarization and Gatekeeper acceptance.
- The release workflow now executes the *signed* macOS binary — not just the
  pre-signing one — and fails unless it prints the expected version and source
  commit, and unless `otool -L` shows only libraries under `/usr/lib` or
  `/System`. The Linux build asserts the same shape with `ldd`, rejecting any
  `libssl`, `libcrypto`, `libssh2`, `libgit2` or `libcurl` linkage.
- Official release binaries no longer report `unknown` from
  `prview --build-source-sha`. The workflow builds with `PRVIEW_SOURCE_SHA` set
  to the released commit and fails the job unless the built binary reports
  exactly that commit and the Cargo.toml version.

### Changed

- `SHA256SUMS` is regenerated deterministically from the downloaded archives in
  a byte-sorted order, verified with `sha256sum -c`, and every archive is
  required to have an entry. The per-build `prview-*.tar.gz.sha256` files are no
  longer uploaded; the manifest format is unchanged. The release also pins the
  published target set to the two documented platforms, so a release that is
  missing a target archive — or carries an undocumented one — fails instead of
  publishing a partial set.

- `report.json` schema 3.0 makes `quality.breaking_changes.md_path` nullable.
  Missing Markdown reports no longer advertise a dead link; existing Rust API
  reports remain linked even when they contain no breaking findings.
  The same schema uses the canonical PASS/CONDITIONAL/BLOCK vocabulary for
  `gate.status`, matching `gate.verdict` instead of projecting merge permission
  as ALLOW/BLOCK. MERGE_GATE.md explains non-blocking quality failures beside
  the policy and merge-permission axes.
- `MERGE_GATE.json` schema 3.0 records actual policy provenance captured at
  load: `origin: file` with a source path, or `origin: builtin-default` with
  `source: null`. CLI/MCP readers and the validator accept 3.0 while retaining
  older schema contracts and the typed enforcement requirements from 2.3.
- `PROVENANCE.json` schema 2.0 also cross-checks its own two statements about
  the substrate. A new `consistency` object reports how many run/check
  comparisons were made and every `PROVENANCE_CONTRADICTION` found between them:
  an operator tree frozen clean but read `local-dirty` by a check (or the
  reverse), a check that scanned a commit other than the reviewed target, and a
  check that ran in a checkout that is not this repository. Cache replays and
  rows without provenance are not compared, so a replayed or missing observation
  is never reported as a contradiction. `MERGE_GATE.json` schema 3.0 publishes
  the identical rows as an additive `provenance_contradictions` array and one
  `PROVENANCE_CONTRADICTION` review signal each, MERGE_GATE.md explains the
  class, and `CONSISTENCY_CHECK.json` reports `consistent: false` while one
  stands. `report.json` publishes the same fact rather than a narrower one: its
  `quality.consistency` gains `provenance_contradictions` (the identical rows)
  and `provenance_comparisons` (the identical count), and folds them into its own
  `consistent` flag through the same `merge_provenance` reduction the summary
  checker uses — so `report.json` can no longer read `consistent: true` for a run
  `CONSISTENCY_CHECK.json` calls inconsistent. A contradiction is a
  provenance/confidence problem, not a verified failure: no quality failure,
  blocking issue or verdict axis is derived from it, and a run without
  contradictions keeps a byte-identical decision.
- `PROVENANCE.json` schema 2.0 separates review identity (`target_sha`) from
  operator state (`worktree_head_sha`, `operator_worktree`). The ambiguous 1.0
  field names are removed from new records. Operator HEAD is now captured
  before checks, so a later commit cannot change the recorded starting state.
  A HEAD change detected during status fingerprinting invalidates all operator
  fields instead of combining different checkouts; this is not a worktree lock.
  The architecture documentation includes the 1.0-to-2.0 reader migration.
- The default human handoff is a single `dashboard.html` with an offline reader
  for Markdown, JSON, logs, patches, and target-commit source. `--no-dashboard`
  selects the static `review.html` instead. Large text has bounded embedding
  and paging; the pack retains original downloads.
- Dashboard labels distinguish file/test matching from execution coverage,
  structural scores from test outcomes, and repository-wide Loctree candidates
  from changes introduced by a PR. Commit subjects remain complete, check
  duration includes execution status, and declared owners come from the
  target revision's CODEOWNERS. English and Polish descriptions are aligned.
- The dashboard lint section projects the canonical findings model instead of
  re-parsing check output. Counts are reported as `in changed files`,
  `outside changed files`, and `origin unknown` — the canonical `in_diff`
  tri-state — rather than `new` and `legacy (pre-existing)`, and a lint check
  that was skipped or errored is reported as not executed instead of clean.
- Summary labels state the evidence behind them: `Breaking: 0` is now
  `Public API structural changes: 0` with a note that semantic compatibility
  was not assessed, `Sanity OK` is `Artifact pack integrity: OK` (in the
  dashboard and on stdout), `Heuristics OK` is `Loctree structural signals: 0`,
  and `Checks OK (x/y)` is `Checks passed: x/y`. The PR comment export uses the
  same wording.
- Security cards report "not executed" for a scanner that was skipped or
  errored instead of scraping a metric out of its skip reason.
- `30_context/INLINE_FINDINGS.sarif` emits `properties.in_diff: null` and
  `properties.classification: "unclassified"` for a finding whose origin was not
  established. `in_diff` was previously always a boolean, so a consumer that
  assumes that type — or that matches `classification` without a default branch
  — needs updating. `docs/contracts/merge_gate.md` documents the tri-state.
  Every result carries both properties, including Cargo audit advisories and
  rows from checks that have no dedicated parser.

- Updated the bundled Loctree library from 0.13.0 to 0.14.4, together with
  its `loctree-ast` and `report-leptos` dependencies. Structural analysis uses
  this compiled library independently of any installed Loctree CLI.
  The integration explicitly permits deliberate non-git revision archives,
  as required by the new library, using the same scan options in tests.
- CI and the prview gate run on every pull request, not only those targeting
  `main`.
- The repo's git hooks are fast guards only: the pre-commit hook runs
  `rustfmt --check` on staged Rust files instead of `cargo check`, and the
  `prview gate` pre-push hook is gone. Quality proof lives in required CI, where
  clippy now covers `--all-targets`, and in an explicitly invoked `make check`.

### Fixed

- A real review on Windows no longer dies with `STATUS_STACK_OVERFLOW`: the
  composed root future of the review pipeline did not fit the 1 MiB default
  Windows main-thread stack, and Tokio cannot size the thread that runs
  `block_on`. The entrypoint now builds its own runtime on a dedicated
  64 MiB-stack thread on every platform.

- Reviews of a commit other than the operator checkout fail publication when no
  shared snapshot was materialised, instead of silently skipping the snapshot
  integrity validation and describing the target with local files. An operator
  checkout that could not be captured at all — an unborn `HEAD`, or one that moved
  while provenance was being read, which the capture deliberately discards — now
  fails the same way instead of skipping the guard: `--quick` and `--watch`
  publish with an empty ledger, so an unknown checkout identity was the one way a
  snapshot-free pack could still claim a target it never read.

- Pinned targets use exact commit lookup even when a branch has the SHA as its
  name; Semgrep cannot fall back to the operator checkout for an unavailable pin.
  Snapshot boundary comparisons run off the async dispatcher and retain worker
  failures as unknown evidence. Windows Semgrep invocation honors discovered
  batch executables, with native CI coverage of the owned contract fixture.

- Snapshot caveats retain observed HEAD changes after restoration, including
  empty commits with no tracked-path changes. Security option help names the
  Semgrep opt-out and no longer promises unconditional cargo-audit execution.

- Check configurations pin the target resolved for the diff in headless, update
  and TUI runs. Moved or deleted branch/PR refs cannot redirect shared checks to
  the operator checkout; unavailable pinned commits fail planning explicitly.

- Check configurations pin the review BASE alongside the target, so the whole
  range is resolved once at diff capture. Semgrep's diff-scoped
  `--baseline-commit` now reads that captured merge-base SHA instead of
  re-resolving the symbolic base ref: a base branch that advanced past the target
  mid-run (the reviewed branch merged into `main` while the run was in flight)
  used to collapse the re-derived merge-base onto the target, so the scanner saw
  an empty delta while the pack diff was non-empty and the report and the scanner
  described different ranges. A pinned run carrying no captured base refuses to
  plan, exactly like an unavailable pinned commit, rather than falling back to
  symbolic resolution.

- Final snapshot observations use the same immutable creation SHA as check
  boundaries. A snapshot/diff target mismatch aborts publication before output
  allocation, preventing a pack from combining two reviewed commits.

- An explicit Semgrep security opt-out is a declared mode skip, requiring review
  when policy requires the scanner instead of being classified as unknown.

- Shared review snapshots now preserve tracked/index changes against the original
  target as SNAPSHOT_INTEGRITY evidence and require review without rewriting
  passing or failed Cargo results. Committed changes and unknown observations
  cannot certify clean; newly untracked lockfiles do not trigger this rule.
  Non-clean check boundaries remain visible after later restoration and prevent
  overlapping results from entering the target cache.

- `tools/validate_merge_gate.py` no longer certifies a schema-3.0 gate that
  omits `provenance_contradictions`, that carries contradiction rows with no
  `decision.review_caveats` array, that attributes a row to a `check_id` absent
  from its own `checks[]`, or whose `PROVENANCE_CONTRADICTION` signals merely
  COUNT the rows. Signals are now matched to rows one-to-one as a multiset on
  the exact `<code>: <explanation>` spelling, so a duplicated, missing or
  unsupported signal is an error. `tools/tests/test_validate_merge_gate.py`
  pins all five cases against real gates in `tools/fixtures/merge-gate/`, and
  CI runs every `tools/tests/test_*.py` rather than one named file.

- Provenance contradictions are published as review signals by every artifact,
  not only `MERGE_GATE.json`. The signal strings now come from one renderer
  (`ProvenanceConsistency::review_caveats`) that the merge gate and the
  dashboard context share, and the dashboard context is built with the run's
  substrate cross-check — so `report.json`'s `gate.review_caveats`, the
  dashboard, and the dashboard's "Copy PR comment" carry the identical
  `PROVENANCE_CONTRADICTION` entry instead of staying silent about a
  disagreement the gate names. `quality.consistency` already folded the
  contradictions in and is unchanged.

- Pre-existing failure classification uses the operator HEAD captured before
  checks. A later checkout can invalidate stability but cannot grant a new
  downgrade; unknown captured HEAD no longer takes a permissive local fallback.

- MCP process-ownership tests wait for a *complete* published pid instead of the
  bare existence of the pid file. A shell's `>` redirection creates the file
  before `printf`/`echo` writes the digits, so under parallel test load the
  fixtures read an empty file and failed containment assertions that production
  code had satisfied.

- `locales/en.json` and `locales/pl.json` declare `summary.lintTotals` once.
  The obsolete `{new}/{legacy}` template was a duplicate key that JSON parsers
  silently discarded; a locale test now rejects duplicate keys outright.

- A Rust source file counted as covered through an inline `#[cfg(test)]` module
  shows the marker as text. It was rendered as a source link, and clicking it
  opened a "source unavailable" dialog because no blob exists under a synthetic
  marker.

- The dashboard lint section counts no findings for a check it reports as not
  executed. A lint check in `Error` status still produces a canonical row for
  its runner diagnostic, and counting it showed `origin unknown / 1 total` above
  a card stating that no result was produced.

- The dashboard evidence inventory excludes the `prview mcp` launcher control
  files (`RUNNING.json`, `run.log`, `run.stderr.log`) that the manifest and the
  archive already exclude. They are mutable launcher state, not pack payload,
  and the reader no longer presents them as immutable evidence.

- Pytest locations are recognized in repository paths containing spaces and in
  non-Python files reported by doctest or plugin collectors (`.rst`, `.txt`,
  `.md`). Both were previously dropped, so `INLINE_FINDINGS.sarif` and
  `report.json` lost a source location Pytest had supplied.

- `report.json`'s `quality.sarif.findings_count`, the dashboard run history and
  the previous-run delta all count the same operator-finding list, so
  informational notes (the Cargo audit baseline row, the Loctree repository
  summary) cannot report a worsening trend for a run whose diagnostics did not
  change.

- `MERGE_GATE.json` `files.dashboard` names the HTML entry point the run
  actually generated: `review.html` under `--no-dashboard`, `dashboard.html`
  otherwise. It previously always named `dashboard.html`, so every
  static-report run pointed the canonical merge decision at a file the pack
  did not contain.

- MCP contract tests bound response waits and pagination, and clean the owned
  server tree before reaping it on timeout or drop, including detached reviews.

- MCP child registration briefly resamples an `ESRCH` identity lookup when
  the owned child has not yet become waitable, preserving fail-closed behavior
  for ambiguous ownership and exposing wait-status errors in diagnostics.

- Private Rust API and Loctree workers require application-only arguments paired
  with their environment requests and refuse nested launches. A library test
  runner can no longer re-enter the full test suite through `current_exe()`.
  MCP probes also reject test runners rather than treating `mcp` as a test filter.

- Pytest failure excerpts preserve diagnostic locations without treating startup
  text or passed test names as an error, or a traceback location as proof of
  causation. Process noncompletion without a diagnostic records an unknown
  cause consistently across the dashboard, failure summary and report JSON. General
  structural notes no longer inflate SARIF, merge-gate, or report counters.
- CODEOWNERS patterns respect root anchoring and directory depth, and cached
  check labels render as translated text rather than escaped HTML.
- Evidence dialogs use an opaque centered surface, long logs stay within their
  cards, check duration labels remain readable, and failed quality badges keep
  the same color regardless of merge policy. The shared reader searches each
  occurrence and downloads preserved complete originals through Blob URLs;
  unavailable originals are explicitly separate file links. Offline navigation
  no longer rewrites local file URLs or falls back to a second diff reader.

- A signal arriving after durable pack publication no longer relabels the
  completed run as exit 130, while an unchanged `--update` remains
  cancellation-sensitive. Crash-journal recovery refuses and quarantines
  symlinks, non-regular inputs, and records larger than 64 KiB.
- Cancellation remains typed as exit 130 even when rollback of the temporary
  `latest` publication also fails; the rollback error is retained as context.
- Vitest filtering now uses its supported `--testNamePattern` option. Snapshot
  Drop no longer starts a potentially blocking `git worktree remove` child on a
  Tokio worker, and exact rollback skips unreadable stale sibling registrations.
- The CLI now rejects `--watch` together with `--tui` instead of silently
  entering TUI mode and ignoring the requested watcher.
- Context artifact planning is reconciled with the command outcome and actual
  output file, so a timeout or unavailable command cannot remain recorded as
  generated. Invalid SANITY results now abort before ZIP and publication while
  retaining the incomplete directory for diagnosis.
- Fast remote-only runs no longer enter repo-backed Rust API snapshot analysis;
  they emit exact-revision typed unknowns that degrade the gate and require
  review. Heavier modes isolate all base/target comparisons in one governed
  same-binary worker with one 30-second total deadline. Worker timeout, failure,
  or malformed output remains typed uncertainty. Headless Ctrl-C remains exit
  130; the TUI keeps its documented cooperative first-interrupt behavior.
- Cargo resource planning now opens only bounded regular configuration files
  and refuses final symlinks, devices, FIFOs, and oversized inputs before any
  synchronous read. A malicious off-HEAD `.cargo/config*` can therefore lower
  the plan fail-closed, but cannot hang or exhaust the review before a governed
  child exists. Context provenance now also marks `tauri info`, esbuild
  metadata, and npm SBOM collection as `snapshot-borrowed-deps` when those
  commands consume prview's linked local `node_modules`.
- Python resource planning now applies the same finite-read boundary to the
  selected project-scoped uv authority. A FIFO, device, non-regular file, or
  `uv.toml`/`pyproject.toml` above 1 MiB fails closed before a governed uv child
  exists; contained metadata symlinks still resolve to their in-tree regular
  target.
- Unix cancellation and timeout cleanup now freezes a hardened tool group,
  discovers the fixed point of its live PPID descendants, binds each detached
  group leader to its native birth identity, and kills leaf groups before the
  original PGID. A child using `setsid` or `setpgid` can no longer escape the
  resource governor merely by leaving its parent's group. Already-reparented
  double-fork daemons remain outside the portable Unix guarantee and are
  documented as such rather than covered by a false whole-tree claim.
- Rust API analysis now treats `dylib` as both Rust-linkable and
  native-producing, so private macro boundaries that may synthesize exported
  symbols cannot disappear from an otherwise ordinary Rust dependency
  surface. Custom-cfg authority also follows repository-backed Cargo config
  from each package directory through its ancestors, with Cargo's merge and
  same-directory filename precedence, instead of recognizing only the
  repository-root config.
- MCP lifecycle readers no longer classify a possibly-active v2 review as
  stale merely because its process-birth identity is missing or the native
  identity probe is indeterminate. A live PID now retains the one-active-run
  invariant unless a successfully read identity proves PID reuse; confirmed
  dead or recycled publishers still become stale.
- Off-HEAD reviews now abort when their required shared target snapshot cannot
  be materialized. Checks, Python pre-sync, and later context artifacts can no
  longer continue against independent or local trees and combine multiple
  revisions in one apparently coherent pack.
- Python runners now resolve `CARGO_BUILD_JOBS` through the same effective
  Cargo configuration path as direct Cargo gates. A Rust-backed `uv sync`,
  `uv run`, or direct Python plugin can no longer raise a stricter reviewed
  repository `[build].jobs` ceiling.
- `MERGE_GATE.json.stale_cache_caveats` now dates old cached passing rows as
  well as failures/blockers. A stale PASS can support a clean verdict after a
  compiler or toolchain change just as a stale failure can hold a merge; the
  caveat remains additive and never changes the decision by itself. A replay
  whose age cannot be established is now surfaced as `age_status: "unknown"`
  with a null age instead of being silently treated as fresh.
- MCP quick reviews no longer cancel when a short-lived hardened helper exits
  between spawn and governor registration. A non-reaping wait-status probe
  distinguishes that completed child from an ambiguous identity failure while
  preserving its PID/PGID until registration. If another member still occupies
  that exited leader's group, registration terminates the group while the
  leader remains unreaped. A rejected group signal is no longer mistaken for a
  surviving tree when a bounded process census proves that only the zombie
  leader remains; a live member or unreadable census still cancels fail-closed.
  A provisional-only PGID is never left for the MCP parent to guess about after
  PID reuse becomes possible.
- MCP `run_review` now rejects source-buildable targets without a native
  PID-reuse-safe process-birth identity before taking the activation lock or
  spawning a review. Linux, macOS, and Windows remain the explicitly supported
  MCP review targets; other targets retain the direct CLI path.
- MCP branch activation no longer maps every lock failure to a retryable
  "another review is running" response. Live contention carries
  `retryable: true`; a stale legacy-compatible `.active.lock` preserves the
  fail-closed mixed-version invariant but exposes `recovery_required`, its exact
  path, and no retry timer. Permission, linked-path, and unsafe lock-file I/O
  failures surface as non-retryable `storage_corrupt`; an unparseable legacy
  token remains unattributable stale evidence and uses explicit recovery. A
  failed legacy claim explicitly releases its v2 kernel lock before returning,
  so documented manual recovery can retry immediately across platforms.
- Python checks honor uv's `UV_NO_CONFIG` boolish contract before inspecting
  repository configuration: enabled values skip discovered `uv.toml` and
  `[tool.uv]` limits, false values preserve discovery, and an explicit contained
  `UV_CONFIG_FILE` remains authoritative. Invalid and non-UTF-8 values fail loud
  instead of manufacturing a resource ceiling. When uv is unavailable and
  prview invokes Ruff, Mypy, or Pytest directly, uv-only files and environment
  selectors no longer participate in planning or break an otherwise runnable
  direct gate.
- Direct Cargo gates preserve the effective repository `[build].jobs` ceiling
  visible from the exact reviewed cwd, including remote snapshots, instead of
  overriding a stricter project limit with prview's resource-plan width.
  Signed inherited job limits retain Cargo's logical-core-relative semantics;
  unknown, invalid, or zero Cargo config influence fails closed to one worker.
- `--tests-pattern` no longer allows a Cargo regex to match zero tests or an
  option-shaped value such as `--no-run` to produce a false green. Cargo accepts
  literal substrings only and filtered runs require positive libtest execution
  evidence; Vitest retains regex semantics, Mixed JS/Rust reviews require their
  shared literal subset, and Pytest is explicitly unfiltered.
- Repo-backed Rust API analysis now discovers Cargo's real binary targets from
  `src/main.rs`, `src/bin/*.rs`, `src/bin/*/main.rs`, and `[[bin]]` entries,
  respecting the per-target-category edition-2015 `autobins` default, explicit
  paths, and `required-features`. Implicit libraries remain independent of
  explicit binary/example/test/bench targets unless `autolib = false`.
  Cargo-valid special library identities (`self`, `crate`, `super`, and `Self`)
  remain analyzable as manifest identities even though they cannot be written
  as raw Rust identifiers. Binary targets keep a target-scoped analysis identity,
  preserving digit-prefixed and hyphen/underscore-distinct manifest target names, so
  `foo-bar` and `foo_bar` cannot collide and native-export evidence cannot
  inherit a same-named library's Rust dependency projection. Direct native
  function evidence excludes implementation bodies, matching associated
  exports, while static initializers remain observable.
- While the TUI waits for an in-process synchronous analysis stage to unwind,
  it continues reading raw terminal input. A second Ctrl-C now returns typed
  cancellation immediately, allowing terminal cleanup and the established exit
  130 path instead of trapping the operator in the cancel join. Headless and
  TUI cancellation both split the synchronous cancel transition from blocking
  process-tree termination, so the second interrupt stays live even while a
  Windows `taskkill` is still running. A ready interrupt is drained before a
  completed work future can publish a verdict.
- The opt-in balanced resource plan no longer creates two parent permits on a
  one-core host; parent permits now stay within the detected logical-core count,
  preserving the advertised single-heavy-tool envelope on constrained runners.
- The `gh --version` startup probe preserves typed cancellation at its own
  error boundary instead of relying on the outer startup supervisor to recover
  it. Direct or internal config construction under a run scope no longer
  mislabels cancellation as a missing GitHub CLI.
- MCP quick reviews route Tokio child-wait errors through the same bounded
  containment and direct-root reap used for timeouts. On Unix the adapter first
  requests cooperative Ctrl-C, then uses a parent-owned, incarnation-bound
  sidecar ledger to reach every separately-grouped tool if the review root
  cannot unwind. A pre-exec provisional row, stopped-root descendant census,
  and inherited ledger-lock barrier close the spawn-before-registration gap;
  a provisional PID is never trusted as signal authority by itself. The ledger
  FD stays CLOEXEC in the MCP parent and is handed off only inside the forked
  review root. Killing the root process group alone cannot contain nested Cargo
  or Semgrep groups. The mode-0600 sidecar lives outside the immutable pack;
  confirmed cleanup removes it, while an unconfirmed cleanup retains it and
  returns `containment_confirmed: false`. Running markers remain non-blocking
  diagnostic stale state instead of substituting stale-marker expiry for
  explicit cleanup.
- MCP active-run discovery filters markerless history and the completed-run
  `latest` alias before lifecycle probing. Runs without `SANITY.json` no longer
  reload the global publication index while they are polled, while completed
  publication still wins over a lingering live marker.
- MCP deep reviews retain and reap every detached direct child, including an
  immediately failing process, and inherit the parent-owned child-group ledger
  used by quick reviews. After the root exits, the reaper drains separately
  hardened Cargo, Semgrep, and other tool groups before it may remove the
  running marker; unconfirmed containment remains explicit diagnostic state.
  A zombie or surviving nested group therefore cannot be mistaken for a
  completed detached review or keep a later review blocked.
  Versioned running markers bind the PID to its native process-birth identity
  across Linux, macOS, and Windows; recycled v2 PIDs become stale, while a live
  legacy PID blocks conservatively until it exits. Marker or reaper setup
  failures terminate and reap the child tree before the RPC returns
  `run_failed`. The reaper receives the caller's exact publication-index path,
  so its background completion check cannot drift into another storage home.
- Off-HEAD Python pre-sync now runs on the shared reviewed snapshot with the
  same per-commit `UV_PROJECT_ENVIRONMENT` as Ruff, Mypy, and Pytest; it no
  longer mutates the operator checkout's environment or warms a venv the gates
  do not use. `uv.toml` is contained like `pyproject.toml` and `uv.lock`, while
  ambient uv config/project/working-directory redirects fail closed when they
  would leave the exact reviewed root. Repository `uv.toml` or `[tool.uv]`
  concurrency limits are ceilings alongside inherited environment values and
  the resource plan. Every later `uv run` inherits the same resolved
  `UV_CONCURRENT_*` and `CARGO_BUILD_JOBS` caps, so a failed pre-sync cannot
  retry an unbounded build or override a stricter project limit.
  Pytest is pinned to the single config selected inside the reviewed root (or
  an explicit empty config), preventing ambient parent options from changing
  the run. An isolated probe of the actual project pytest selects the supported
  major/minor discovery contract; unknown versions, malformed or unreadable
  configs, unparseable addopts, a standalone option terminator, and xdist custom
  gateway options (`--tx`/`--px`) fail closed. Shell-quoted inherited/config
  addopts follow pytest token boundaries, while explicit lower
  or disabled xdist counts stay unchanged and larger/auto pools are capped.
- Run publication fails closed when the durable index cannot be read or when a
  completed pack cannot be committed to discoverable history. Transactional
  readers reject corrupt JSONL instead of saving a fabricated partial ledger,
  and latest-journal recovery preserves a valid journal until the index is
  readable again. MCP Quick and Deep require both finalized pack bytes and the
  exact durable run-id/path index row; SANITY alone is never `completed`.
- Ordinary-machine bounded-runtime receipts bind their release binary to the
  exact clean source commit through an embedded build SHA and record the
  binary's SHA-256 digest; an old binary beside a new checkout cannot produce
  exact-SHA evidence.
- Private Rust API dependency resolution follows `extern crate self as alias`
  back to the crate root, so a private layout or auto-trait change behind
  `alias::Type` cannot disappear as stable unresolved evidence.
- Rust transforming-attribute uncertainty, including recursively nested
  `cfg_attr`, is collected before visibility filtering and binds its annotated
  input to lock-backed external dependency
  identity or, for reachable local proc-macros, the complete live tracked-entry
  substrate. Nonstandard crate roots, private annotated items, source
  replacements, stale/unrelated lockfiles, and unproven symlink targets can no
  longer hide generated public API uncertainty.
- Derive macros are modeled as additive: the annotated item remains a confirmed
  input contract while custom generated output stays typed uncertainty. Custom
  derive/helper tokens cannot manufacture a confirmed break, and an imported
  derive whose name shadows `Debug`, `Clone`, or another builtin remains
  conservative instead of being misclassified as compiler-provided. Builtin
  `Default` variant markers remain confirmed through nested, matching
  `cfg_attr` predicates; unresolved conditional relationships stay typed
  uncertainty, while a custom derive helper named `default` remains transform
  input only. Associated transformers are materialized only when their
  inherent or trait owner is externally reachable; public type-alias chains
  remain typed owner uncertainty rather than invented confirmed methods.
- External transformer proofs require an effective lock entry with an external
  source, registry checksum or precise Git commit, and a version satisfying the
  declared requirement; a stale lock containing only a same-name workspace
  package cannot qualify. Reachable path manifests and effective
  `.cargo/config` bytes from every reachable member invocation context
  participate in the proof, and working-tree regular-to-symlink type changes
  fail closed.
- Public `async fn` and return-position `impl Trait` bodies retain item-local
  opaque-return auto-trait uncertainty through public reexports and inherent
  projections, bound to canonicalized repo-backed Rust plus all other live
  tracked input identities and effective lock data so private helpers,
  nonstandard includes, and build assets cannot disappear; tracked symlinks stay
  unresolved. New/removed opaque items and newly added async trait defaults do
  not gain redundant Unknown findings, including through public trait aliases.
  Ordinary bodies remain outside confirmed contracts; adding a public
  trait-method default is compatible, while removing one is a confirmed
  contract change. Parameter, local irrefutable destructuring, closure, loop,
  and lexical-shadow binder spellings are alpha-normalized in opaque bodies;
  refutable pattern names stay conservative without name-resolution proof.
- Rust 2024 `unsafe(no_mangle)`, `unsafe(export_name)`, and
  `unsafe(link_section)` remain ordinary confirmed attributes on public items.
  A private `no_mangle`/`export_name` function or static emits typed
  binary-export uncertainty instead of disappearing from the API analysis,
  including through nested `cfg_attr`. In native-only targets, associated binary
  exports with a transforming attribute bind their complete macro-visible member
  input, a separate normalized owner/ABI contract, and the revision-backed
  transformer implementation. Native-producing
  `dylib`/`cdylib`/`staticlib`/`bin` targets, including mixed `rlib + cdylib`
  targets, also retain typed potential export evidence when a custom associated
  attribute can synthesize the export itself. The speculative associated-macro
  fallback is limited to those native artifacts, so an internal macro on a
  private owner in an `rlib`-only crate does not manufacture API uncertainty.
  Conditional `macro_export`
  declarations remain root API, while item-position macro invocations in both
  Rust-linkable and native-only targets bind their input to a revision-backed
  implementation substrate; native-only `include!` also retains its included
  source proof instead of disappearing behind the target-level uncertainty.
- Rust named private-field order is preserved only when `repr(C)` defines it.
  Pure reorders under `repr(transparent)` or standalone `repr(packed)` /
  `repr(align)` are neutral, while private field types and semantic repr
  attributes remain observable; `repr(C)` and primitive enum representations
  stay order-sensitive.
- TUI startup keeps its signal supervisor through the raw-mode transition and
  gives an already-pending interrupt priority during the explicit handoff to
  key events. `--tui` now rejects one fixed immutable
  `--output-dir`, just like `--watch`, because every rerun needs a fresh pack.
- Unix-only Git/tar test overrides are compiled only on Unix, allowing the
  Windows process-tree cancellation job to compile under `-D warnings`.
- Publication rollback tests run the synchronous restore on a blocking worker
  under a real bounded completion deadline instead of a scheduler-sensitive
  sub-100ms assertion.
- Fast remote-only preflight no longer lists TSC or ESLint as expensive work
  when that preset will skip those gates.
- MCP Quick and Deep reserve their output path through a private one-shot nonce
  before launching the child, then adopt only that fresh control-only directory.
  Public `--output-dir` remains immutable and cannot reuse an existing pack;
  `--watch` rejects a fixed explicit output path because every iteration needs a
  distinct pack. Mutable MCP liveness/log files remain readable beside the pack
  but are excluded from its manifest and ZIP.
- Retention authority rejects every Windows reparse point, including junctions
  and mount points that are not reported as symlinks. Pre-rename validation,
  transaction recovery, every intermediate identity component, and recursive
  cleanup all use the same owned-file/directory predicate and never traverse an
  external target. Identity and manifest files on Unix must also own their inode
  rather than be hard links.
- Windows check and context-command trees are owned by Job Objects, so a
  successful wrapper cannot orphan descendants by exiting before prview reaps
  it. Synchronous Git/pipeline/context cleanup uses the live Job Object first
  and `taskkill` only as a fallback; a dual termination failure returns from
  cancellation instead of entering an unbounded wait. The `windows-latest` job
  proves cancel and root-exits-first paths. Its process-tree fixture now uses
  the native process-liveness probe instead of spawning `tasklist` for every
  poll, accepts only newline-terminated stable PID publications before taking
  descendant ownership, binds each captured PID to its native process-birth
  identity before probing or cleanup, and reports captured PowerShell output
  on a bounded 30-second readiness failure. This prevents stale ownership from
  certifying a recycled runner PID and narrows failure cleanup to the recorded
  process incarnation before invoking the PID-based tree-kill fallback.
- Primitive integer enum representations (`repr(u8)` through `repr(isize)`) are
  ABI-sensitive in Rust API deltas, including non-exhaustive variant additions.
- Unparseable and non-UTF-8 rootless Cargo manifests fail crate discovery closed
  as `WorkspaceDiscovery` uncertainty instead of letting a parseable fixture
  become the product authority.
- Parseable Cargo manifests that define neither `[package]` nor `[workspace]`
  are invalid authorities rather than invisible files. A root manifest emits
  typed manifest uncertainty, a rootless candidate prevents sibling selection,
  and a manifest cannot combine `[workspace]` with `package.workspace`.
- Inherited, `pub(crate)`, and `pub(super)` fields share one external-private
  Rust API visibility while their types and tuple positions remain observable.
- `uv sync` preserves an operator's stricter inherited `UV_CONCURRENT_*` caps,
  direct Cargo gates preserve stricter inherited and repository-configured
  `CARGO_BUILD_JOBS`, and
  Cargo test binaries preserve a stricter inherited `RUST_TEST_THREADS`,
  instead of raising any of them to the prview worker limit.
- Snapshot extraction preserves the `git archive` stdout pipe after applying
  hardened subprocess defaults, so `tar` receives the archive instead of an
  empty stdin and the producer no longer exits through SIGPIPE.
- Rust API crate discovery anchors workspace authority at the repository-root
  `Cargo.toml`; a nested fixture workspace can no longer displace a root product
  package from the census.
- Nested `tsconfig*.json` files under fixtures, dependency/build-cache trees, or
  vendor directories no longer turn a Rust repository into a Mixed profile;
  real monorepo configs remain product signals even when a legitimate package
  is named `build` or `dist`.
- `prview state --tui` no longer starts a review from `r`.
- Moving a library crate root (`src/lib.rs` → `lib.rs`) is not a public API
  change; the compared crate contract keeps `proc-macro` and `crate-type` only.
  A package without an explicit `[lib]` and without a live implicit
  `src/lib.rs` is now correctly treated as having no library target instead of
  emitting `MissingLibRoot`; an explicit missing library root remains typed
  uncertainty. A tracked symlink in the implicit root position is never
  silently treated as an absent crate or followed outside revision provenance;
  both compared sides retain non-neutralizable typed uncertainty instead.
- Cargo-valid keyword package and library names that Rust can address as raw
  identifiers remain in the API census, and `package.build = true` resolves to
  Cargo's default `build.rs` rather than being rejected as an invalid manifest
  value.
- Cache hits take content and mtime from one open file handle, so a concurrent
  replacement cannot pair one entry's bytes with another's age.
- A finished context-command child is unregistered before its output is read.
- A check-dispatcher board row is finished by the queued check name, and an
  exhausted future stream with leftover rows is an error instead of a hang.
- Timeout waits keep durable tree ownership until after process-tree
  termination, including after a Windows root PID has already exited.
- Optional Cargo dependencies without an explicit `[features]` table are
  implicit features in the repo-backed API contract, so removing or renaming
  them is a compared delta.
- Library `[lib] crate-type` is part of the crate contract. Ordinary Rust API
  projection is limited to Rust-linkable `lib`/`rlib`/`dylib` outputs,
  procedural macros declared by either `proc-macro = true` or an effective
  `crate-type = ["proc-macro"]` retain their separate export surface, and native-only
  `cdylib`/`staticlib`/`bin` targets retain binary-export evidence, including
  exported associated functions in inherent and trait impls, plus typed target
  uncertainty without inventing a Rust dependency API.
- Exported declarative macro contracts bind the effective defining-crate
  edition, including package, workspace-inherited, and library-target
  authority. An edition change without an exported macro does not manufacture
  a crate-level break.
- Public custom `cfg` predicates, including nested fields, variants, trait or
  impl members, and foreign items, bind revision-backed build-script and Cargo
  config authority. Only a live declared/implicit build script or effective
  repository-backed config in the package's manifest-directory ancestor chain
  that can actually supply `--cfg` qualifies; sibling and descendant configs
  do not. Equal complete digests may neutralize; missing,
  invalid, included, or otherwise unresolved authority never does. Compiler-set
  `target_abi` remains a built-in predicate rather than custom cfg.
- Trait-default comparison is structural and cfg-qualified. Adding a method or
  associated-const default is compatible, while removing a default, changing a
  const value/type/cfg, or swapping defaults between disjoint cfg branches
  remains a parent `Changed` fact. Opaque-return proofs use the same member cfg
  identity and cannot cross-cancel between same-named methods.
- Adding a field to a `#[repr(C)]` (or packed/transparent) struct is a parent
  `Changed` ABI break even when the struct is `#[non_exhaustive]`.
- `include!` / `include_str!` / `include_bytes!` unknowns carry a digest of the
  included file in both item and public const/static expression position, so an
  unchanged invocation no longer hides a breaking edit of the included source.
- Transforming-attribute uncertainty is bound to the complete annotated input;
  an unchanged derive/attribute can no longer neutralize a changed public item.
- Adding a variant to an ABI-sensitive `#[repr(...)] #[non_exhaustive]` enum —
  including primitive integer reprs — is a parent `Changed` fact rather than an
  informational addition.
- TUI artifact generation uses `governor::blocking_stage`, matching headless
  `App::run`, so a single-worker Tokio runtime can still poll q/Escape and
  cancel while the synchronous pack is being written.
- A cancelled artifact run no longer publishes `latest` or a run-index row.
  Those advertisements ran after the `PackPublication` seam and before the
  final cancellation check, so Ctrl-C in that window could delete the pack's
  success surfaces while still pointing `latest` and the index at the
  incomplete directory. If cancel is observed after `latest` is written, the
  previous completed alias is restored before the incomplete marker is left.
  `register_and_prune` atomically stages retention candidates in private
  prune-trash before the index commit: a cancel mid-transaction restores every
  directory and the prior index. Physical deletion is deferred to the next
  registration, before that run mutates its own index, and is cooperatively
  cancellable.
  If a custom output filesystem cannot be staged atomically, retention is
  skipped with a warning while the new row and all predecessor rows are kept.
  `latest` and `index.jsonl` are one globally serialized, restart-recoverable
  publication: a durable journal reconciles crashes at either boundary.
  During migration, the **index critical section** remains mutually exclusive
  with pre-0.8 create-new sentinels. This cannot serialize the whole legacy
  publication: a pre-0.8 binary retargets `latest` before it attempts that
  sentinel. Operators must therefore drain and exclude every pre-0.8 publisher
  before the 0.8 cutover; only 0.8-to-0.8 publication is end-to-end
  transactional. A stale legacy sentinel fails closed and requires explicit
  operator removal after old publishers are ruled out. Lock and journal paths
  refuse links that could redirect truncation, and journal writes use owned
  unique temp files.
- Repo-backed crate discovery follows workspace `members`/`exclude` (or the
  single root package). Fixture and tool `Cargo.toml` files are not product API.
- Multiple independent workspaces without a root `Cargo.toml` now fail closed
  as side-specific `WorkspaceDiscovery` uncertainty instead of being silently
  unioned into one public surface.
- `impl std::fmt::Display` (and other external/prelude traits) on a public type
  is a `TraitImplResolution` unknown, not a silent no-delta.
- Unqualified imported trait impls on public types are retained as
  `TraitImplResolution` unknowns without relying on a hard-coded trait-name
  allowlist.
- Private-field type changes on a public struct (for example `u8` → `Rc<()>`,
  which drops `Send`/`Sync`) change the parent contract, while a pure private
  field reorder under the default/`repr(Rust)` layout does not. An informational
  field addition to a non-exhaustive struct cannot mask a simultaneous repr,
  generic, attribute, or private-field change; public fields are identified by
  visibility rather than collision-prone synthetic names.
- Public signatures that depend on transitive non-public local types or their
  local trait impls now emit guard-aware `PrivateTypeDependency` uncertainty
  instead of overclaiming a compiler-derived breaking fact. Private imports,
  module aliases, and unreachable-module reexports participate in that closure.
- A root package that declares `package.workspace` is resolved through that
  workspace's complete member authority instead of being treated as an
  isolated package; missing, invalid, or incomplete workspace membership fails
  closed as `WorkspaceDiscovery` uncertainty.
- TUI/headless `git fetch` and `git archive | tar` register with the run
  governor, so q/Escape/Ctrl-C can stop the sync phase. Headless provenance,
  target/base snapshot creation, watch-mode Git state probes, and TUI preflight
  also run only after an interrupt supervisor is active; TUI preflight happens
  before raw terminal mode.
- Raw-mode TUI Control-C is handled before wizard/panel routing, and async
  command deadlines now include bounded stdout/stderr drain with reader-task
  cleanup after the child is reaped.
- Loctree cache creation runs in a private governed worker, so cancelling TUI or
  headless analysis terminates the synchronous scan instead of merely dropping
  the awaiting `spawn_blocking` handle.
- Cold `uv sync` takes the run's Exclusive budget and inherits the configured
  download/build/install concurrency cap.
- Context commands share one stage timeout instead of minting a fresh deadline
  per Exclusive spawn.
- A context command that reaches that deadline while still queued remains
  `skipped` in the ledger instead of being claimed as a zero-duration run.
- Same-run context dedup is `reused`, not `cached` with a null age. A live
  gate that already produced the signal is reuse; only a stored replay stays
  `cached` with the original entry's age. `RUN.json` `ledger.schema` is `2`.
- Persistent replay is disabled for TypeScript, ESLint, Stylelint, Ruff and
  Mypy until their keys can bind the complete effective config, ignore,
  plugin, dependency and toolchain inputs. Same-run context reuse remains.
- `RUN.json` ledger rows emit `queue_wait_secs` when a check waited on the
  resource budget before admission, so a slow tool is not confused with queue
  pressure.
- Public trait-impl evidence alpha-normalizes impl-level generic binders, so
  renaming `impl<T> Trait for Wrapper<T>` to `impl<U> Trait for Wrapper<U>`
  is not a review-required unknown delta.
- Public trait associated const/type members reuse the same alpha-normalized
  binder scopes. Rust API additions remain informational only when they cannot
  add an auto-trait input or shift an existing implicit discriminant: an
  appended fieldless variant on an otherwise unchanged `#[non_exhaustive]` enum
  qualifies; inserted or payload variants and fields added to existing structs
  or variants retain a parent `Changed` finding.
- A check admitted by the resource governor but unable to launch its target
  command is recorded as ledger `skipped`, not as live `run` coverage. Checks
  that actually execute before returning a runtime skip remain `run`.
- `include!`, `include_str!` and `include_bytes!` dependencies in every public
  contract position carry their source digest, including declarations exposed
  through private-module reexports, without degrading unreachable declarations
  or scanning ordinary function bodies. Proofs bind the resolved public alias,
  never cross disjoint cfg declarations, and terminal `include_str!` /
  `include_bytes!` proofs remain stable when an unchanged private donor file
  moves behind that alias. Plain `include!` stays review-required until its
  path-sensitive and transitive expansion can be proven.
- `repr(C)` union members are canonicalized as an order-independent set while
  retaining names and types; named `repr(Rust)` enum-variant fields are likewise
  order-neutral, while `repr(C)` enum payload order remains ABI-significant.
- Public trait impls declared in private helper modules are retained as
  `TraitImplResolution` unknowns instead of disappearing from the delta.
- Retargeting a public reexport between two still-public types is a compared
  contract change; private donor renames stay semantically equal.
- TUI backend errors after analysis has started cancel and join the analysis
  task instead of detaching Cargo/Node process trees.
- The TUI `r` hotkey no longer starts a run from the branch wizard or the
  help overlay, so `r` can still be typed into the branch filter.
- `cargo test` caps libtest through `RUST_TEST_THREADS` rather than forwarding
  `--test-threads` after `--`, so `harness = false` custom targets are not
  passed libtest-only flags and a stricter operator-provided thread limit is
  never raised.
- Repo-backed Rust API analysis now preserves callable tuple-constructor shape,
  exhaustive versus `#[non_exhaustive]` enum policy, explicit-repr private
  layout, public data-type binder semantics, and valid siblings beside legal
  non-UTF-8 Git paths. Trait impls that need compiler resolution remain typed
  unknowns instead of silently disappearing.
- Raw non-UTF-8 Git path identities cannot collide with a legal UTF-8 file whose
  name literally matches the printable `<git-path-bytes:...>` surrogate, while
  nested artifact-facing surrogates remove the internal NUL sentinel before
  JSON or rendered output.
- Public modules, library-crate declarations, and Cargo feature contracts now
  participate in the shared Rust `ApiDelta`, so their removal is projected
  consistently through human artifacts, JSON, MERGE_GATE, report, CLI, and MCP
  readers without changing schema 2.3 or existing field names.
- Ctrl-C is observed in every phase of a run, including the artifact stage. The
  interrupt was watched from the same `select!` as the run itself, which only
  works while the run keeps yielding — `artifacts::generate` is synchronous and
  polls its children with `std::thread::sleep`, so for the whole of the longest
  stage of a review neither the first nor the second interrupt could fire, and
  since `tokio::signal` had replaced SIGINT's default disposition the terminal
  could not end the process either. The supervisor now watches from its own task.
- A cancelled run never reports a verdict. Only the checks stage watched for
  cancellation, so a Ctrl-C arriving during the heuristics or the artifact stage
  — or during a run whose gates all replayed from the cache, where that stage's
  wait loop is never built — was ignored: the run finished, wrote a pack whose
  context commands were all recorded `cancelled`, and exited `0` or `1` on the
  verdict computed from it. Cancellation is now checked between the stages, so
  the run ends in exit `130` as the contract says. A partial pack may remain on
  disk; nothing claims a verdict from it.
- `--watch` ends on the first Ctrl-C. The iteration reported a cancelled run as
  an ordinary error and carried on watching, but the governor it shares with
  every later iteration was by then permanently closed — so each subsequent edit
  emitted a pack with an empty `30_context` and announced "Regenerated
  artifacts", until a second interrupt killed the process and left the temporary
  worktrees behind. A cancel arriving while the watcher is idle ends it too.
- Ctrl-C during the `uv sync` pre-step stops it. The Python venv build ran
  outside any child scope, so the governor held no pid for it and `cancel()`
  signalled nothing: the run announced that it was stopping its tools and then
  waited out the full five-minute timeout with `uv` still building. It is now
  registered like every other child, and is not started at all for a run that
  has already been cancelled.
- `prview --update` no longer reports a verdict for a cancelled run. Every
  cancellation gate sat after the checks stage, and `--update` returns before
  reaching it: a Ctrl-C during the initial `git fetch` — which the governor holds
  no pid for, so it cannot be cut short — on a HEAD with no new commits printed
  "stopping running tools" and then handed back the *previous* run's pack, which
  the exit code was computed from. The run is now asked on entry and again before
  reusing that pack, so it exits `130` like any other cancelled run.
- The TUI's check dispatcher stops on Ctrl-C the way the headless one does. It
  was a copy that had drifted back into two already-fixed bugs while its comment
  still claimed to mirror them: the `uv sync` pre-step ran outside any child
  scope and with no cancellation gates, and the loop over the running gates had
  no arm for a cancel at all, so a gate with a long timeout could hold the stage
  open after every child had been killed. Both paths now go through the shared
  helper and the shared loop shape.
- The context stage no longer repeats a gate's work on a JS repository. When the
  gates share an analysis snapshot, the ledger entries that predate it are
  adopted onto that snapshot's revision signature — but the signature was
  computed once for the whole run, ignoring which dependencies each tool borrows
  from the working tree. An ESLint entry was therefore filed under `snapshot`
  while the context stage went on to ask for `snapshot-borrowed-deps` for the
  same directory, the lookup missed, and the context stage re-ran a full
  `eslint . -f json` (and, off the fast path, a full `tsc` trace) that the gate
  had already done — on exactly the two scenarios the shared snapshot exists
  for. Each entry is now adopted under the signature its own tool will later
  compute, so the gate's result is found — and a missing tool is no longer
  reported as "no ESLint gate in this run" when the run did have one.

### Added

- Ctrl-C cancels a run instead of killing it. The first interrupt stops the
  governor granting work, SIGKILLs the process group of every tool the run
  spawned (reaching `cargo → rustc → cc` and `sh → pnpm → tool`, not just the
  direct child), and unwinds the run through its ordinary error path so the
  temporary worktrees and analysis snapshots are removed on the way out — an
  aborted process left all of them on disk. A cancelled run exits `130`
  (128 + SIGINT), deliberately outside prview's verdict codes because it
  produced no verdict; a second interrupt exits immediately. Context commands
  that never started are recorded as `cancelled` rather than omitted. `--tui`
  keeps its own quit path. A Loctree worker reaped on the narrow
  cancel-versus-wait seam remains a typed cancellation rather than leaking a
  platform-specific wait error.
- Temporary review worktrees arm a path-exact in-process registration rollback
  before `git worktree add`. Cancellation or timeout after Git has written
  common-dir metadata, and cleanup of an already-created snapshot under a
  cancelled run, remove only that snapshot's administrative entry without
  spawning an ungoverned child or waiting for a future global prune.

### Changed

- The ordinary-machine `--deep` acceptance fixture now runs and requires real
  Cargo, Vitest, Semgrep, TSC, ESLint, and Stylelint processes. A green receipt
  can no longer be produced by the narrower Rust-plus-Vitest subset or by a
  check that merely started and then failed, skipped, or reused cached evidence:
  every required `RUN.json` row must be a live `passed` result. Its census
  separates Semgrep RPC coordinators from scan workers, and exact-SHA receipts
  require a clean source tree at the claimed commit. The harness deliberately
  omits `--resource-budget`, so it proves the bare `--deep` CLI default itself
  resolves to the safe one-parent/one-child plan.
- Rust trait-impl unknown evidence resolves top-level trait/owner aliases
  (including reference, pointer, slice, and array owners) to guarded nominal
  pairs, preserves each trait/owner pair's joint cfg region, and compares
  ordinary associated items independently of source order.
  Declaring scope remains fail-closed for relative names; aliases used only in
  generic arguments remain typed uncertainty until compiler-backed resolution.
- Alias-resolution exhaustion is structural, never paired away as an equal
  partial proof, and cannot be spoofed by matching diagnostic text.
- Retention recovery validates a staged payload's RUN identity before any move
  or deletion. Missing, invalid, or mismatched transaction metadata preserves
  evidence without blocking later publication, while I/O failures after
  mutation still abort. An unconfirmed previous-index rollback keeps the outer
  durable journal for the next lock owner. Relative custom output paths become
  absolute before publication, and a custom path must be newly claimed for one
  immutable pack rather than reused by multiple history rows. Unix/macOS parent-directory fsync defines the
  power-loss ordering; other platforms do not claim that durability tier.
- Quality checks and context commands now run under one machine-wide budget
  (`ResourceGovernor`) instead of each stage picking its own fan-out. Checks
  declare `Light` or `Heavy` via `Check::resource_weight`; unspecified checks
  stay `Exclusive`. Rustfmt is `Light`. The Cargo family, Vitest, and Semgrep
  are `Heavy` and cost half the budget each, so a mixed Rust+JS profile no
  longer starts four toolchains that each size their worker pool to the whole
  machine. TypeScript, ESLint, Stylelint, and Python gates stay `Exclusive`
  until their descendant pools have a tested cap. The cargo `target/` write
  lock is unchanged and is always taken before the budget. Context commands
  (`tsc --traceResolution`, `eslint`, `stylelint`, `esbuild`) draw on the same
  budget and now lead their own process groups like the checks always have.
  `--resource-budget safe|balanced` selects the plan: `safe` is the default
  (one expensive tool and one child worker); `balanced` is the capped opt-in.
  Vitest remains at one CLI worker in both plans because its CLI override must
  not raise a repository's stricter project-level worker ceiling.

- Progress output tells queued work from running work. The stage line reads
  `Running: X (12s) · Queued: Y, Z` instead of naming every runnable check as
  running from the first instant; the task ledger's `started_at` is the moment a
  check was admitted rather than first polled, so the gap from `queued_at` is
  time spent waiting for the machine; and the "still running after Ns" notice
  measures from admission, so a check parked on the budget is no longer reported
  as a slow tool. TUI mode gains `CheckEvent::Running` beside the existing
  `Started`, which now maps to the `Pending` lifecycle.


- `prview gate --strict` now consumes a schema 2.3 typed enforcement
  disposition instead of collapsing every `CONDITIONAL` cause into one exit.
  Clean and proven warnings-only packs exit `0`; confirmed/potential breaking,
  degraded/unknown analysis, quality failures, and other review requirements
  exit `2`; hard blocks remain `1`. `prview gate --strict --fail-on-warnings`
  adds the explicit warning-clean exit `2`. Top-level `prview --ci` deliberately
  keeps its historical Block/quality-failure exit `1`, while
  `--ci --fail-on-warnings` still rejects the canonical pack warning tally.
  `MERGE_GATE.json` now requires the disposition plus typed check and effective
  inline proof, with CLI/MCP/gate sharing one fail-honest reader; packs through
  schema 2.2 remain readable but cannot inject the new warnings-only exception.

- Rust `PUBLIC_API_DIFF` and `BREAKING_CHANGES` now share one repo-backed
  `ApiDelta` computed from the exact base/target Git trees. Existing public-API
  JSON rows and artifact filenames remain compatible, while additive structured
  fields expose stable IDs, counts, confidence, evidence, unknown reasons, and
  provenance in both artifacts, `MERGE_GATE.json`, and `report.json`. Added-only
  API touch is informational; confirmed removals, changes, relocations, and
  visibility changes use the existing breaking-escalation policy, while unknown
  regions degrade confidence without claiming a break. JS/TS keeps the legacy
  diff analyzer behind a side-aware language filter: cross-language renames
  retain only the JS/TS side, including quoted Git paths, without leaking Rust
  lines or losing removed exports. Git headers own section identity: non-null
  file markers must match both decoded header paths, and add/delete null sides
  require coherent mode metadata; malformed or incomplete sections fail closed.
  Exact duplicate base/target OID pairs are
  coalesced before snapshotting, while distinct comparisons keep separate
  provenance. Rust env-requirement detection is preserved as a separate
  non-API signal.

- The three check inventories now project the policy evaluation captured by the
  run instead of re-running eligibility while artifacts are written.
  `RUN.json.checks[]` remains executed-only, while `MERGE_GATE.json` and
  `checks-status.json` retain configured pre-run skips and their original
  reasons. PR review and dashboard skip rows explicitly state that PrView did
  not execute the check and that no external CI status is implied.
- Risk heatmap aggregation now covers the full changed-file set instead of the
  first ten displayed risk rows, with a linear-time path index for large diffs.
  MCP verdict gate rows now pass through every field from the corresponding
  `MERGE_GATE.json` check row. The MCP examples also now document the lowercase
  `passed` status that the runtime already emitted, rather than the stale `PASS`
  example.
- Cargo-audit caveats classify vulnerability and informational advisory keys as
  new, pre-existing, resolved, or unknown-baseline. A changed `Cargo.lock` uses
  a valid base audit report from the effective member or workspace lockfile used
  by the live check; tool or report failure remains explicitly unavailable and
  current findings remain unclassified rather than being treated as
  pre-existing. Invalid current output now fails the check and is reported
  separately as `current-unavailable` instead of clean or resolved.
  Informational-only reports no longer fall through to the generic SARIF
  scraper. Semgrep partial-analysis caveats now name files reported under its
  JSON `errors[]` payload.

### Fixed

- A `--pr` / `--remote` run with nothing snapshot-backed to run now still reads
  the reviewed tree. The shared target snapshot was materialised only when a
  runnable check needed one, which silently excluded two ordinary shapes: a
  run whose complete applicable gate set has sound cache hits, and the fast
  remote-only preset, where the
  snapshot-backed gates all skip and only semgrep (which owns its worktree)
  remains. Both left the task ledger with no scan dir, so every `30_context`
  command fell back to the operator's local checkout while the diffs and
  `MERGE_GATE.json` described the PR's commit — the same
  `PRV-CONTEXT-SNAPSHOT-PROVENANCE` split pack, reached through a quieter door,
  with nothing in `RUN.json` to distinguish it. An off-`HEAD` target is now
  enough to materialise the snapshot on its own, which also resolves the
  run-wide substrate so `RUN.json` stops reporting an all-cacheable `--pr`
  run's replays and skips as being about no particular tree. Such a run pays
  for one `git worktree` its gates do not need. Local reviews (target == `HEAD`) still
  materialise nothing.

- `30_context/*` artifacts are now produced from the same reviewed tree the
  quality gates judged. In `--pr` / `--remote` runs the gates scanned a snapshot
  of the reviewed commit while the context generators (`cargo tree`, `tsc
  --traceResolution`, `eslint`, `npm`/`pnpm` SBOM, `stylelint`, `esbuild`,
  `tauri info`) ran against the operator's local checkout, so one pack could
  describe two different revisions. The run's shared target snapshot is now
  owned by the task ledger and stays alive through artifact generation, and
  every context command's working directory — plus the filesystem probes that
  decide which commands to plan at all, static Tauri command discovery, and its
  diff mapping — follows it. Local reviews are
  unaffected: there the repo root is the reviewed tree.

- The human-readable `PRVIEW CONFIG` panel now caps itself to the active
  terminal width and wraps long refs and preset notes before drawing its right
  wall. Narrow panes no longer let the terminal wrap border glyphs onto stray
  lines; display-width accounting also handles Unicode refs, and terminals too
  narrow for a coherent box (or whose width cannot be queried) receive an
  unboxed fallback.

## [0.7.0] - 2026-08-23

### Added

- `--fail-on-warnings`: opt-in escape hatch that makes `--ci` exit `1` when any
  check reports warnings. It is only meaningful together with `--ci` (clap
  rejects it otherwise) and it restores the pre-change CI behaviour for teams
  that want a warnings-clean trunk. `prview gate` is untouched — its exit codes
  come from the verdict contract, not from this flag.
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
  re-reads the tree it is about to analyse. Paths are taken from git's raw
  bytes: a filename that is not valid UTF-8 is fingerprinted by those bytes and
  its content read through an OS-native path, where a single `<non-utf8>`
  placeholder previously merged every such entry into one line whose content
  lookup found nothing.
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
  and a `skipped` reason; rows for checks that ran carry `skipped: null`. Those
  rows identify the gate through the canonical name→id mapper, like every other
  id in the pack: a skipped check was labelled with a naive slug of its display
  name, so the same configured gate appeared as `typescript` when skipped and
  `tsc` when it ran (likewise `cargo_check`/`cargo`, `vitest`/`tests`) and could
  not be correlated. `REPORT.json.checks_skipped[]` is corrected with it. A
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

### Changed

- **`--ci` exit code for a warnings-only run: `1` → `0`.** Warning-level checks
  no longer break `quality_pass`, and `--ci` still exits `1` only on `BLOCK` or a
  broken quality gate — so a run whose worst signal is a warning now exits `0`.
  Pass `--ci --fail-on-warnings` to keep the old exit. Runs with a real failure,
  and every `prview gate` exit code, are unchanged.
- **BREAKING (behavioral): an unreadable `MERGE_GATE.json` is now an execution
  error, not a guessed verdict.** `prview --json` / `--ci` used to fall back to
  re-deriving the decision from the in-memory policy engine when the gate
  artifact was missing or unparsable, publishing `allow_merge = recommendation
  != block` — the only path in the codebase where `allow_merge: true` could
  coexist with a `CONDITIONAL` verdict, contradicting the documented
  `allow_merge == (verdict == "PASS")` invariant. That fallback is removed:
  a missing, unparsable, or unknown-schema gate artifact now prints an error and
  exits `3`, the same execution-error code `prview gate` already used. This also
  applies to `--update` runs that re-read an earlier pack, so a truncated
  previous run reports the failure instead of resurrecting a plausible verdict.
- **`MERGE_GATE.json` readers check `schema_version`.** A pack with an unknown or
  unparsable MAJOR is rejected fail-loud (`exit 3` on the CLI, `storage_corrupt`
  on the MCP surface), and so is a `schema_version` that is present but is not a
  `MAJOR.MINOR` string — a number, an object, or an explicit `null` used to be
  read as "field absent", i.e. as a legacy pack, which is the opposite of what it
  means. A version with extra components (`2.1.3`) is rejected rather than
  truncated to `2.1`, so "readable by prview" cannot drift away from the exact
  set `tools/validate_merge_gate.py` accepts. A newer MINOR of a known MAJOR is
  read and reported with a `schema_forward_compat:` caveat — on every known
  MAJOR, so a `1.9` pack is now caveated instead of accepted in silence — and the
  MCP surface marks that read `normalized: true`, as the documented contract
  already promised. Version components must also be spelled canonically:
  `u32::from_str` accepts leading zeros and a leading `+`, so `02.2`, `2.02` and
  `+2.2` all parsed to the known `(2, 2)` and were read as the current schema
  while the validator rejects those exact strings. An absent `schema_version` stays accepted: pre-2.1 packs
  predate the field, and the documented `ALLOW`/`HOLD` verdict tolerance is
  unchanged.
- **A versioned pack without a `decision` object is a corrupt artifact.** The CLI
  reader fell back to treating the gate's ROOT as the decision, so a pack that
  states `schema_version: "2.2"` and then carries no `decision` (or a non-object
  one) normalized quietly to `BLOCK` / `allow_merge: false` with an
  `unknown_verdict:` caveat — a verdict nothing in the pack ever stated. It now
  exits `3`, matching `tools/validate_merge_gate.py` (which requires `decision`
  at every version) and the `prview mcp` adapter (which already returned
  `storage_corrupt`). A pack with NO `schema_version` predates the field and
  keeps the legacy tolerance: its root is still read as the decision.
- **The legacy tolerance is now whole on both readers.** The `prview mcp` adapter
  required a `decision` object unconditionally, so a genuine pre-2.1 pack — no
  `schema_version`, signals at the root — was answered `storage_corrupt` by the
  MCP surface while the CLI read the very same file and printed a verdict. One
  artifact cannot be simultaneously readable and corrupt depending on which
  surface asks. Both readers now select the decision object through a single
  `gate::select_decision_object`: `decision` when it is an object, the root when
  the pack states no `schema_version`, and fail-loud otherwise. The corruption
  rule for versioned packs is unchanged; only the disagreement is gone.
- **A wrongly typed decision signal is a normalization, not an absent field.**
  `verdict: "PASS"` beside `merge_recommendation: 7` used to collapse through
  `as_str()` into "no recommendation", so the `prview mcp` adapter returned a
  decision derived from the surviving signal with `normalized: false` and no
  caveat — a field ignored in silence, which the MCP contract forbids. Each
  decision signal now distinguishes absent from present-but-untypable and emits
  an `unreadable_verdict:` / `unreadable_merge_recommendation:` /
  `unreadable_allow_merge:` caveat with `normalized: true`. A pack with no
  usable signal at all is still `storage_corrupt`.
- **The CLI reader names wrongly typed signals too, and refuses to approve on
  them.** The `unreadable_*` discipline above shipped on the MCP surface only;
  the CLI still went through `as_str()` / `as_bool()`, so `verdict: 7` was
  reported as `unknown_verdict: … carries no verdict` (a claim about a field that
  was in fact present), `merge_recommendation: 7` fell through to
  `review_required`, and `allow_merge: "true"` silently became `false`. Worse,
  a pack with a valid `verdict: "PASS"` beside a mistyped `merge_recommendation`
  published a `PASS` derived from a decision block the reader had only partly
  read. Both readers now share `gate::readable_signal`: a present-but-untypable
  field emits the same `unreadable_<field>:` caveat on `--json`, and — matching
  the unknown-verdict rule already in place — forces every derived axis
  conservative (`verdict: "BLOCK"`, `allow_merge: false`,
  `merge_recommendation: block`, `--ci` exit `1`). A well-typed pack gains no
  caveat and is unaffected.
- **Unknown verdicts are reported instead of silently absorbed.** The CLI still
  collapses an unrecognized verdict to `BLOCK`, but now says so through a new
  optional `caveats` array on the `--json` summary (`unknown_verdict: …`) — the
  reader no longer presents a normalization as something it read. The MCP
  `verdict` surface likewise reports `unknown_verdict` /
  `unknown_merge_recommendation` and sets `normalized: true` instead of dropping
  the unparsable field on the floor. The `--json` summary keeps
  `schema_version: "cli-json/v1"`: `caveats` is additive and omitted when empty.
  A verdict the CLI collapsed to `BLOCK` now also forces the axes derived beside
  it: `allow_merge` is `false` and `merge_recommendation` is `Block` regardless
  of what the same unreliable decision block claimed. A pack with an unreadable
  verdict but `allow_merge: true` and `merge_recommendation: "approve"` used to
  publish `verdict: "BLOCK"` next to an approval — breaking the
  `allow_merge == (verdict == "PASS")` invariant — and, because
  `compute_exit_code` keys off the recommendation, `--ci` exited `0` on it.
- Human stdout no longer prints "All checks passed!" when no gate artifact was
  readable. The raw check tally is not a verdict; the summary now names the
  missing truth.
- **`report.json` schema_version: `1.0` → `2.0`.**
  `quality.coverage.heuristic_ratio` is `null` when nothing was measured
  (previously a misleading `1.0`) and is accompanied by new `measured: bool`
  and optional `not_measured_reason` fields; `quality.heuristics` omits its
  counters on a skipped scan. No field was removed or renamed, but a field that
  was always a number can now be `null` and counters can now be absent, so a
  decoder written against `1.0` does not parse every pack — that is a MAJOR, not
  an additive MINOR. Consumers reading `heuristic_ratio` must handle `null` —
  the bundled dashboard PR-comment generator renders it as `not measured`, and
  `history.rs` already treats a missing value as "no baseline".
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

### Fixed

- **The blocker flag and the blocker list are certified as one fact.** The
  emitter computes `policy_allow_merge = blocking_issues.is_empty()` after the
  last entry is pushed and writes both verbatim, but the contract validator used
  that relation only in the harsher direction — a listed blocker raises the
  verdict a pack must clear — which left the two halves free to contradict each
  other outright. `policy_allow_merge: true` beside a listed blocker certified
  clean, telling a reader that trusts the flag that policy let the merge through
  while the list beside it named what blocked it. From schema 2.2, where both
  fields are required, `tools/validate_merge_gate.py` enforces the equivalence in
  both directions: `true` with blockers and `false` without them are both
  rejected. This completes the reconciliation port rather than adding a rule to
  it — same shape as the `quality_pass` / `quality_failure_details` equivalence,
  and distinct from the older "no `allow_merge: true` beside a blocker" check,
  which is about the merge verdict rather than the policy flag it derives from. A
  test in `src/artifacts/merge_gate.rs` pins the flag to the list across the
  emitted packs, so a second input to the flag fails the emitter instead of
  making the validator reject prview's own output. Probed against every pack on
  disk: no pack from a real run is rejected.
- **An `impl` owner is part of a declaration's site.** A `pub` associated item
  moved between two impl blocks in one file — `pub const VALUE` leaving `impl A`
  and appearing in `impl B` — matched on file, kind, name and text with an empty
  scope on both sides, so the exact pairing consumed it and `A::VALUE` vanished
  from the report entirely. Impl owners now ride the same stack as inline
  modules, recorded as the header text with whitespace collapsed and nothing
  parsed. The asymmetry is deliberate: two KNOWN and different owners never pair,
  while an owner the hunk never showed stays unknown and pairs with anything, so
  the accepted unseen-opener limit is untouched. Over 211 commits of this
  repository the reports are identical before and after; over 708 crates.io
  release pairs removals move 30,555 → 30,694 and signature changes 53,938 →
  53,805, i.e. mostly a reclassification of a real owner change. Recorded limit,
  mirroring the `cfg` operand-ordering one: the same owner written with a
  different path qualifier reads as two owners (40 of 2,784 blocked pairings,
  all in one crate) — closing it means parsing types.
- **The contract validator now certifies the reconciliation, not just the
  shape.** `tools/validate_merge_gate.py` checked each decision field on its own,
  so a pack stating `verdict: "PASS"` beside `analysis_status: "incomplete"`, a
  `block` recommendation and `policy_allow_merge: false` validated OK — while
  every reader normalizes that same artifact to `BLOCK`. The readers were already
  protected; the hole was in CERTIFICATION. From schema 2.2 the validator ports
  their whole rule: it requires the remaining decision axes (`analysis_status`,
  `merge_recommendation`, `policy_allow_merge`) with the vocabularies the typed
  enums emit, and rejects a `verdict` milder than the most conservative axis
  stated beside it. The rule is one-directional on purpose: a HARSHER verdict is
  legal, because a semgrep scan that passes with parse errors writes `approve`
  beside `degraded` and the contract turns that into `CONDITIONAL`. A test in
  `src/policy/engine.rs` pins both enum spellings to the words the validator
  lists. Probed against 3,547 real packs on disk (2,039 at schema 2.2): no
  legitimate pack is rejected.
- **Bytes inside a literal now traverse the whole `cfg`-attribute pipeline
  verbatim.** The accumulator glued an attribute's physical lines together with
  nothing between them, and the caller trimmed each line before the tracker saw
  it, so both the line break and a continuation's indentation vanished from
  inside the value: `#[cfg(api = "a\nb")]` produced the same guard as
  `#[cfg(api = "ab")]`, and a declaration that really left one configuration
  paired with its re-add under another. This is the third finding of one shape,
  after the delimiter count and the whitespace strip, so it is closed as an
  invariant rather than patched again. The tracker now takes the raw line, joins
  a physical break with `\n` exactly when a literal is open across it, and trims
  nowhere — after the dense view there is no whitespace left outside a literal,
  so a trim could only eat value. Layout outside a literal is still normalized:
  re-indenting or re-wrapping a predicate is the same gate. Of 568,128 `cfg`
  attributes in the local crates.io registry, 4 carry a literal spanning a line
  break, 2 of them gate an item, and none collide.
- **An unreadable `checks` list is not an empty one.** `checks` present but not
  an array left the warning tally at zero and fell back to the checks the run
  itself executed — which on an unchanged `--update` run is none — so
  `--ci --fail-on-warnings --update` exited `0` on a reused pack whose warning
  list the reader could not read. It now counts as at least one warning and says
  so in the existing `unreadable_checks:` caveat. This is the r27 rule one level
  up, on the container instead of an entry, and no legacy carve-out applies:
  `checks` has been emitted since schema 1.0 and `validate_merge_gate.py` has
  always required an array there, so a non-array was never a valid shape. An
  ABSENT `checks` keeps its tolerance — a pack that states no list may simply
  predate this build.
- **Whitespace inside a `cfg` value is part of the gate.** The guard tracker
  normalized an attribute by stripping whitespace from its whole text, literals
  included, so `#[cfg(api = "a b")]` and `#[cfg(api = "ab")]` produced one guard:
  a declaration that really left builds configured with `--cfg 'api="a b"'`
  paired with its re-add under another value and produced no finding. The strip
  is now `SourceScanner`'s own dense view, which removes spacing only where it
  can see the spacing is outside every literal, so reformatting an attribute is
  still not a different gate. A fix by construction rather than by frequency: of
  524,530 gating attributes in the local crates.io registry only 3 carry
  whitespace inside a value literal, and none of them collide.
- **A check status outside the emitted vocabulary is unreadable, not clean.**
  `checks[].status` is a closed, case-sensitive set — `passed`, `failed`,
  `warnings`, `skipped`, `error` — but the CLI tallied warnings by comparing
  against the single string `"warnings"`, so any other spelling counted as "not a
  warning" and `--ci --fail-on-warnings --update` exited `0` on a reused pack
  whose warning signal it could not read. `tools/validate_merge_gate.py` accepted
  any non-empty string there, so such an artifact even passed the repository
  gate. Both sides now name the vocabulary: the reader counts an unrecognized
  status toward the tally and raises an `unreadable_check_status:` caveat naming
  the checks, and the validator rejects the pack. Case is deliberately not
  folded — normalizing `"WARNINGS"` silently would hide that the pack is
  off-contract, and the tally is the same either way. The vocabulary lives as
  `CheckStatus::EMITTED` next to `CheckStatus::as_str`, with a test pinning the
  two together.
- **An attribute's delimiters are counted with its literals removed.** The
  `cfg`-guard tracker resolved comments away with a carried scanner but counted
  brackets with a literal state of its own, reset at every line — so a literal
  opened on an earlier line was invisible to it. A `)` typed inside a multi-line
  `#[doc = r#"…"#]` balanced the attribute early and the literal's remaining
  lines then cleared the pending `cfg`; a `#[must_use = "… \` continued onto the
  next line had its own closing quote read as an opener, swallowing the `]`, so
  the attribute never closed and absorbed the real `#[cfg(…)]` below it. Either
  way both diff sides came out unguarded, the identical declaration text paired,
  and a configuration-specific removal produced no finding. The counter now runs
  on a literal-free view from a second scanner walking the same lines, while the
  guard text keeps its literals so `feature = "a"` and `feature = "b"` stay two
  gates. Measured over the local crates.io registry: of 237,368 `cfg`-guarded
  attribute runs reaching a public declaration, 8,793 wrap, 90 carry a literal
  spanning the break, 13 a raw string, and 9 balanced wrongly.
- **Re-indenting the inside of a multi-line public constant is a value change
  again.** Continuation lines reached the breaking-change accumulator already
  trimmed, so whitespace at a line edge INSIDE a string literal — which is value,
  not layout — never reached the comparison. Two literals differing only in their
  indentation produced identical identities, and the exact-match pass consumed
  the addition: a changed public value left no finding at all. The accumulator
  now takes the raw line and normalizes per edge — the leading edge is kept when
  the previous line left a literal open, the trailing edge when the line itself
  does — so a reflow outside a literal stays the no-op it must be, and a trailing
  comment's leading gap still contributes nothing. Measured over the local
  crates.io registry, of 200,553 multi-line public declarations 640 continuation
  lines sit at a literal edge and 272 carry whitespace the old view dropped.
- **`tools/validate_merge_gate.py` now requires a boolean `quality_pass` from
  schema 2.2.** The validator checked the field's agreement with the failure
  details but never its presence or type, so a 2.2 pack stating
  `quality_pass: "false"` — or omitting it — was certified clean while both
  decision readers normalize a present-but-unreadable signal to BLOCK. The
  contract gate was therefore passing artifacts the CLI and MCP refuse to trust.
  The 2.2 writer emits the field unconditionally as a boolean, so requiring it
  there is safe; absence stays forgiven below 2.2, where readers derive the flag
  instead.
- **A body-less test item can now end its own test context.** After a top-level
  `=` an item states a value, but the perf tracker kept reading `<` as a generic
  opener there, so `#[cfg(test)] const ENABLED: bool = 1<2;` left the signature's
  bracket depth above zero — the very thing the `;` close tests. The item could
  not end the context it opened, and every loop or query below it was recorded as
  test-only and dropped from the signal. Angle tracking now stops at the item's
  top-level `=`, the same rule the declaration scanner already applies. The
  reported shape is a comparison, but the corpus idiom is the compact shift
  (`const Reverse = 1<<8;`, as objc2 generates its bitflags): of 2,206,540
  single-line `const`/`static`/`type` declarations in the local crates.io
  registry that end at their own `;`, 1,069 left the depth stuck open before this
  change and 64 still do — and those 64 are an array type wrapping to the next
  line, where holding the depth open is exactly right.
- **A turbofish return type no longer hides a changed public signature.**
  `pub fn run() -> Buffer::<{` is a valid return type — rustc accepts
  `Type::<…>` in type position — but its `<` follows a `:`, which the scanner did
  not accept as opening a generic argument list. The list went uncounted, the
  const block's `{` read as the item's body opener, and both diff sides
  finalized at that identical prefix: they paired as an unchanged re-add and a
  changed const argument, which is a changed public return type, produced no
  finding at all. `:` now joins an identifier and a closing `>` as a predecessor
  that opens a list; whitespace still does not, so a comparison is still not a
  list. Verdict-neutral where it is not needed — over all 4,334,018 public
  declaration lines in the local crates.io registry the old and new rules
  disagree on none, because a turbofish that closes on its own line nets out
  either way. What changes is a list left open at end of line.
- **`tools/validate_merge_gate.py` now rejects a `quality_pass` that
  contradicts its own evidence.** The flag and `quality_failure_details` are one
  fact written twice — the emitter sets `quality_pass` to
  `!QualityFailureSummary::has_new_failures()` and serializes the very details
  that answer it — but the validator checked each side's shape and never
  compared them. `quality_pass: true` beside
  `{"origin": "failure", "classification": "introduced"}` therefore certified
  clean, and both decision readers trust the permissive scalar, so a
  validator-clean pack could approve an explicitly introduced failure. The check
  is an equivalence: `quality_pass` is true if and only if no detail has
  `origin: "failure"` with a classification other than `pre-existing`. The
  `pre-existing` carve-out is load-bearing — a failure that predates the diff is
  emitted beside `quality_pass: true` on purpose, so the simpler one-way rule
  would have rejected packs prview itself writes. Packs without the field are
  untouched.
- **A compactly written comparison in a const argument no longer mutes
  production code.** The perf tracker judged `<` a generic opener whenever it
  followed an identifier, which reads `Buffer<{ 1 < 2 }>` correctly and the same
  type written `Buffer<{1<2}>` wrongly — `<` after a digit looks exactly like `<`
  after an identifier. The signature's bracket depth then stayed above zero, the
  real body brace read as another type-level brace, the test context never
  closed, and every loop or query below the test was recorded as test-only and
  dropped from the signal. Spacing is formatting, so it can no longer decide the
  verdict: bracket tracking is now frozen inside a brace opened within the
  signature, where a const argument holds an expression and a destructured
  parameter holds a pattern and `<`/`>` are operators in both.
- **A comparison inside a const argument no longer swallows the item body.**
  `pub fn run() -> Buffer<{ 1 < 2 }> {` counted the comparison as another
  generic opener, the argument list's own `>` closed only that phantom level,
  and the depth was still above zero at the real body brace — which read as a
  further const argument, absorbed the body, and turned a body-only rewrite into
  a phantom `ChangedSignature`. Inside a const block `<` and `>` are operators,
  so the generic depth is now frozen there. Nothing is lost: whatever such a
  block states about generics closes what it opens — a turbofish
  (`{ size_of::<u32>() }`) or a qualified path (`Uint<{ <Self>::LIMBS / 2 }>`),
  which are also the only shapes the local crates.io corpus carries. Those
  survived the previous rule by cancellation, the block's stray `>` closing the
  outer list; they now reach the same verdict by construction.
- **A signature edited in place is no longer swallowed by the context lines
  around it.** A hunk interleaves two texts, and the scanner reconstructs both:
  the before side is context ∪ removed lines, the after side is context ∪ added
  lines. It used to end BOTH pending declarations at the first line from the
  other side, so the everyday shape of an edited signature — `pub fn f(`
  retouched on both sides, a shared `x: u8,`, then `-old: u16,` / `+new: u32,`
  and a shared `) {` — finalized to two identical openers, paired as an
  unchanged re-add, and reported the parameter change nowhere. A `-` line now
  extends only the removed side, a `+` line only the added side, and a context
  line extends whichever side still has a declaration open. Context lines only
  CONTINUE a declaration and never start one: a `pub` item first seen on a
  context line is unchanged by the patch. `MAX_DECL_CONTINUATION_LINES` (32)
  still bounds growth and a hunk header still finalizes both sides, so the
  reconstruction stays inside the hunk that emitted it.
- **Braces in a stacked test attribute no longer end the test context.** An
  attribute's brackets belong to the attribute, never to the item it annotates,
  but the brace scan read them as the annotated item's: with `#[rstest]` stacked
  over a brace-bearing `#[case(…)]`, the attribute's `{` was taken as the body
  opener and its `}` closed the context on the same line, so the test function
  below was classified as production and a query in its loop surfaced as a
  phantom regression. The scan now tracks attribute depth per character and
  skips what is inside one. The plain `#[case(Case { id: 1 })]` was safe only by
  accident — its `[` and `(` hold the signature depth above zero — while
  `#[case(1 > 0, 2 > 1, Case { id: 1 })]` clamps that depth back to zero first
  and reaches the bug; skipping attributes removes the class rather than the one
  shape.
- **A legacy `PASS` pack no longer fails `--ci` on the CLI while the MCP adapter
  approves it.** A decision written before `quality_pass` existed —
  `{"verdict": "PASS", "merge_recommendation": "approve", "allow_merge": true}`
  — reconciled correctly to `PASS`, because an absent field adds no rank, but the
  summary then published `quality_pass: false` from a bare default, derived
  `analysis_status: incomplete` from that, and exited `1` under `--ci`. The two
  readers answered the same artifact differently. Ranking an absent field and
  publishing one are separate questions: an absent axis is now derived from the
  reconciled outcome, so a reconciled `PASS` — which the contract permits only
  when quality passes and the analysis is complete — publishes both, and a
  decision held below `PASS` stays conservative on both. The absent/mistyped
  split is untouched: an unreadable value normalizes the decision to `BLOCK`, so
  nothing can be inferred as passing from it.
- **An incomplete analysis or a stated blocker can no longer be published as an
  approval.** The conservative reconciliation ranked `verdict`,
  `merge_recommendation`, `allow_merge` and `quality_pass`, but read
  `analysis_status` only afterwards for display and `blocking_issues` only for
  passthrough — so a pack shaped `verdict: "PASS"`, `merge_recommendation:
  "approve"`, `allow_merge: true`, `quality_pass: true` published a clean
  approval even when it also stated `analysis_status: "incomplete"` or listed a
  blocking issue, on the CLI and the MCP surface alike. The contract permits
  `PASS` only when the analysis is `complete`, and an entry reaches
  `blocking_issues` only from a check whose `merge_impact` is `Block`. Both now
  rank: `degraded`/`incomplete` as `CONDITIONAL`, a non-empty `blocking_issues`
  (and its restatement `policy_allow_merge: false`) as `BLOCK`, each named in the
  `core_inconsistency:` caveat and typed through `gate::readable_signal` so a
  mistyped one normalizes conservatively. `analysis_status: "complete"`,
  `policy_allow_merge: true` and an empty `blocking_issues` state no rank — they
  are preconditions of a `PASS`, not grants of one — and absence still states
  nothing, so older packs read exactly as before.
- **The decision axes are now enumerated in the contract.** Every field the
  `decision` object may carry has a row in the ranking table of
  `docs/contracts/merge_gate.md` saying whether it ranks and why, under one rule:
  an axis states a rank only when its value RULES OUT a more permissive outcome.
  The deliberate exclusions are recorded with their reasons — `recommended_merge`
  restates `merge_recommendation`, `recommended_label` has an open vocabulary,
  the `quality_failures` arrays are populated by warning-origin entries that
  never flip `quality_pass`, and `quality_failure_details` is the evidence behind
  that axis rather than an axis of its own. A field added to `decision` without a
  row is an unfinished change.
- **A `quality_pass` that cannot be typed is no longer read as absent.** Both
  readers took that axis with a bare `as_bool()`, which returns nothing for a
  present-but-mistyped value just as it does for a missing one — so a pack
  stating `quality_pass: "false"` beside a clean approval was read as a pack
  written before the field existed, and published `PASS` with `allow_merge: true`
  and no caveat at all, on the CLI and the MCP surface alike. `quality_pass` now
  goes through the same `gate::readable_signal` as `verdict`,
  `merge_recommendation` and `allow_merge`: a stated-but-unreadable axis
  normalizes to `BLOCK` and is named by an `unreadable_quality_pass:` caveat. An
  absent `quality_pass` is still silent and still states no rank, so packs
  written before the field are unaffected.
- **A failed quality axis can no longer be published as a `PASS`.** The
  conservative reconciliation ranked `verdict`, `merge_recommendation` and
  `allow_merge` but read `quality_pass` separately, afterwards — so a pack
  shaped `verdict: "PASS"`, `merge_recommendation: "approve"`,
  `allow_merge: true`, `quality_pass: false` published a clean approval with
  `allow_merge: true`, on the CLI and on the MCP surface alike, where automation
  could act on it. A stated `quality_pass: false` now ranks as `CONDITIONAL` on
  both readers, exactly like `allow_merge: false`, and is named in the
  `core_inconsistency:` caveat. `quality_pass: true` still states no rank — a
  quality-clean run is held at `CONDITIONAL` by a breaking-change escalation, so
  one axis may not soften a verdict the others agree on — and an ABSENT
  `quality_pass` still states nothing, so packs written before the field are
  read exactly as before.
- **A `|` in a declaration no longer breaks the `BREAKING_CHANGES.md` tables.**
  Declaration text went into a markdown table verbatim, and Rust states bitwise
  or, patterns and closures with the table's own delimiter — so a row reporting
  `pub const MASK: u32 = READ | WRITE;` opened extra columns and rendered as
  garbage exactly where the declaration mattered. Every cell carrying source
  text now escapes `|` as `\|`, which is what GitHub's table parser needs: it
  splits on unescaped pipes before any inline markup runs, so a code span was
  never protection. The span is also fenced by a backtick run longer than any
  inside the cell, so a declaration stating a backtick of its own —
  `pub const TEMPLATE: &str = r#"`value`"#;` — no longer closes its own code
  span partway through and renders the remainder as prose.
- **`#[cfg(not(test))]` no longer mutes a production performance finding.** The
  perf tracker opened inline test context on the bare token `test` appearing
  anywhere inside a `cfg` predicate, so a query-in-loop under
  `#[cfg(not(test))]` — code compiled into every build EXCEPT the test one — was
  recorded as test-only and dropped, and so was one under
  `#[cfg(any(test, feature = "bench"))]`, which compiles outside the test build
  whenever the feature is on, or under `#[cfg(feature = "__internal-test")]`, a
  feature that merely has `test` in its name. This inverted the module's own
  rule that ambiguity resolves toward production. Only a gate that provably
  holds solely in a test build now opens the context: an exact `#[cfg(test)]`,
  an `#[cfg(all(…))]` naming `test` among its operands, `#[test]` /
  `#[tokio::test]` / `#[rstest]`, and `mod tests`. Measured over the local
  registry (58,586 files), of the 11,030 attributes the old pattern read as test
  context 83.62% are exactly `cfg(test)` and 6.76% are `all(…, test, …)` — the
  remaining 9.62% are the ones it was getting wrong. `all` is commutative, so
  the operand's position carries no meaning: `all(feature = "bench", test)` is
  read exactly like `all(test, feature = "bench")`, where matching only the
  first operand made the same predicate production or test context depending on
  how it was written (72 further attributes over that registry, none lost). The
  operand must be a direct one, so `all(not(test), …)` — which proves the
  opposite — and `all(any(test, …), …)` stay production. The predicate is also
  read as a whole attribute rather than per physical line: rustfmt wraps a long
  one, and a `#[cfg(all(` / `feature = "bench",` / `test` / `))]` spread over
  four lines matched on none of them, so its test-only item was read as
  production and its query-in-loop surfaced as a phantom regression. The lines
  of one attribute are now joined and matched once, on the line that closes it,
  bounded to 8 lines so an attribute that never closes is dropped instead of
  swallowing the rest of the hunk. The shape is rare — 10 occurrences over that
  registry, every one a genuine `all(test, …)` gate.
- **A block or struct-literal initializer no longer hides a changed public
  constant.** `pub const LIMIT: usize = {` and `pub const ZERO: Self = Self {`
  had their `{` read as the item's body opener, so both diff sides finalized at
  their identical first line, paired as an unchanged re-add, and a changed
  expression inside the block produced no finding at all. After a top-level `=`
  the item states a value and runs to its `;`, and a `;` inside the initializer
  terminates a statement rather than the declaration. Only a top-level `=`
  counts — inside a generic argument list one states a default
  (`struct Foo<const N: usize = 4>`) or an associated type
  (`impl Iterator<Item = u8>`), both still followed by a real body brace.
  Measured over the local registry (58,586 files, 1,960 crates, 4,334,320 public
  declaration lines) this changes the verdict on 2,465 lines, every sampled one
  a public constant with a multi-line struct-literal or block initializer.
- **A reflowed declaration is no longer reported as a changed signature.** The
  comparison identity preserved every physical line break, so
  `pub type Alias =` followed by `u32;` was a different declaration from
  `pub type Alias = u32;` — a purely cosmetic rewrap produced a
  `ChangedSignature` whose "before" and "after" printed as the same string, and
  could escalate the verdict. A break is now kept only where the previous line
  left a string literal open, which is where it is part of the value; elsewhere
  the lines are joined with a space. For the same reason a line contributing no
  code is still dropped from the identity except inside a literal, where a blank
  line is a blank line in the value.
- **A const argument that is not the first one no longer hides a public type
  change.** The breaking-change scanner recognized `Buffer<{ LIMIT }>` as
  type-level syntax by the exact `<{` sequence, so `Buffer<u8, { LIMIT }>` —
  where the brace follows a comma, which is where a const generic usually sits —
  finalized the declaration at its opener. Both diff sides then held the same
  prefix, paired as an unchanged re-add, and the changed const expression below
  produced no finding. The scanner now tracks the generic argument list itself.
  `<<` is consumed whole so a shifted public constant still terminates at its
  `;`, and measured over the local registry (59,946 files, 2,025 crates,
  4,354,142 public declaration lines) the new rule and the one it replaces judge
  zero lines differently.
- **The MCP adapter and the CLI now answer the same way about a decision they
  cannot rank.** A pack that stated a signal outside the vocabulary — a
  `verdict: "PROBABLY"`, or nothing but `allow_merge` — was read as a
  conservative `BLOCK` summary by the CLI and refused as `storage_corrupt` by
  `prview mcp`, one artifact with two answers. `storage_corrupt` is now reserved
  for a decision block stating none of `verdict`, `merge_recommendation` and
  `allow_merge`; a stated-but-unrankable signal is a decision the pack gave, and
  the adapter normalizes it exactly as the CLI does, with a caveat and
  `normalized: true`. The substitution governs the axes published beside it, so
  an unreadable verdict beside `merge_recommendation: "approve"` no longer reads
  as an approval on the MCP surface while the CLI blocks on the same bytes.
- **A self-consistent `BLOCK` pack no longer reports contradicting itself.**
  Both readers compared `allow_merge` to the numeric rank of the winning
  verdict, but `allow_merge` has two values and `false` ranks as `CONDITIONAL` —
  so `verdict: "BLOCK"` beside `merge_recommendation: "block"` and
  `allow_merge: false`, the shape every blocking run writes, raised a
  `core_inconsistency:` caveat naming a disagreement that was not there. The
  check now compares the textual axes to the published verdict and `allow_merge`
  to the flag actually published.
- **`--ci` strictness no longer depends on which preset the run resolves to.**
  `--update` outranks `--ci` when the execution preset is picked, so
  `prview --ci --fail-on-warnings --update` published `execution_mode: "update"`
  — and the exit code read its strictness off that label. Both `--ci` exits, the
  `!quality_pass` one and the warning hardening clap insists on `--ci` for, were
  therefore inert for exactly the combination CI jobs use. Strictness now follows
  the flag the caller typed. On top of that, an `--update` run with no new
  commits forced exit `0` outright: it reuses the previous pack and reports it,
  so a second invocation turned a warning-carrying — or outright `BLOCK` — pack
  green. Such a run now derives its exit from the pack it reused, like every
  other run; `--soft-exit` stays the one deliberate way to ask for `0`.
  (`output::compute_exit_code` takes the strictness explicitly as a result.)
- **A `MERGE_GATE.json` decision that states nothing is corrupt, not a BLOCK.**
  A pack shaped `{"schema_version":"2.2","decision":{}}` passed the CLI's
  structural check — the object is there and it is an object — and then
  normalized to `BLOCK` and published a summary with `--ci` exit `1`, for an
  artifact that never gave a verdict. The other three readers already refused
  it: the MCP adapter with `storage_corrupt`, `prview gate` on deserialization,
  and `tools/validate_merge_gate.py` on its required fields. The CLI now
  requires at least one of `verdict`, `merge_recommendation` or `allow_merge`
  and exits `3` without them, so the readers agree on the same pack. Presence is
  the test, not recognizability: a stated `verdict: "PROBABLY"` is still read
  and still collapses to `BLOCK` with its caveat.
- **A block comment no longer takes a `cfg` guard down with it.** The guard
  tracker read `/** Configuration for the a build. */` standing between
  `#[cfg(feature = "a")]` and the item it guards as a new item, so both sides of
  a diff came out unguarded, the identical declaration text paired as an
  unchanged re-add, and a struct that really disappeared for the `a` build
  produced no finding at all. Comments are now resolved away before the tracker
  reads a line, by the same per-side scanner the declaration accumulator uses:
  the comment reaches it as the blank line it is, wrapped over as many lines as
  it likes. The same resolution retires the recorded limit on the attribute's
  delimiter counter — `/* ))) */` inside a wrapped `#[cfg(any(` predicate no
  longer balances the attribute early. Literals stay in that view, because
  `#[cfg(feature = "a")]` and `#[cfg(feature = "b")]` are different gates.
- **A const argument in a type no longer ends the declaration.** `pub type Alias
  = Buffer<{` opens a const argument, but the accumulator read that `{` as the
  item's body opener and finalized there. Both diff sides held the same
  truncated prefix, paired as an unchanged re-add, and a changed const
  expression on the lines below — a different public type — produced no finding.
  A `{` directly after a `<` is now carried to its matching `}`. The rule is
  that exact sequence rather than generic-argument tracking, because `<` is also
  the shift operator: 4,666 public `const`/`static` declarations in the local
  registry state a shift on their own line, against 6 that carry a `<{`.
- **A changed multi-line array constant surfaces again.** An array type states
  its length with a `;` — `pub const TABLE: [u8; 2] = [` — and the declaration
  accumulator accepted that `;` as the terminator. Both sides of a diff
  finalized at their identical opener, paired as an unchanged re-add, and the
  changed values below produced no finding at all. Square brackets are now
  counted like parentheses before a `;` ends a declaration.
- **A literal spanning two lines is no longer the same value as one with a
  space.** The comparison identity joined physical lines with a space, including
  the lines a literal spans, so a rewritten public constant paired away as an
  unchanged re-add. Lines are now separated by the boundary that separated them.
- **A raw-identifier module is its own scope.** The inline-module parser stopped
  at the `#`, recording both `mod r#type` and `mod r#match` as `r`: two
  namespaces looked like one, and a removal from the first was cancelled by an
  unrelated addition in the second.
- **A comparison inside a const argument no longer holds a test context open.**
  The perf tracker counted the `<` of `Buffer<{ 1 < 2 }>` as a generic opener,
  leaving the signature depth stuck above zero so the real body brace was read
  as another type-level brace. The context never closed and every production
  loop and query after the test was muted. A `<` now opens a generic only where
  one can be — directly after what it parameterises.
- **Rewording a comment inside a declaration is no longer a signature change.**
  Declarations were compared on their verbatim text, comments and all, so a
  remove+re-add of a byte-identical public signature whose internal comment had
  been rewritten came out as a `ChangedSignature` — a breaking-change claim
  about text no consumer can observe. Pairing now compares a comment-free view
  of the same lines while `BREAKING_CHANGES.md` keeps showing the declaration as
  written. String and char literals stay in that view: a literal is code, so a
  changed `pub const GREETING: &str = "hello";` still surfaces.
- **A brace in a test function's signature no longer ends its test context.**
  The perf tracker treated the first `{` after a test marker as the item's body
  opener, but a brace in type or pattern position — `fn run() -> Buffer<{ LIMIT
  }>`, or the extractor idiom `fn handler(Parameters(Req { field }):
  Parameters<Req>)` — balances before any body exists. The next line then looked
  like the item closing again, so the context ended at the signature and every
  loop and query in the test body was reported as a production perf regression.
  The body opener is now the first brace outside the signature's bracket
  nesting.
- **`report.json` names the origin of every quality-failure detail.**
  `gate.quality_failure_details[]` carried `name` + `classification` while
  `MERGE_GATE.json` has carried `origin` (`"failure"` / `"warning"`) since
  schema 2.2, so the two artifacts of ONE run disagreed about what "failure"
  meant: a consumer reading `introduced_quality_failures: ["Rustfmt"]` next to
  `quality_pass: true` in `report.json` had nothing to reconcile them with. The
  field is additive and `report.json` stays `schema_version: "2.0"` — that major
  is itself unreleased, so no consumer has ever seen a 2.0 without it.
- **A `cfg_attr` that applies a `cfg` is part of the guard.** The guard filter
  recognized only the literal `#[cfg(` spelling, so
  `#[cfg_attr(feature = "a", cfg(unix))]` — which gates the item exactly as a
  `cfg` does — was dropped from BOTH sides' identity: the declaration text then
  paired, and a symbol that really left one configuration produced no finding at
  all. `cfg_attr` now joins the conjunction when it applies a `cfg`, and only
  then: `#[cfg_attr(unix, derive(Debug))]` decides an attribute on the item, not
  the item, and a gate invented there would split an ordinary re-add into a
  phantom removal.
- **A trailing `//` no longer swallows the rest of a declaration.** Continuation
  lines are joined with a space, and the joined text was then scanned as one
  piece — so a comment on any continuation line commented out every line
  appended after it. `declaration_complete` never saw the closing `)` or the
  body `{`, the accumulator ran on into the body, and a body-only rewrite of a
  commented multi-line signature was reported as a `ChangedSignature` that never
  happened. Completeness is now decided on a separate view of the same lines,
  read one physical line at a time, which ends a `//` where it really ends while
  still carrying an open literal or `/* … */` across the lines.
- **Every risky-pattern needle is word-bounded, not just the plain words.**
  Bounded matching was applied only to needles made entirely of identifier
  characters, so `todo!(`, `dbg!(`, `println!(`, `console.log(`, `unsafe {` and
  `as any` kept raw substring matching: `mytodo!(…)` was reported as a TODO
  marker and every `has any` in a doc comment as a type cast — the exact
  substring false positives bounded matching exists to exclude. Each side of a
  needle is now bounded where the needle itself has an identifier edge, which
  leaves `.unwrap()` matching `value.unwrap()` and `eslint-disable` matching
  `eslint-disable-next-line`. `eprintln!`/`eprint!` are now listed explicitly:
  they used to be caught only because `eprintln!(` contains `println!(`.
- **A test marker on a body-less item no longer mutes the rest of its hunk.**
  The performance-regression tracker closed a test context only when its opening
  brace balanced again, but `#[cfg(test)] mod tests;` and `#[cfg(test)] use
  crate::helper;` never open one. The context stayed active for the remainder of
  the hunk, so production loops and queries added below such a declaration were
  recorded as test-only and disappeared from the signal. A context opened over an
  item that ends at its `;` now closes there.
- **A long signature change is no longer swallowed by the accumulation cap.**
  Declaration text stopped accumulating after eight continuation lines, which
  cuts inside the real distribution of `pub` signatures: two long declarations
  that agree on their opener and those eight lines finalized to the SAME
  truncated text, so the exact-match pass paired them as an unchanged re-add and
  a parameter, bound or return type changed on the ninth line or later produced
  no finding at all. The bound is now 32 lines and is documented as what it is —
  a runaway valve for static bodies and generated data tables, not a display
  width.
- **A `cfg` predicate wrapped across lines still guards its declaration.** The
  breaking-change pairing recorded only the opener of `#[cfg(any(`, and the first
  continuation line then looked like a new item and cleared the guard: both sides
  of the diff came out unguarded, so a `pub` item that really disappeared for one
  configuration paired with its re-add under a different one and left no finding
  at all — the exact false negative the guard was added to prevent. Attributes
  are now accumulated to their balanced close, which also makes a wrapped
  predicate compare equal to its single-line spelling, and a wrapped
  `#[derive(…)]` no longer takes the `cfg` above it down with it.
- **One verdict vocabulary now answers for every reader surface.** The CLI
  matched a stored verdict case-sensitively while the MCP adapter ranked it
  through an uppercase fold, so a pack stating `verdict: "pass"` was a clean
  `PASS` to MCP automation and an unknown verdict normalized to `BLOCK` on the
  CLI — the same artifact approved by one reader and rejected by the other, which
  is the divergence the shared reconciliation exists to prevent. `APPROVE`
  diverged identically, case aside. A third surface was worse: `prview gate`
  compared the folded summary verdict against the pack's RAW string, so any
  legacy or non-canonical spelling (`ALLOW`, `HOLD`, `pass`) failed loud as a
  "gate verdict mismatch" on a pack both other readers accept. The vocabulary
  moved into `gate::canonical_verdict` and all three surfaces fold through it;
  `rank_from_verdict` is now derived from it, so ranking and folding cannot drift
  apart. `GateVerdict` stays a strict parser of canonical spellings and is fed
  the folded value.
- **Raw C string literals are read as raw strings.** The diff scanner accepted
  the `r` and `br` raw prefixes but not `cr` (Rust 1.77), so `cr#"…"#` was not
  recognized as an opener: the prefix leaked into the code text and the first
  interior `"` opened a phantom ordinary string, leaving every brace in the
  literal's body to be counted as syntax — the same failure as an untracked
  multi-line literal, which pops a `mod` scope early and can cancel a real API
  removal. Unlike the raw forms, `b"…"` and `c"…"` escape exactly like an
  ordinary string and were already blanked correctly. The construct is real
  outside this tree: 38 `cr#"…"#` sites across 11 crates in a 2025-crate
  crates.io sample, including `syn` and `proc-macro2`.
- **String literals are tracked across lines, like block comments already were.**
  The diff scanner blanked a literal only on the line that opened it, so the tail
  of a multi-line template or JSON fixture reached the delimiter trackers as
  code: its closing `"` read as an OPENER and the `}` in front of it as syntax.
  That popped `mod a` one level early, left a removed `a::Config` with an unknown
  scope, and an unknown scope pairs with anything — so an unrelated `b::Config`
  addition cancelled a real API removal. The construct is not exotic here: 241
  multi-line literals live in this tree and 168 carry a brace in their body, and
  replaying the last 201 commits shows 29 hunk sides whose brace counting this
  corrects (21 of them in the scope-popped-early direction, the one that HIDES a
  breaking change). The scanner now carries an open normal or raw literal, with
  the raw delimiter's own hash count, and forgets it at the same hunk boundary
  where it forgets an open comment. The residual cost of carrying is a hunk that
  STARTS mid-literal, measured at 1 in 872 over the same history, and it cannot
  outlive the hunk.
- **The `cfg` guard of a declaration is the whole stack of attributes above it.**
  Stacked `#[cfg(…)]` attributes are Rust's `AND`, but only the last one was
  recorded, so `#[cfg(unix)] #[cfg(feature = "x")] pub struct Config;` replaced
  by the same struct under `#[cfg(windows)] #[cfg(feature = "x")]` compared equal
  on the shared feature alone: the removal paired with the re-add and the API
  that disappeared for Unix builds was never reported. The guard is now the
  complete conjunction, sorted — reordering two attributes gates the item
  identically and is not an API change.
- **Contradictory decision signals are reconciled by conservativeness, not by
  field order.** A gate stating `verdict: "BLOCK"` beside
  `merge_recommendation: "approve"` is correctly typed and in vocabulary, so
  none of the unreadable/unknown guards fired and the CLI simply believed each
  field in turn — publishing a `BLOCK` verdict next to an `Approve`
  recommendation and, because `compute_exit_code` keys off the recommendation,
  exiting `0` on a gate whose own canonical artifact said BLOCK. Both readers now
  rank every stated axis through the shared `gate::rank_from_verdict` /
  `gate::rank_from_merge_rec` (1 = pass, 2 = hold, 3 = block), publish all axes
  from the highest rank, and name the contradiction with a `core_inconsistency:`
  caveat. `allow_merge: true` beside `review_required` no longer buys a `PASS`
  either, which is the `allow_merge == (verdict == "PASS")` invariant holding on
  contradictory packs too. A recommendation outside the vocabulary cannot rank,
  so it is excluded and named with `unknown_merge_recommendation:` — the caveat
  the MCP surface already emitted and the CLI did not.
- **A gate whose root is not a JSON object is corrupt on both readers.** The
  legacy tolerance says WHERE a schema-less pack's decision sits, not that
  anything parseable counts as one. A `MERGE_GATE.json` holding an array, a
  scalar or `null` was read by the CLI as a decision with every signal missing,
  which normalized to `BLOCK` and returned a successful summary — for an artifact
  the MCP reader rejected as `storage_corrupt`. Both now fail loud (`exit 3` /
  `storage_corrupt`) with a message that names the actual defect.
- **`--ci --fail-on-warnings` counts the warnings it promised to count.** The
  flag read `Report.checks` — the list the CLI itself executed — while the
  artifact run appends `public_api_diff`, `unsafe_audit`, `ghost_refs` and the
  synthetic `heuristics_loctree` to the list `MERGE_GATE.json` is built from, and
  none of those ever returns to the CLI. A run whose only warning came from one
  of them exited `0` under a flag that promises to fail when any check warns. The
  exit now keys off the pack's canonical `checks[]`, and the `--json` summary
  states both numbers: `checks_summary.warned` (what the CLI ran) and the new
  additive `checks_summary.warned_in_pack` (the complete count), which is never
  smaller. A pack with no readable `checks` array falls back to the CLI tally and
  says so with an `unreadable_checks:` caveat.
- A warning is no longer reported as a failed quality check. A baseline-signal
  check that reports `Warnings` (cargo-audit raising an unmaintained-crate
  advisory, `rustfmt`, `eslint`, `ruff`, `prettier`, `stylelint`, `semgrep`) is
  admitted to the quality summary so the pre-existing downgrade can be computed
  for it — but when it produced no locatable finding it classified as
  `unclassified`, which flipped `quality_pass` to `false` and printed
  "N quality checks failed" for output that never contained a failure. Warning
  entries now carry their origin and are excluded from the failure gate whatever
  they classify as: `quality_pass` stays `true`, `decision.analysis_status` stays
  `complete` instead of being degraded, the dashboard hero reads
  `ALLOW WITH REVIEW` instead of `HOLD`, and the gate reason gets a separate
  honest sentence (`2 warning signals: 1 pre-existing, 1 introduced`). Real
  failures (`Failed`/`Error`) are unchanged and still fail closed on
  `introduced`, `mixed`, and `unclassified`. The origin is now stated on the
  wire: `decision.quality_failure_details[]` carries `origin`
  (`"failure"` / `"warning"`), which is what lets a reader make sense of
  `introduced_quality_failures: ["Rustfmt"]` sitting next to
  `quality_pass: true`. This is an additive field, so `MERGE_GATE.json` is
  `schema_version: "2.2"` and `tools/validate_merge_gate.py` accepts it — and,
  from 2.2, requires it: an entry that omits `origin`, mistypes it, or spells it
  anything other than `failure` / `warning` now fails the contract validator,
  because a consumer told to filter on `origin == "failure"` cannot do that on a
  pack where the field is optional. The validator checks the whole entry, not
  only the field that names the schema: `name` must be a non-empty string and
  `classification` one of `introduced` / `pre-existing` / `mixed` /
  `unclassified`, the vocabulary `QualityFailureClass::as_str` emits. Validating
  `origin` alone let `{"origin": "failure"}` — a failure naming no check and
  stating no provenance — pass its own contract gate, and let `classification`
  drift to any string at all, including the `preexisting` spelling used by the
  sibling count field rather than the `pre-existing` the emitter writes.
- Perf regression detection now resolves inline Rust test context (`#[cfg(test)]`,
  `mod tests`, `#[test]`) **per hit line** instead of per hunk. A production hot
  path that merely shared a hunk with a test module was classified as
  `test_context_only` and silently dropped from the reviewer-facing signal
  (`perf_regression_suspected` and the risk score both ignore test-only
  suspects). Test context now opens at its marker and closes when the braces
  opened after it balance out, commented-out markers no longer open it, and any
  ambiguity resolves toward production — a false positive costs a reviewer a
  glance, a false negative hides a real regression. The scope is read from the
  patch's **target state** only: a `#[cfg(test)]` that the patch *deletes* no
  longer opens test context over the added production code, and a renamed test
  function no longer leaves the scope permanently open (its removed and added
  declaration lines each contributed an opening brace while sharing one closing
  brace). A hit is now also paired only with a nearby loop in the *same*
  context, so a production statement cannot borrow a loop from an adjacent test
  module — or the reverse. Trailing comments are stripped before both the marker
  match and the brace tracking, so `let x = 1; // #[cfg(test)]` no longer opens
  test context and a `{` inside a comment no longer shifts the scope; a `//`
  inside a string literal is still code. String and char literals are blanked
  for the same reason: `const CLOSE: &str = "}"` in a test module used to close
  the scope early and report every later test hit as production, and an
  unmatched `{` in a literal held it open and muted real production hits.
  Normal, raw (`r#"…"#`) and byte-string literals as well as char literals
  (`'}'`, `'\u{7b}'`) are recognised; lifetimes are not mistaken for char
  literals. Block comments count too, and they are tracked ACROSS lines —
  commenting a block of code out is exactly how an unbalanced brace ends up
  inside a comment, and a `/* … } … */` spread over three lines closed the test
  scope early (or, with a `{`, held it open and muted real production hits). A
  `/*` inside a string literal stays data: `format!("{}/*.{}", dir, ext)` is a
  glob pattern, and reading it as a comment opener would swallow the rest of the
  hunk — a far more common line in real diffs than a block comment is. A *string*
  literal spanning several diff lines is carried the same way a block comment is:
  the scanner keeps one open across lines, so a brace inside a multi-line
  template or JSON fixture never reaches a delimiter tracker as syntax. What ends
  the carrying is the hunk boundary, where the text stops being contiguous.
- Breaking-change detection pairs duplicate declarations one-to-one. `cfg`-gated
  variants share (file, kind, name), and the pairing search never consumed its
  match, so every removal cancelled against the same unchanged re-add: the
  addition that actually replaced one of them stayed unpaired and its signature
  change was never reported, while a genuine removal could be cancelled by an
  addition already spent on another. Exact matches are now claimed first, each
  addition is consumed once, and one cancelled removal retires exactly one
  finding.
- Breaking-change detection no longer loses a removal to a same-named symbol in
  another inline module. `pub mod a { pub struct Config }` deleted while
  `pub mod b { pub struct Config }` is added in the same file was cancelled as a
  no-op remove+re-add; the pairing now also requires compatible inline-module
  scopes (tracked per diff side, hunk-local — an unseen `mod` opener leaves the
  scope unknown and pairs as before). That module tracker now reads code only:
  a brace inside a comment or a string/char literal (`// }`, `"{"`, `'}'`) used
  to open or close a module scope that does not exist, so a removal and its
  unrelated same-named addition landed in the same phantom scope and cancelled
  each other — the breaking change vanished from the report. The literal/comment
  scanner is shared with perf-regression test-context tracking (`rust_source`),
  so both brace trackers agree on what counts as syntax, block comments spanning
  lines included.
- Breaking-change detection no longer cancels a removal against a re-add under a
  DIFFERENT `cfg`. `#[cfg(feature = "a")] pub struct Config;` replaced by the
  same struct under feature `b` is an exact text match, so the pairing dropped
  the removal — but `Config` really did disappear for anyone building with
  feature `a`. The guard standing above a declaration is now part of its pairing
  identity (whitespace-insensitive, so a reformatted attribute is not a
  different predicate). A guard the diff never showed on one side stays unknown
  and pairs as before, the same tolerance an unseen `mod` opener gets: the
  attribute often sits on a context line, and reading "not shown" as "no cfg"
  would turn ordinary re-adds into phantom removals.
- A public declaration no longer ends at a delimiter inside its own literal.
  `pub const TEMPLATE: &str = r#"{` opens a multi-line raw string, and reading
  that `{` as the declaration's body opener finalized a truncated declaration —
  identical on both diff sides, so the removal was cancelled and the literal
  change the patch actually made produced no finding at all. Completion is now
  judged on code only, and the accumulated text is scanned as a whole, so a
  literal spanning continuation lines closes the declaration where it really
  ends.
- Multi-line public declarations are compared in full. `pub struct Config<` with
  a changed bound on the next line used to hide behind its identical opening
  line, because only that line was compared. Continuation lines are now
  accumulated on both diff sides — for every symbol kind, not just `pub fn` —
  up to 8 lines, and `BREAKING_CHANGES.md` shows the full declaration.
- `BREAKING_CHANGES.md` no longer collapses two different symbol kinds into one
  row. Changed signatures were grouped by (file, name); now that non-fn
  declarations also produce signature changes, a `pub struct Limit` and a
  `pub const Limit` in one file were rendered as one row plus a bogus
  "feature-gated variant" note. The grouping key now carries the symbol kind.
- The pattern scan no longer reports an identifier as a TODO marker. Word
  boundaries were read byte-wise over ASCII only, so `$` — an identifier
  character in JavaScript/TypeScript and the macro metavariable sigil in Rust —
  and every non-ASCII letter counted as a boundary: `const $TODO = false` and an
  identifier abutting a Unicode letter were both reported, inflating `prod_hits`
  and the risk score with exactly the false positives bounded matching exists to
  exclude. Boundaries are now read per character over the union of identifier
  characters the scanned languages accept.
- A skipped `semgrep` run keeps its diagnostic. The tool/config-error skip reason
  was built from stderr alone, but under `--json` semgrep reports rule and config
  failures in the stdout payload's `errors[]` and can leave stderr empty — so the
  one explanation available was discarded and the policy engine received the bare
  "semgrep exited 2 with no findings payload" sentence. The excerpt is now taken
  from stderr, else the payload's `errors[]` (reading `message` / `long_msg` /
  `short_msg` / `type`, whichever the semgrep version emits), else raw stdout, so
  a crash traceback printed on stdout also survives.
- `report.json` distinguishes a disabled heuristics run from a broken scanner.
  `--quick` and `--no-heuristics` short-circuit the scan to a default result
  that the caller still passes on, so the report described the intentional skip
  as `skip_reason: "loctree analysis unavailable"` — a tool failure that never
  happened — and pointed `log_path` at a zero-filled stub, while the
  `"heuristics not run"` reason was unreachable from the production path. A run
  that never asked for heuristics now reads `heuristics not run` and omits both
  `total_files` and `log_path`. No field changed shape, so `report.json` stays
  `schema_version: "2.0"`.
- Coverage no longer reports an unmeasured scan as perfect. A diff with zero
  changed source files produced `0/0 (100%)` in `AI_INDEX.md`,
  `coverage-delta.txt`, and the dashboard; it now reads `not measured`, and the
  coverage card/chip/section is omitted instead of showing a fabricated 100%.
  A real `0/N` (N > 0) is still a genuine `0%` measurement.
- `report.json` no longer zero-fills skipped analysis. `quality.heuristics` now
  carries `status` (`"measured"` / `"skipped"`), an optional `skip_reason`, and
  `total_files`; a loctree run that scanned no files (or never ran) omits
  `dead_exports`, `cycles`, `twins`, and `unused_symbols` instead of emitting
  zeros indistinguishable from a clean scan. This matches the SKIP semantics
  `MERGE_GATE.json` and `heuristics_loctree.result.json` already used.
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
- The status digest now fingerprints what a dirty **symlink reaches**, not only
  the path it names. The link's own identity is still the target path — a link
  retargeted at identical bytes is a different tree — but everything the checks
  read through it lives at the far end, and hashing the pathname alone let all
  of it change between two runs under one unchanged digest. The resolved file is
  hashed, a directory is recorded without being descended into (an absolute link
  can leave the repo), a dangling link reads `absent`, and a device or fifo is
  never opened.
- The status digest's reading is now **bounded**. It is taken before the first
  check starts, and `recurse_untracked_dirs` means an untracked dataset, model
  checkpoint or vendored bundle in the dirty subset was hashed whole — gigabytes
  of reading in front of a review nobody had started yet. One capture may now
  hash 256 MiB in total (measured at ~1 s in a release build), shared across
  every entry and every nested repository it descends into; a file that does not
  fit what is left is described as `stat:<len>:<mtime>` rather than read. That is
  deliberately a different word from `blob:` and not a content hash: two runs
  where an oversized file changed while keeping both its size and its mtime do
  collide, a far narrower window than a constant "too big" marker, which would
  have made every large file equal to every other. A refused read leaves the
  allowance intact, so the entries after a huge one are still hashed, and entries
  are ordered before any content is read, so the digest of an unchanged tree does
  not depend on the order git reports them in. Ordinary review-sized dirt is
  nowhere near the bound, so existing digests are unchanged. A fifo, socket or
  device node in the dirty subset is also no longer opened at all — a reader with
  no writer blocked the run forever.
- Cargo geiger's *self-handled* skips now carry their substrate. The error
  fallback above only covers checks that return an error, so geiger's two
  internal skips — the ten-minute timeout degraded to `Skipped` rather than a
  gate error, and the virtual-workspace pre-flight — slipped past it and wrote
  null `cwd`, `target_sha` and `tree_state`. In both cases a cargo command had
  already read the reviewed tree, so the rows are now built from the directory
  it ran in, with `exit_code: null` naming the single thing that is genuinely
  unknown. A null substrate now means what it says: nothing was read.
- Cargo geiger's virtual-workspace pre-flight now fires. It tested a
  `root_package` key that `cargo metadata --format-version 1` does not emit, so
  it was always false and every virtual workspace paid a full geiger scan
  (minutes) before cargo refused the manifest. The probe now asks whether the
  manifest in the directory geiger would run in appears among the workspace's
  packages, which leaves a member directory — a concrete package inside a
  virtual workspace — scanned as before.
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
  than guessing which crate the review is about. That single candidate must also
  prove it *is* the configured project — matching `[package] name`, or the member
  list for a virtual workspace root that names no crate. Being the last manifest
  standing is not evidence of having moved: a commit that deletes the Rust
  project while keeping an `examples/demo` crate within reach had every cargo
  gate run against the demo and file its green verdict for a project the commit
  no longer contains, one that profile detection would not even call a Rust
  project locally. Nothing to compare against skips with a reason too.
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
  already handles ageing advisories. A lockfile that is *present but out of
  date* is not a pin either: a target that adds a dependency without
  regenerating `Cargo.lock` still sends cargo to the registry, since no cargo
  command here passes `--locked`. The manifest's declared dependencies are now
  checked against the lock's package list (renames followed) *and* against the
  versions it pins — a `serde = "1"` bumped to `"2"` over a lock still holding
  1.x is as unresolved as a dependency the lock never heard of — so a lock the
  manifest has outgrown carries the same day stamp. The
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
  manifest alongside the directory for the cases the tree lookup cannot cover —
  and `Cargo.lock` with them, because cargo follows a symlinked lockfile even
  under `--locked`, so a reviewed commit tracking its lock as a link to an
  external file had its entire dependency graph resolved from another project's
  pins.
  A **local** review is one of those cases and was reached by neither guard —
  the local plan returns before the containment check runs — so a checkout
  tracking `Cargo.toml` as a link to an external manifest had cargo build a
  foreign project while provenance recorded the local checkout. The manifest is
  now resolved against the cargo root before a local plan is returned; an
  externally configured `cargo_root` whose own manifest sits inside it is still
  a legitimate local setup and is unaffected. A contained manifest can still
  *declare* its way out: an absolute `path` dependency — or a relative one that
  climbs out or passes through a symlink — had cargo compile source the reviewed
  commit does not contain, under that commit's cache key and a `snapshot`
  provenance row. Every local path an off-`HEAD` run's cargo root manifest names
  (dependencies, dev, build, `[workspace.dependencies]`, `[target.*]`, `[patch]`,
  `[replace]`) is now resolved against the snapshot, and one that leaves it is
  refused with the dependency named — and not only the root manifest's, since
  `cargo check` at a workspace root builds its members: every manifest within
  three levels of the cargo root is read the same way, through a bounded walk
  that never enters a symlinked directory and skips `target/` and `.git/`. A
  member manifest that is itself a link out of the snapshot is refused with
  them. Local reviews are untouched: a path dependency on a sibling checkout is
  an ordinary local setup, and a local run claims nothing about a commit's
  contents.
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

### Security

- bump ammonia 4.1.3 → 4.1.4 (RUSTSEC-2026-0213: XSS via SVG `animate`/`set` attributes)

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
- build(deps): bump loctree 0.8 → 0.13.0 — source-compatible; stale 0.8-era caches are rescanned automatically via the schema gate, and the wider file-type scan coverage broadens the `LOCTREE` heuristics totals (`total_files`, `total_loc`, `by_language`) for the same tree

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

[Unreleased]: https://github.com/vetcoders/prview-rs/compare/v0.8.0...HEAD
[0.8.0]: https://github.com/vetcoders/prview-rs/compare/v0.7.0...v0.8.0
[0.7.0]: https://github.com/vetcoders/prview-rs/compare/v0.6.0...v0.7.0
[0.6.0]: https://github.com/vetcoders/prview-rs/compare/v0.5.0...v0.6.0
[0.5.0]: https://github.com/vetcoders/prview-rs/compare/v0.4.0...v0.5.0
[0.4.0]: https://github.com/vetcoders/prview-rs/releases/tag/v0.4.0
