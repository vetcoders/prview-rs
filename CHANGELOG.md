# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

> Entries prior to 0.4.0 document development that predates this repository's
> public debut. 0.4.0 is the first public release, so only versions from 0.4.0
> onward have git tags and comparison links.

## [Unreleased]

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

[Unreleased]: https://github.com/vetcoders/prview-rs/compare/v0.7.0...HEAD
[0.7.0]: https://github.com/vetcoders/prview-rs/compare/v0.6.0...v0.7.0
[0.6.0]: https://github.com/vetcoders/prview-rs/compare/v0.5.0...v0.6.0
[0.5.0]: https://github.com/vetcoders/prview-rs/compare/v0.4.0...v0.5.0
[0.4.0]: https://github.com/vetcoders/prview-rs/releases/tag/v0.4.0
