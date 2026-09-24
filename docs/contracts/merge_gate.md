# MERGE_GATE Contract (schema 3.1)

`MERGE_GATE.json` is the policy-aware merge decision emitted at
`00_summary/MERGE_GATE.json`. It is the single machine-readable verdict surface
consumed by AI reviewers, CI, and the `prview mcp` adapter. This document is
derived from the emitter `src/artifacts/merge_gate.rs` and the decision engine
`src/policy/engine.rs` + `src/artifacts/verdict.rs`. When the code and this
document disagree, the code is the contract and this document is the bug.

## Top-level object

| Field | Type | Notes |
|---|---|---|
| `schema_version` | string | `"3.1"` |
| `generated_at` | string | RFC 3339 local datetime |
| `bridge_stage` | integer | `0..4` |
| `target` | string | Resolved target branch name (not raw CLI input) |
| `bases` | string[] | Resolved base branch names |
| `profile` | string | Resolved profile kind |
| `policy` | object | `{ version, mode, default_severity, source, origin }` |
| `checks` | object[] | Per-check evaluation records (see below) |
| `inline_findings` | object | Inline SARIF summary (see below) |
| `stale_cache_caveats` | object[] | Advisory, additive: gate rows replayed from an old or age-unverifiable cache (see below) |
| `provenance_contradictions` | object[] | Advisory, additive: substrate statements the run and a check make that cannot both be true (see below) |
| `decision` | object | The merge decision (see below) |
| `files` | object | Artifact-root-relative paths (see below) |
| `rust_api_delta` | object \| null | Additive lossless Rust API delta; `null` on non-Rust runs (see below) |

### `policy`

| Field | Type | Notes |
|---|---|---|
| `version` | integer | Policy document version |
| `mode` | string | Policy mode (e.g. `shadow` / `warn` / `block`) |
| `default_severity` | string | `block` \| `warn` \| `ignore` |
| `source` | string \| null | Path actually read at policy load, or `null` for built-in defaults |
| `origin` | string | `file` \| `builtin-default`; consistent with `source` |

Policy provenance is captured when configuration is loaded, before checks.
Creating or removing a policy file later does not change this record.
`--policy-mode` overrides the effective mode without changing the policy's
origin. An absent requested policy retains the existing built-in fallback.

Migration from 2.x: readers must accept nullable `source` and use `origin` to
distinguish built-in defaults from a file. Older records carried the resolved
candidate path even when no file was read; that path is not proof of a loaded
policy. The validator retains the older string contract for 1.x/2.x records.

### `files`

Paths are relative to the artifact root, using the subdirectory pack layout:

| Field | Value |
|---|---|
| `merge_gate_json` | `"00_summary/MERGE_GATE.json"` |
| `inline_findings` | `"30_context/INLINE_FINDINGS.sarif"` when emitted, otherwise `null` |
| `full_patch` | `"10_diff/full.patch"` |
| `checks_log` | `"20_quality/full-checks.log"` |
| `dashboard` | `"dashboard.html"`, or `"review.html"` when the run was started with `--no-dashboard` |

`files.dashboard` names the pack's single browser entry point, not a fixed
filename: a `--no-dashboard` run writes `review.html` instead of
`dashboard.html`, and the field follows it. Every value in `files` points at a
file that exists in the pack (`inline_findings` is `null` rather than naming an
absent SARIF). A consumer that hard-codes `"dashboard.html"` was already
following a dead link on static-report runs.

`MERGE_GATE.md` is also written beside the JSON, but it is a human-readable
companion and is not listed in `files`.

## `checks[]` — check item schema

Every element of `checks` is one policy evaluation record:

| Field | Type | Notes |
|---|---|---|
| `id` | string | Policy check id (`check_id`) |
| `name` | string | Human-readable check name |
| `status` | string | `passed` \| `failed` \| `warnings` \| `skipped` \| `error` — lowercase, exactly |
| `execution_state` | string | `executed` \| `skipped` \| `unavailable` \| `unknown` |
| `outcome` | string | `passed` \| `findings_failed` \| `findings_warning` \| `system_error` \| `skipped` \| `unavailable` \| `unknown` |
| `class` | string | `PASS` \| `SKIP` \| `FAIL` \| `INFO` |
| `severity` | string | `block` \| `warn` \| `ignore` |
| `policy_conclusion` | string | `satisfied` \| `advisory` \| `blocked` |
| `confidence_impact` | string | `complete` \| `degraded` \| `incomplete` |
| `merge_impact` | string | `approve` \| `review_required` \| `block` |
| `blocking` | boolean | True iff `merge_impact == block` |
| `duration_secs` | number | Non-negative; `0.0` for a check with no executed result |
| `cached` | boolean \| null | `null` for a check with no executed result |
| `reason` | string \| null | Policy reason, when present |
| `evidence` | string | `20_quality/<artifact_id>.result.json` for an executed check; otherwise the reason text or `"skipped — no artifact generated"` |
| `log` | string \| null | `20_quality/<artifact_id>.log` for an executed check, else `null` |
| `scope` | object \| null | Additive (3.1). How much of this check's test suite the run decided had to execute, and why. Valid ONLY on the checks that own an ecosystem's test scope (`Cargo test`, `Vitest`); the validator rejects it on any other row, because test-scope evidence pinned to a check that runs no tests would read as a narrowed lint (see below) |

### `scope` (schema 3.1, additive)

Review must be proportional to the change, not to the size of the repository —
but a narrower run is only acceptable if the artifact says so. `scope` is that
statement.

| Field | Type | Notes |
|---|---|---|
| `mode` | string | `full` \| `change-scoped` — a CLOSED vocabulary |
| `reason` | string | Non-empty. For `full` it is the escalation reason (`escalated_by`), naming the specific fact that widened the run |
| `inputs` | integer \| null | Changed paths the decision took into account, escalation or not. `null` means ONLY "there was never a set to count" — it is not a synonym for `0` |
| `selected` | integer \| null | Units selected, counted in each ecosystem's own unit: **Cargo — workspace packages** named by `-p`; **Vitest — test files the reporter collected**, which is what `vitest related` resolved the changed sources into, not the sources themselves. A narrowed run whose reporter could not be read publishes `0` beside its selector, because nothing was proven collected. MUST be `null` when `mode` is `full`: a full run selected nothing, and a count there would read as coverage |
| `universe` | integer \| null | The whole population the selection was drawn from, when knowable before the run |
| `selector` | string \| null | The fragment of the command line that narrowed the run, VERBATIM: `-p <pkg> [-p <pkg>…]` for Cargo, the `related …` argument line up to and including the selector inputs for Vitest (the reporter flags that follow it say how the run was OBSERVED, not what it selected, and stay out). It is a substring of `command` in the check's `20_quality/<id>.result.json`. MUST be `null` when `mode` is `full`, and is `null` for an empty selection, which spawned no command at all |
| `non_participating` | object[] | Additive, omitted when empty. Changed paths classified as NOT inputs to test selection, each as `{ path, rule }`. Both fields are required and non-empty: an entry that does not name its rule cannot be challenged, which is the whole point of publishing the list |

A pack must never state `change-scoped` for a run that executed the full
command. `mode` is a claim about a command that ran, not about a decision that
was taken: it is published only against execution evidence the check itself
left, so a scopeable decision from a check that did not narrow is reported as
`full` with reason `scoped execution not confirmed by the check`, and a run that
escalated at execution time reports `full` with the runtime reason. For the same
reason a row for a check that never ran carries no `scope` at all — there is no
command to describe.

**An empty selection is a skip, never a pass.** `mode: "change-scoped"`,
`selected: 0`, `selector: null`, on a row whose `status` is `skipped` and whose
`outcome` is `skipped`, with `no tests related to the change` as the reason.
This shape too is published only against the check's own evidence: the empty
selection is recorded as `executed_scope: {"mode": "nothing-selected"}` beside a
`command` of `<no command recorded>` in `20_quality/<gate>.result.json`. A
skipped row without that record is reported as `full` with
`scoped execution not confirmed by the check`, like any other unproven claim.
That row does not block: the suite applies to the repository but not to this
change, and the classification that proved it escalates anything it cannot name.
It is never relabelled `passed` — no suite ran.

A narrowed run can reach the same reason a second way: the selection was
non-empty, but the runner found no test related to it. For Vitest that verdict
comes from its JSON reporter (zero suites, no collected file) rather than from
anything printed in the log, and a narrowed run whose report cannot be read is
an `error` — `could not verify that the narrowed Vitest run executed any test` —
so an unverified run is never green and never a skip either. Such a row keeps
its `selector`: a command did run, and the pack says which one.

**Caveats.** Every narrowed or skipped test run owes `decision.review_caveats`
one advisory line, derived from the very `scope` objects published on the rows,
so a caveat cannot describe a narrowing the rows do not show and a narrowed row
cannot reach a reviewer uncaveated. Caveats are advisory: scope never moves a
verdict.

`inputs`, `selected` and `universe` are counts of files and packages, so they
are integers or `null`; a fractional count is rejected outright, because "1.5 of
1028 test files" is not a statement a reviewer can act on.

`non_participating` records a decision about TEST SELECTION ONLY. A path listed
there is still in the diff, the artifacts, the signals and the verdict; it simply
did not help decide which tests had to run. The same list is rendered as a
`## Test scope` table in `MERGE_GATE.md`.

Reading rule: absence means "this check has no test suite to scope, or this pack
predates 3.1" — never "the full suite ran". Only a stated `mode` is evidence.

Semgrep disabled by `--skip-security` uses the shared `security disabled` mode
reason, with `execution_state: skipped` and `outcome: skipped`. At `block`
severity its policy conclusion is `advisory`, confidence is `incomplete`, and
merge impact is `review_required`; `blocking` is false. An unavailable required
scanner without an explicit opt-out retains its blocking policy outcome.

Skipped or unavailable checks carry no executed `CheckResult`, so `duration_secs`
is `0.0`, `cached` is `null`, `log` is `null`, and `evidence` degrades to a
non-empty placeholder. These are contract-valid placeholders, never `null`
evidence — the artifact must not fail its own gate on a runner that lacks a tool.

`status` is a CLOSED, case-sensitive vocabulary: exactly the image of
`CheckStatus::as_str`, pinned as `CheckStatus::EMITTED` in `src/checks/mod.rs`
and mirrored as `VALID_CHECK_STATUSES` in `tools/validate_merge_gate.py`. The
CLI tallies warnings by comparing against it, so a status outside it is
UNREADABLE rather than clean: it counts toward the warning tally, raises an
`unreadable_check_status:` caveat naming the offending checks, and
`--ci --fail-on-warnings` fails on it. The validator rejects such a pack outright
— it used to accept any non-empty string, which certified an artifact
`--update` could reuse and the reader could not read. Case is deliberately NOT
folded here, unlike `inline_findings.status`, whose writer has shipped legacy
spellings: folding `WARNINGS` into a warning silently would hide that the pack
is off-contract, and the resulting tally is the same either way.

The same rule governs the CONTAINER. `checks` must be an array — the validator
has required one since schema 1.0 — and a pack stating anything else is
unreadable, not empty: it counts as at least one warning and raises an
`unreadable_checks:` caveat. It used to fall back to "the checks this run
executed", which on an unchanged `--update` run is none at all, so
`--ci --fail-on-warnings` exited `0` on a pack whose warning list the reader
could not read. Actual absence is tolerated only for a schema at or below 2.2,
where the pack may predate typed enforcement. Schema 2.3 requires `checks` and
`inline_findings`; absent, non-container, unknown-status, or mistyped states
raise a reader caveat and the `review_required` disposition. They still count as
at least one warning for the explicit warnings-clean lane, but can never prove
the harmless warnings-only exception.

## `inline_findings`

| Field | Type | Notes |
|---|---|---|
| `file` | string \| null | `"30_context/INLINE_FINDINGS.sarif"` when `findings_count > 0`, else `null` |
| `file_exists` | boolean | `findings_count > 0` |
| `status` | string | Inline analysis status |
| `severity` | string | `block` \| `warn` \| `ignore` |
| `blocking` | boolean | Whether inline findings block the merge |
| `effective_class` | string | `PASS` \| `INFO` \| `FAIL` — the emitter-side class after trusted pre-existing policy |
| `enforcement_disposition` | string | Per-source `clean` \| `warnings_only` \| `review_required` \| `block` proof (required from schema 2.3) |
| `findings_count` | integer | Total inline findings |
| `introduced_count` | integer | Findings this diff introduced (`in_diff == true`) |
| `preexisting_count` | integer | Pre-existing whole-repo findings (`in_diff == false`) |

Repository-wide Loctree summaries are general context, not source-line findings.
They remain available with the check results and as dashboard notes, but do not
create a SARIF result or increase `findings_count`. Likewise, a Pytest failure
without a parsed traceback location stays a general check signal; PRView does
not borrow a path from test progress or startup output to manufacture a location.
These exclusions do not remove the original check's failed/warning status or
its policy evaluation.

Located Pytest findings preserve the error excerpt and the file and line
reported by the traceback. This is the location where failure was reported,
not proof of its cause or that the PR introduced it. Their `in_diff` is `null`
and their SARIF `properties.classification` is `unclassified`, even when the
reported test file changed. The complete Pytest log remains the evidence source;
the dashboard and narrative use a compact diagnostic excerpt instead of test
startup/progress output.

### SARIF result properties

`30_context/INLINE_FINDINGS.sarif` carries the same tri-state on every result:

| Property | Type | Notes |
|---|---|---|
| `properties.in_diff` | boolean \| null | `true` when the reported location is in a file this diff touches, `false` when it is outside, `null` when the origin was not established |
| `properties.classification` | string | `introduced` (`in_diff == true`), `preexisting` (`in_diff == false`), `unclassified` (`in_diff == null`) |

Outside Pytest, `introduced` is derived from the diff intersection alone: the
tool reported the finding in a file this diff touches. That is a location
signal, not a base-versus-target comparison, so it does not prove the change
created the finding.

`in_diff` was previously always a boolean; readers that assume that type must be
updated. `classification` gained the `unclassified` value, so a consumer that
matches on it needs a default branch. Neither `null` nor `unclassified` is a
pass: they mean the origin of the finding is unknown, and the gate treats them
like introduced findings rather than trusted pre-existing ones.

`introduced_count + preexisting_count` may be less than `findings_count`: the
split counts only operator findings with a known `in_diff` value, so the
remainder has no trusted pre-existing proof. Schema 2.3 therefore does not
reconstruct `effective_class` from those counts; it only checks whether the
stored class is possible. With `F = findings_count`, `I = introduced_count`,
and `P = preexisting_count`, the complete count model is:

| Raw status | Required count shape | Possible effective classes |
|---|---|---|
| `passed` / `not_run` | `F = 0` | `PASS` |
| `warnings` | `F > 0` | `INFO`; also `PASS` only when `I = 0` and `F = P` |
| `failed` | `F > 0` | `FAIL`; also `INFO` only when `F >= 2` and `P >= 1`; also `PASS` only when `I = 0` and `F = P` |

Every row additionally requires `I + P <= F`. The `failed`/`INFO` condition is
necessary because the raw error must be pre-existing while a distinct warning
remains effective. An all-pre-existing count does **not** prove `PASS`, because
the effective downgrade also depends on baseline/trust provenance that the
aggregate does not carry.

### What licenses a pre-existing downgrade

An all-out-of-diff failure is downgraded only for a check whose finding
*locations* are an exhaustive baseline signal, and only when that check's
findings are provably from the analysed target. For the file-scoped scanners
(`semgrep_scan`, `eslint`, `stylelint`, `ruff`, `prettier`, `rustfmt`) the
provenance question is about the tree: a dirty worktree can present an
uncommitted finding as out-of-diff, so a dirty local scan downgrades nothing,
and on a remote/snapshot target only the checks that scan the target snapshot
qualify.

`cargo_audit` is judged differently, because its findings are not source
locations. An advisory is `Cargo.lock` × the advisory database, filtered by the
audit's configuration, so no uncommitted source can move it and whole-tree
cleanliness is not evidence about it either way. Its proof has three premises,
and all must hold.

First, the target commit's tree must actually carry the `Cargo.lock` the audit
reads — the one in the cargo root itself, committed as a regular file. `cargo
audit` opens `Cargo.lock` relative to the directory it runs in and never falls
back to a workspace root's, so a root lock beside a lock-less member does not
count, and neither does a symlink committed in the lockfile's place. `cargo
audit` does not refuse a crate that has no lockfile: it resolves one from the
registry, audits that, and exits non-zero on a hit. Those advisories are real,
but they are about a file no commit contains, so nothing about them can be
"unchanged by this PR". A target with no such lockfile, or a lock question that
cannot be answered, proves nothing — in every run shape.

Second, the lockfile the audit read must be that one: true by construction when
the run scanned a target snapshot at the cargo root the first premise asked
about, and true for a local run when that lockfile carried no uncommitted change
(read from the status frozen before the checks ran). A reviewed commit that
moved its crate away from the configured cargo root (`crates/core` → `backend`)
has cargo run in the new directory, so a snapshot of it proves nothing. Unrelated
dirt in the tree no longer suppresses the downgrade; a dirty audited lockfile
does, and an unreadable status establishes nothing. The lockfile must also have
stayed that one while the checks ran: prview's cargo commands do not pass
`--locked`, so a lock the manifest has outgrown is rewritten by the first of
them before the audit reads it. A snapshot run whose check-boundary
observations (`20_quality/SNAPSHOT_INTEGRITY.*`) saw the audited lock change,
or could not be read, proves nothing (each boundary also reads an entry marked
skip-worktree or assume-unchanged on disk, for the reason below); a local run
reads the audited lock again after the checks, in the index and in the working
tree separately (a change
staged and then reverted in the working file cancels out in one combined
diff), and once more on disk against the commit itself, past the index: an
entry marked skip-worktree or assume-unchanged is reported unmodified by status
and by the index's view of the working tree however it was edited. A lock that
no longer matches the target commit in any of these proves nothing either.

Third, the configuration the audit applies must not have changed between the
base and the target. `cargo audit` reads `.cargo/audit.toml` in the directory it
runs in, without walking up, and otherwise `$CARGO_HOME/audit.toml`. That file's
`ignore` list, `informational_warnings` and `[output] deny` decide which
advisories fail. No audit reads the base's configuration: the audit runs in the
reviewed tree, and the base audit reads the base's lockfile but runs in the
repository's checkout. A change that only drops an ignored advisory therefore
fails the audit with the lockfile untouched and every finding out-of-diff.
`.cargo/audit.toml` in the cargo root must be the same committed file in the
base and the target of every diff, or the proof is withheld. A rename or
deletion counts, and so does anything a checkout could resolve differently,
such as a symlink at the path or at a parent. A configuration anywhere else, for
example the repository root's for a member cargo root, is not the file the audit
read and does not count.

The file the audit read must also be the target's. A local review audits the
checkout, so a staged, unstaged, untracked or ignored `.cargo/audit.toml` there
is the configuration `cargo audit` applied. Such a file can ignore the advisory
the change introduced while pre-existing ones still fail, and the downgrade
would then pass a change that its own committed configuration blocks. A local
review therefore withholds the proof when any of these holds:

- the status read before the checks lists the file or a parent of it, such as
  an untracked symlinked `.cargo`;
- after the checks, the tracked file differs from the target's in the index, in
  the working tree, or on disk past a skip-worktree or assume-unchanged flag;
- the target has no configuration and a file exists at the path. This is what
  catches an ignored configuration, which no status read lists.

A snapshot run audits a tree materialised from the target, so there only a
check can make the file differ. A check boundary that saw the tracked file
rewritten withholds the proof. No boundary lists an untracked or ignored file,
though, and a check's build script can generate one, so where the target has no
configuration any file at the path in the snapshot's working tree withholds the
proof too, and a snapshot tree that cannot be looked at proves nothing.

A case-insensitive filesystem reads `.cargo/audit.toml` under any spelling, so
a committed `.cargo/Audit.toml` or `.CARGO/audit.toml` is a configuration the
audit may apply that a comparison by exact path never sees; with two spellings
committed, which one the checkout leaves on disk is its own choice. A path that
matches `.cargo/audit.toml` only when case is ignored, in the target or on
either side of any diff, therefore withholds the proof on its own, and the
dirty paths above are matched ignoring case as well.

With an absolute `CARGO_HOME`, `$CARGO_HOME/audit.toml` lies outside the
reviewed tree: it is the environment's policy, applied alike to the audit and
its baseline, and no commit speaks for it. prview does not set `CARGO_HOME` for
its checks, so they inherit the operator's, and a relative one resolves against
the directory the audit ran in. That puts the fallback configuration, and the
advisory database under it, inside the scanned tree, where a change can edit
them with the lockfile untouched. A relative `CARGO_HOME` therefore withholds
the proof on its own. An empty one counts as unset, as it does for cargo-audit.

The proof licenses a downgrade; it never manufactures one. Advisories the diff
introduced stay `introduced` — warnings-category ones too (`unmaintained`,
`unsound`, `yanked`): they have no vulnerability row of their own, so each new
one reaches the classifier as a dashboard note row with `in_diff: true` (a note,
never a SARIF result or part of `findings_count`), and an audit that introduced
one is `mixed` or `introduced`, never downgraded — and a changed lock with no
base audit leaves every row `in_diff: null`, which stays `unclassified` and
keeps gating. The base audit reads the base's copy of the lockfile the audit
read; a base without one (a member lock the change added beside a root lock)
has no base audit. Every vulnerability entry and every `warnings` item the
check status counts is part of the compared set — a yanked release, which has
no advisory, under the id `yanked` — and an item that cannot be keyed, or two
items that share a key, make the report unreadable. A run with no diff baseline at all (`--current-only`, or no
resolved base differing from the target) downgrades nothing, whatever the
lockfile says.

A consequence worth stating: an advisory published *after* the base commit, on a
lockfile this change never touched, is reported as pre-existing. It is debt
newly revealed, not debt introduced — the change moved no dependency, the
advisory database moved. The `Cargo audit baseline` caveat's `status` and counts
remain the record of what was compared.

That caveat states a provenance claim, so it is qualified by the same proof the
check row is. When the lockfile proof holds it reads plainly:

- `Cargo audit baseline: status=<status>, new=N, pre-existing=M, resolved=R,
  unknown-baseline=U`

When the proof was withheld, the counts are still the right count of advisories
and the wrong thing to state as settled, so the gap is appended in the same
words the blocking line uses:

- `… unknown-baseline=U (<gap>; pre-existing=M is not shown to predate this
  change)`, or `… unknown-baseline=U (<gap>)` when `M` is zero and there is no
  claim to qualify.

The counts themselves never change — classification is the proof's job, not the
renderer's. The qualifier also disambiguates `status=not-required`, which means
"the lock did not change, so no base audit was needed" and would otherwise read,
against a target carrying no lockfile at all, as an untouched file rather than
an absent one.

Both outcomes are named in the check row's `reason`: a downgraded audit reads
`pre-existing: Cargo.lock unchanged by this PR (N advisories)` or `pre-existing:
unchanged vs base audit (N advisories)`, while a blocking one appears in
`decision.blocking_issues` as one of

- `Cargo audit (<Status>): N new advisories introduced (RUSTSEC-… in <package>
  <version>, …), M pre-existing` when the lockfile proof holds, or `Cargo audit
  (<Status>): N new advisories vs the base audit (…), M pre-existing; provenance
  proof unavailable: <gap>` when it was withheld — new against the base audit,
  but not shown to be the target's own, so not called introduced,
- `Cargo audit (<Status>): N advisories with no base comparison (baseline
  <status>)` when nothing could be compared,
- `Cargo audit (<Status>): no readable advisory report (baseline
  current-unavailable), so no advisory could be classified either way` when the
  run produced nothing parseable — the report is the missing thing, so the
  lockfile is not what the sentence is about, or
- `Cargo audit (<Status>): provenance proof unavailable: <gap> (M advisories not
  shown to predate this change)` when the counts are silent and the lockfile
  proof was withheld — `<gap>` being `no Cargo.lock in the target tree`,
  `Cargo.lock dirty or rewritten in the scanned tree`, `the reviewed commit moved the cargo
  root away from the configured one`, `the cargo-audit configuration
  (.cargo/audit.toml) changed or is dirty in the scanned tree`, `.cargo/audit.toml
  is committed under another case, which a case-insensitive checkout reads`,
  `CARGO_HOME is relative, so cargo audit read its fallback configuration and
  advisory database inside the scanned tree`, or
  `the scanned tree could not be tied to the target commit`. When `M` is zero the
  parenthetical is omitted and the gap
  stands alone: no line asserts a count it does not have.

The set the first line counts and the set it names are one set: `new` counts
every advisory in the report, `vulnerabilities` and the `warnings` categories
(`unmaintained`, `unsound`, `yanked`) alike, and each one is named with the
locked package the key carries. The noun is therefore "advisories", not
"vulnerabilities", and `N` always equals the number of names.

`blocking_issues` entries remain free-form non-empty strings; consumers that
need structure read `checks[]` and `decision.quality_failure_details[]`, not
this text.

The emitter and validator share this per-source disposition table:

| Effective inline fact | Per-source disposition |
|---|---|
| `blocking: true` | `block` |
| `effective_class: FAIL`, non-blocking | `review_required` |
| `effective_class: INFO`, or a raw `warnings` status with effective `PASS` | `warnings_only` |
| effective `PASS` with raw `passed`, `failed`, or `not_run` | `clean` |

`blocking` itself is checked against `policy.mode`, inline severity, and
effective class exactly like `PolicyConfig::is_blocking`: shadow never blocks;
warn blocks `FAIL` at block severity; block mode blocks `FAIL` at block or warn
severity. This preserves legal pre-existing errors while preventing a forged
non-blocking inline failure from hiding a policy block.

## `rust_api_delta`

For Rust projects, `rust_api_delta` is an additive lossless copy of the exact
revision-backed view used by `PUBLIC_API_DIFF.json`, `BREAKING_CHANGES.json`,
`report.json`, and decision caveats. Non-Rust runs emit `null`.

| Field | Type | Notes |
|---|---|---|
| `view` | string | `breaking_changes` in MERGE_GATE |
| `analysis_source` | string | `repo_backed_rust_api` |
| `base_revision` / `target_revision` | string | Exact Git-tree provenance; multi-base values explicitly name all revisions |
| `counts` | object | `added`, `removed`, `changed`, `relocated`, `visibility_changed`, `unknown` |
| `findings` | object[] | Stable `id`, kind, semantic identity, before/after, confidence, evidence, provenance, optional unknown reason/source |

Added-only Rust facts do not change the decision axes. Confirmed removed,
changed, relocated, and visibility-changed facts raise the merge axis to review
when `breaking_escalation` is enabled. Unknown facts degrade analysis confidence
and require review; they never masquerade as confirmed removals. Review caveats
carry the same IDs, so consumers can join directly to this structure.

`PrivateTypeDependency` findings retain the canonical guarded declaration,
alias, and impl evidence that produced them. If finite alias resolution is
exhausted, the finding remains `Unknown` with explicit finite-graph-bound
evidence; it is never serialized as Added, omitted as an empty scan, or allowed
to coexist with a clean PASS. Runtime-only phase timings are intentionally
outside this semantic view and live in `RUN.json.timings` under
`rust-api.fast-preset-unknown` or `rust-api.isolated-worker`. The former does
not launch full analysis. The latter covers one isolated worker and one
30-second total deadline across all comparisons; timeout, nonzero exit, or
malformed JSON remains review-required uncertainty.

Unknown classifier names are an additive, open vocabulary rather than a closed
client enum. `OpaqueReturnAutoTraits` identifies a caller-observable `async fn`
or return-position `impl Trait` whose unchanged signature does not prove that
its hidden return type kept the same auto traits. Its evidence may include
`opaque-implementation-digest:sha256:<digest>`. One-sided opaque proofs are not
emitted for wholly added/removed items because the corresponding Added/Removed
finding is already canonical. `MacroGeneratedItems` evidence may include
`macro-implementation-digest:sha256:<digest>` and
`declarative-implementation-digest:sha256:<digest>`. Associated and top-level
function-like macro boundaries neutralize only when the digest required for
their boundary kind is revision-backed. For both proof classes an `unresolved:*`
digest is deliberately non-neutralizable. Only the effective
product/workspace `Cargo.lock` may qualify these proofs; unrelated nested locks
do not, and a proc-macro dependency candidate must appear under its actual
package name with an external source and a locked version satisfying the
declared requirement, including registry checksum or precise Git commit.
Reachable path manifests and effective Cargo config bytes from every reachable
manifest invocation context participate in the digest. Unresolved
manifest/config Cargo source replacement, a stale same-name local lock entry,
or a tracked symlink (including an overlay
typechange to symlink) keeps the digest non-neutralizable. Additive derive
unknowns do not suppress confirmed changes to their annotated input item; they
cover only generated output.

`CfgPredicate` evidence for a custom cfg leaf may include
`cfg-authority-digest:sha256:<digest>` when an active revision-backed build
script or Cargo config can define it. The digest is deliberately conservative
and can over-report after unrelated tracked changes. With no revision-backed
authority it is `cfg-authority-digest:unresolved:*`; unresolved cfg authority is
non-neutralizable even when both sides carry identical evidence. Nested fields,
variants, trait/impl members, and foreign items follow the same rule. For each
package, Cargo config authority is merged from revision-backed files visible
from its manifest directory through in-repository ancestors. This is the
package's possible direct-invocation context; workspace-root-only invocation is
narrower. Deeper values follow Cargo precedence, and legacy `.cargo/config`
takes precedence over `.cargo/config.toml` when both exist in one directory.
Only legal build/target rustflags and a
concrete-target link-override rustc-cfg whose key matches the package's `links`
can define custom cfg, and `package.links` still requires a live build script.
Child-process environment settings, configs outside that package's ancestor
chain, lookalike keys in unrelated tables, and include-backed authority do not
become a complete proof.

## `stale_cache_caveats`

An additive, advisory list naming every gate row whose result was REPLAYED from
a stored entry older than the staleness threshold or whose age cannot be
established. Empty on a run where no such row exists.

| Field | Type | Notes |
|---|---|---|
| `check_id` | string | Policy check id, the same value as `checks[].id` |
| `check_name` | string | Human-readable check name |
| `cache_age_secs` | integer \| null | Age of the replayed entry, or null when the ledger cannot establish it |
| `age_status` | `stale` \| `unknown` | Whether the age exceeded the threshold or could not be verified |
| `threshold_secs` | integer | Current staleness threshold (7 days); exceeded when `age_status` is `stale` |

A stale failure can hold a merge, while a stale passing row can support a clean
verdict after a compiler or tool version changed without changing the current
source key. Both are evidence the current run did not produce, so both are
dated. The ledger lookup binds the caveat to an actual cached replay rather than
to the row's status text.
An unknown age (including a future mtime after clock rollback) is not silently
treated as fresh: it produces an `unknown` caveat with a null age.

The list is WARN-ONLY and changes nothing else. It is deliberately NOT part of
`decision`: that object is closed and every field in it ranks the verdict, so a
report about the pack's evidence must not sit where a reader reconciles axes. The
verdict, `allow_merge`, `enforcement_disposition`, the exit codes and every other
field are byte-identical to the same run with a fresh cache. Readers that ignore
the field lose nothing but the date on the evidence, and `tools/validate_merge_gate.py`
neither requires nor rejects it.

## `provenance_contradictions`

An additive, advisory list naming every disagreement between the substrate the
run recorded and the substrate a check recorded for itself. Present from schema
3.0 and empty on a run whose statements agree. The identical list is published
as `consistency.contradictions` in `00_summary/PROVENANCE.json` and as
`quality.consistency.provenance_contradictions` in `report.json`; all three are
derived from one function over one set of inputs, so the files cannot name
different contradictions.

| Field | Type | Notes |
|---|---|---|
| `code` | string | Always `PROVENANCE_CONTRADICTION` — one token to grep across the pack |
| `kind` | string | `operator-worktree-state` \| `check-target-sha` \| `foreign-substrate` |
| `check_id` | string | Policy check id, the same value as `checks[].id` |
| `field` | string | The run-level field the check row contradicts |
| `run_value` | string | What the run says about the substrate |
| `check_value` | string | What the check says about the tree it read |
| `explanation` | string | One sentence naming the disagreement |

The three kinds are the disagreements provable from the rows alone:

- `operator-worktree-state` — the operator working tree was frozen clean (or
  dirty) before the checks ran, and a check that read THAT live tree recorded the
  opposite state;
- `check-target-sha` — a check scanned a commit that is not the reviewed target,
  so its result does not describe the tree the pack judges;
- `foreign-substrate` — a check ran in a checkout that is not this repository, so
  its evidence belongs to a substrate the pack never declared.

Rows replayed from cache are excluded from all three: their provenance describes
the ORIGINAL execution's tree, and holding it against this run's substrate would
claim a contradiction where there is only a cache hit (those replays are dated by
`stale_cache_caveats` instead). A check with no provenance at all is not compared
either — an evidence gap, already visible as nulls in `PROVENANCE.json`, is not a
contradiction.

A contradiction is a **provenance/confidence** problem, never a verified failure:
it says the evidence may not describe the reviewed commit, not that the reviewed
product is defective. Nothing in `decision` is derived from it — no quality
failure, no blocking issue, no axis movement — and it sits outside that object
for the same reason `stale_cache_caveats` does. It is never silent either: every
row is also rendered as a `decision.review_caveats` entry spelled
`PROVENANCE_CONTRADICTION: <explanation>`, `MERGE_GATE.md` explains the class in
words, and `tools/validate_merge_gate.py` rejects a 3.0 gate whose typed rows and
review signals do not correspond one-to-one. That entry is rendered once, by
`ProvenanceConsistency::review_caveats`, and every surface that publishes review
caveats reads that one list: this file's `decision.review_caveats`,
`report.json`'s `gate.review_caveats`, and — through the dashboard context both
sit on — the dashboard and its "Copy PR comment" output. A contradiction visible
to a machine reading `MERGE_GATE.json` but absent from the summary a reviewer
pastes into the PR is the same silence this field exists to prevent. A pack
carrying a contradiction is also
reported as `consistent: false` in `00_summary/CONSISTENCY_CHECK.json` **and** in
`report.json`'s `quality.consistency`: the two sections check different counters,
but neither may call a run consistent while a substrate contradiction stands.

`tools/validate_merge_gate.py` enforces the whole of the above on a 3.0 gate, not
just the row shapes:

- the root `provenance_contradictions` field is **required**, empty array
  included — omitting it is rejected rather than read as "no contradictions",
  because "cross-checked and agreed" and "never cross-checked" are different
  facts and only the field distinguishes them;
- each row's `check_id` must appear as some `checks[].id` in the same file — a
  row attributed to a check the gate never emitted is evidence no reader can
  follow back to anything;
- the `PROVENANCE_CONTRADICTION` entries of `decision.review_caveats` must
  correspond **one-to-one** to the rows as a multiset, each spelled exactly
  `<code>: <explanation>`. Equal counts are not enough: a missing signal, a
  duplicated one covering a second row, or a signal no row supports is rejected.
  When rows exist, an absent or non-array `decision.review_caveats` is rejected
  for the same reason.

## `decision`

The decision object is the merge verdict. Its scalar fields are derived from two
authoritative axes — `analysis_status` (confidence) and `merge_recommendation`
(policy) — plus `quality_pass`, through a single function.

| Field | Type | Notes |
|---|---|---|
| `analysis_status` | string | `complete` \| `degraded` \| `incomplete` |
| `merge_recommendation` | string | `approve` \| `review_required` \| `block` |
| `verdict` | string | `PASS` \| `CONDITIONAL` \| `BLOCK` — the single-field gate for AI consumers |
| `enforcement_disposition` | string | `clean` \| `warnings_only` \| `review_required` \| `block` (required from schema 2.3) |
| `allow_merge` | boolean | Derived: `true` **iff** `verdict == "PASS"` |
| `policy_allow_merge` | boolean | Whether policy hard-blocked (no blocking issues); NOT the same as `allow_merge` |
| `quality_pass` | boolean | No new quality failures from this diff |
| `recommended_merge` | boolean | Legacy flag: `merge_recommendation == approve` |
| `recommended_label` | string | Human gate label (e.g. `MERGE`, `MERGE WITH REVIEW`, `HOLD`, `BLOCK`) |
| `quality_failures` | string[] | Names of quality failures |
| `introduced_quality_failures` | string[] | Failures introduced by this diff |
| `preexisting_quality_failures` | string[] | Pre-existing failures |
| `mixed_quality_failures` | string[] | Mixed-provenance failures |
| `unclassified_quality_failures` | string[] | Failures with unknown provenance |
| `quality_failure_details` | object[] | `[{ name, classification, origin }]` — `name` a non-empty check name, `classification` one of `introduced` \| `pre-existing` \| `mixed` \| `unclassified`, `origin` `"failure"` \| `"warning"` (schema 2.2) |
| `decision_reason` | string | Human-readable reason for the verdict |
| `review_caveats` | string[] | Non-blocking caveats requiring reviewer attention |
| `blocking_issues` | string[] | Issues that block the merge |

`MERGE_GATE.md` and `AI_INDEX.md` expose this full canonical list in a
`Review signals (N)` section. Its count is the list length; entries retain
canonical order and text, with multiline continuation indented inside the
same Markdown item. Empty lists omit the section. Rendering does not change
which caveats require review, their origin, or any decision field.

## Verdict semantics

`verdict` collapses the decision into one enum for AI consumers. It is produced
by `derive_decision` (`src/artifacts/verdict.rs`), which calls
`MergeRecommendation::legacy_verdict` (`src/policy/engine.rs`):

| verdict | condition |
|---|---|
| `BLOCK` | `merge_recommendation == block` |
| `CONDITIONAL` | `merge_recommendation == review_required`, OR `approve` with degraded/incomplete analysis or failing quality |
| `PASS` | `merge_recommendation == approve` AND `analysis_status == complete` AND `quality_pass == true` |

## Enforcement disposition (schema 2.3)

`enforcement_disposition` answers a different question from `verdict`: whether
an operator-selected process lane accepts the already-derived decision. It does
not rank or rewrite `verdict`, `allow_merge`, or `merge_recommendation`. In
particular, a pre-existing warning may legitimately serialize
`verdict: "PASS"`, `allow_merge: true`, and
`enforcement_disposition: "warnings_only"`; readers preserve all three.

| disposition | Typed cause | `prview gate` advisory | `prview gate --strict` | `prview gate --strict --fail-on-warnings` |
|---|---|---:|---:|---:|
| `clean` | No effective warning, review ratchet, or blocker | accept | accept | accept |
| `warnings_only` | One or more effective typed warnings, with complete analysis and no review/block ratchet | accept | accept | reject (`2`) |
| `review_required` | Confirmed/potential breaking under effective escalation, unknown/degraded analysis, or quality failure | accept | reject (`2`) | reject (`2`) |
| `block` | Canonical hard block | block (`1`) | block (`1`) | block (`1`) |

The emitter computes this value while it still holds typed policy evaluations,
legacy breaking findings, and the repo-backed Rust `ApiDelta`. No reader parses
`review_caveats` or other prose to reconstruct a cause. Pure Rust API additions
stay `clean`; confirmed/potential breaking raises only when the existing
`[gate] breaking_escalation` policy is effective. With that operator knob off,
the breaking fact remains informational and does not create a hidden strict
failure. Unknown Rust facts still degrade confidence and require review.

`warnings_only` requires literal typed proof: an exact
`checks[].status=warnings`/`outcome=findings_warning` row, or the validated
inline per-source `warnings_only` disposition. The latter may accompany raw
inline `failed` when a trusted pre-existing error and an introduced warning
produce effective `INFO`; readers never infer it from prose. Every such source
is non-blocking.
Schema 2.3's validator rejects a missing proof, a `clean` disposition beside an
explicit warning, and any lower typed `blocking: true` beside a decision that is
not the full canonical Block tuple. CLI, MCP, and `prview gate` consume the same
schema-aware proof parser and disposition table.

Check-row proof is also relational, not vocabulary-only. Schema 2.3 validates
status/outcome/execution/class tuples, confidence loss, the
conclusion/merge/blocking triad, and policy-mode blocking. `advisory` normally
requires `review_required`; its only `approve` exception is an exact same-name
typed pre-existing failure/warning downgrade. A raw failed/error
check must map one-to-one by name to an `origin: failure`
`quality_failure_details` row. The only legal failed-check `approve` downgrade
is a typed `classification: pre-existing` detail beside the exact
advisory/approve/non-blocking tuple; amputating that detail is an unreadable new
quality failure, not a clean pass. Every `origin: warning` detail, across all
four classifications, likewise maps one-to-one by name to a literal warning
check row. Only the `pre-existing` warning classification can justify the
advisory/approve downgrade; an orphaned or duplicated warning detail is
unreadable review evidence and still counts in warning-clean lanes.

This table governs `prview gate`. Top-level `prview --ci` intentionally retains
the historical adapter: Block or `quality_pass: false` exits `1`, while a typed
review requirement with passing quality remains advisory. Its explicit
`--fail-on-warnings` lane additionally exits `1` whenever the canonical pack
warning tally is non-zero, including a warning mixed with a higher review
disposition.

## Invariants

- **`allow_merge == (verdict == "PASS")`.** `allow_merge` is derived in
  `derive_decision` and set nowhere else. The contradictory state
  `allow_merge: true` beside a `CONDITIONAL` or `BLOCK` verdict is
  unrepresentable (PV-03).
- **`derive_decision` is the single source** of `verdict`, `allow_merge`, and
  `recommended_merge`. No caller sets these fields independently.
- **`enforcement_disposition` is orthogonal to verdict rank.** It selects an
  exit adapter action after the canonical axes are reconciled; it cannot turn a
  `PASS` into `CONDITIONAL` or change `allow_merge`.
- **`policy_allow_merge` is a distinct axis** ("policy did not hard-block") and
  is not conflated with `allow_merge` or the recommendation. It is derived from
  one input and set nowhere else: `policy_allow_merge == blocking_issues.is_empty()`,
  computed after the last entry is pushed and emitted beside that list. The
  contract validator enforces the equivalence in both directions from 2.2.
- **Only `origin: "failure"` entries may fail the quality gate.** Warning-level
  checks enter `quality_failures` (and its classification arrays) so the
  pre-existing downgrade can be computed for them, but they never flip
  `quality_pass`. Reading `introduced_quality_failures` without `origin` is what
  made `quality_pass: true` look like a contradiction; a consumer that wants
  "what actually failed" filters `quality_failure_details` on
  `origin == "failure"`. All three fields of the entry are validated, not just
  the one that names the schema: `tools/validate_merge_gate.py` requires a
  non-empty `name` and a `classification` from the emitted vocabulary, so
  `{"origin": "failure"}` — a failure naming no check and stating no provenance
  — is rejected rather than passed through as contract-clean. The
  classification vocabulary is pinned to `QualityFailureClass::as_str`
  (`src/artifacts/verdict.rs`); note that the value is `pre-existing` while the
  sibling count field is `preexisting_quality_failures`, and an unvalidated
  `classification` is exactly where that drift would hide.
- **An executed check always carries its result artifact and log** (non-null
  `evidence` + `log`); a non-executed check carries non-null placeholders, never
  `null` evidence.

`HOLD` and `ALLOW` are retired pre-2.1 **verdict** synonyms. Current runs never
emit them as `decision.verdict`; the schema validator and the `prview mcp`
adapter still tolerate them on read-back of older packs. `recommended_label`
is a separate human label and can still say HOLD. A failed check can be
non-blocking under policy while failing quality and requiring that HOLD;
MERGE_GATE.md exposes quality, policy, and merge permission to explain it.

Report schema 3.0 mirrors `decision.verdict` in both `gate.verdict` and
`gate.status`. The older report status ALLOW/BLOCK was a permission projection,
so consumers of old packs must use `gate.verdict` to distinguish CONDITIONAL
from BLOCK. No report field changes the canonical decision or exit adapter.

## Reader contract

`MERGE_GATE.json` is the ONLY derivation of the verdict. No reader re-derives one
when the artifact cannot be read: `prview --json` / `--ci` exits `3` and the
`prview mcp` adapter returns `storage_corrupt`. The removed CLI fallback
(`fallback_merge_gate_summary`) re-derived `allow_merge = recommendation != block`
and was the single place where `allow_merge: true` could coexist with a
`CONDITIONAL` verdict, breaking the invariant above.

Readers accept a pack by MAJOR version and say what they had to normalize:

| `schema_version` on disk | Reader behavior |
|---|---|
| absent | Accepted silently — pre-2.1 packs predate the field, and their root object is read as the `decision` |
| known schema through `2.2` | Canonical verdict stays readable, but any injected `enforcement_disposition` is ignored; a legacy `CONDITIONAL` is conservatively `review_required` for strict enforcement |
| `2.3` / `3.0` | `enforcement_disposition`, `checks`, `inline_findings`, policy mode, and typed quality-failure provenance are required and cross-checked as enforcement proof |
| known MAJOR, newer MINOR | Accepted with a `schema_forward_compat:` caveat; the 2.3 typed-enforcement requirements still apply |
| unknown MAJOR, unparsable version, a non-canonical spelling (`02.2`, `+2.2`), or a non-string value | Fail loud |

Errors in the additive disposition axis are deliberately separate from errors
in the canonical evidence axes. A missing, unknown, or wrongly typed 2.3
`enforcement_disposition` preserves the stored `PASS` / `CONDITIONAL` / `BLOCK`
and its `allow_merge`, adds a precise caveat, and normalizes only enforcement to
`review_required`. Advisory mode therefore still reports the pack's canonical
decision, while strict mode rejects it; malformed additive metadata never
manufactures a canonical `BLOCK`, and never unlocks `warnings_only`.

The same shared readback boundary feeds CLI JSON, MCP `run_review`/`verdict`, and
the gate exit adapter. `prview gate` does not deserialize a second enum path or
drop reader caveats: it consumes the normalized `CliJsonSummary`, merges its
caveats with stored review caveats, and applies the one enforcement table.

A pack that STATES a `schema_version` must also carry the `decision` object that
schema is built around. A missing or non-object `decision` there is a corrupt
artifact, not a normalization: the CLI exits `3` and the MCP adapter returns
`storage_corrupt`, matching `tools/validate_merge_gate.py`, which requires
`decision` at every version. Only a pack with NO `schema_version` keeps the
legacy tolerance of reading its root as the decision — and that tolerance is
whole: a legacy pack shaped `{"verdict": "ALLOW", "allow_merge": true}` is read
by BOTH readers, not accepted by one and called corrupt by the other. The rule
lives in one place (`gate::select_decision_object`) so the two surfaces cannot
answer it differently again.

That tolerance is about WHERE the decision sits, not about what counts as one. A
schema-less pack whose root parses to an array, a scalar or `null` has no fields
to read at all: it is corrupt on both readers, not a decision with every signal
missing. Reading one as a decision produced a "successful" summary carrying a
normalized `BLOCK` for an artifact that never stated anything.

A `decision` object that is present and states no canonical decision falls under the same
rule. It must carry at least one of `verdict`, `merge_recommendation` or
`allow_merge`; a block carrying none of the three is corrupt on every surface —
the CLI and `prview gate` exit `3`, the MCP adapter returns `storage_corrupt`,
and `tools/validate_merge_gate.py` rejects it for the required fields it is
missing. The test is PRESENCE, not recognizability: a
stated `verdict: "PROBABLY"` is a decision this pack gave, and it collapses to
`BLOCK` with an `unknown_verdict:` caveat as described below. Absence stays
forgiven per FIELD — that is the shape of an older pack — but a decision block
with no signal at all is not an older pack, it is a truncated one.

For the same reason the tolerance is a fallback, not a precedence rule: a
`decision` object, wherever it appears, is the decision. A schema-less pack that
carries one is read from it rather than from its root, because reading a plainly
stated decision as "every signal missing" would normalize to `BLOCK` and
fabricate a block the artifact never stated. No writer has ever produced that
shape — every generation back to the first public release emits `schema_version`
and `decision` together — so a schema-less pack that ALSO carries root-level
decision fields is undefined by this contract rather than resolved by it.

One vocabulary answers "what verdict is this?" for every surface. The CLI
`--json` summary, the MCP adapter and `prview gate` all fold a stored spelling
through `gate::canonical_verdict`, which is case-insensitive and accepts the
retired synonyms (`ALLOW`/`APPROVE` → `PASS`, `HOLD` → `CONDITIONAL`). Case is
not meaning: a pack stating `verdict: "pass"` stated a pass, and reading it as a
block would fabricate a verdict the artifact never gave. Each surface owning its
own copy of this vocabulary is precisely how they came to read one file three
ways — MCP ranking `"pass"` as a clean `PASS`, the CLI calling it an unknown
verdict and normalizing to `BLOCK`, and `prview gate` refusing the pack as a
verdict mismatch. `GateVerdict` stays a strict parser of the canonical spellings;
it is fed the folded value, never the raw one.

A verdict outside `PASS` / `CONDITIONAL` / `BLOCK` (and the legacy synonyms) is
never read as-is and never silently dropped: BOTH readers collapse it to `BLOCK`
with an `unknown_verdict:` caveat, and the MCP adapter additionally sets
`normalized: true`. A verdict a reader substituted this way also governs
everything derived beside it: `allow_merge` is forced `false` and
`merge_recommendation` to `block`, whatever the same decision block claimed. A
pack whose verdict could not be read is not a pack whose approval can be
trusted, and the exit code follows the recommendation — so the invariant
`allow_merge == (verdict == "PASS")` holds on the substituted verdict too. The
same holds for a verdict that is simply absent while another signal is stated:
the surviving `merge_recommendation: "approve"` does not buy a `PASS` on either
surface, because a pack that names no verdict has not approved anything.

That is what keeps "the artifact is corrupt" and "the artifact said something
this reader cannot use" apart on every surface. `storage_corrupt` is reserved
for a decision block stating none of the three signals. A signal that is present
but unrankable — a verdict outside the vocabulary, a recommendation outside it,
a lone `allow_merge` — is a decision the pack DID give, so it is normalized
conservatively with a caveat instead of being called corrupt by one reader while
the other publishes a summary from it. `allow_merge` is itself a rankable
signal: `false` ranks as `CONDITIONAL` and `true` as `PASS`, and a pack stating
nothing but `allow_merge: true` therefore still reads as `BLOCK` on both
surfaces, because its missing verdict is substituted before its lone flag is
ranked.

The accepted version set is exactly the one `tools/validate_merge_gate.py`
accepts (`1.0`, `2.0`, `2.1`, `2.2`, `2.3`), compared as written — a spelling that merely parses to a known tuple
(`02.2`, `2.02`, `+2.2`) is rejected, so "readable by prview" cannot drift away
from "valid per the contract validator". From schema 2.2 the validator also
requires every `quality_failure_details` entry to carry an `origin` of exactly
`failure` or `warning`: consumers are told to filter on it, which they cannot do
if a pack may omit or mistype it.

From 2.2 it also REQUIRES `quality_pass` and requires it to be a boolean. The
2.2 writer emits the field unconditionally, from a single object literal, as a
Rust `bool` — so a pack that claims 2.2 and omits it, or states it as `"false"`,
is not an old pack but a broken one. Absence stays forgiven below 2.2, where the
readers derive the flag instead, and that carve-out is deliberate: tightening it
would reject every pack written before the field existed. Type-checking it here
is what puts the validator back in step with the readers, which normalize a
present-but-unreadable signal to BLOCK — without it the contract gate certified
an artifact the CLI and MCP both refuse to trust.

From the same version it also cross-checks `quality_pass` against those details,
because the two are one fact written twice: the emitter sets the flag to
`!QualityFailureSummary::has_new_failures()` and then serializes the very
details that answer it. An entry gates the diff when its `origin` is `failure`
AND its `classification` is anything but `pre-existing`, so

> `quality_pass` is true if and only if no `quality_failure_details` entry
> has `origin: "failure"` with a classification other than `pre-existing`.

Both directions are checked. `quality_pass: true` beside
`{"origin": "failure", "classification": "introduced"}` is the combination that
matters: the emitter cannot produce it, both decision readers trust the
permissive scalar, and a validator-clean pack could therefore approve a failure
it also reports. `quality_pass: false` with nothing that could have failed it is
equally unemittable, and rejected too. The `pre-existing` half of the rule is
not a detail — a failure that predates the diff is published beside
`quality_pass: true` deliberately, so the simpler rule "a failure-origin entry
forces `quality_pass: false`" would reject a legitimate pack, and a validator
that cries wolf on genuine output gates nothing. A pack that omits
`quality_pass` entirely is left alone, per the absence rule below.

The blocker axis is cross-checked the same way, and for the same reason. The
emitter computes `policy_allow_merge = blocking_issues.is_empty()` after the
last entry is pushed to that list and then writes both verbatim, so

> `policy_allow_merge` is true if and only if `blocking_issues` is empty.

Both directions are checked from 2.2, where both fields are required.
`policy_allow_merge: true` beside a listed blocker is the shape that matters: it
tells a reader trusting the flag that policy let the merge through while the list
beside it names what blocked it. `false` with nothing in the list is equally
unemittable and rejected too. This is not the same rule as the ranking below,
which asks only how conservative the pack is and is satisfied by either half —
ranking a pair does not check that the pair agrees. It is also not the
pre-existing "no `allow_merge: true` beside a blocker" rule: that one is about
the merge verdict, this one about the policy flag it is derived from. A test in
`src/artifacts/merge_gate.rs` pins the flag to the list across the emitted packs,
so the day the flag gains a second input the emitter fails rather than the
validator rejecting output prview still writes.

### The reconciliation is certified, not only read

From 2.2 the validator requires the remaining decision axes on the same
argument, and with the same vocabularies: `analysis_status` (`complete` /
`degraded` / `incomplete`), `merge_recommendation` (`approve` /
`review_required` / `block`) and a boolean `policy_allow_merge`. All three come
out of the same object literal as `quality_pass`, from the typed enums in
`src/policy/engine.rs`, so a 2.2 pack missing one is broken rather than old. The
two enum vocabularies are case-sensitive and canonical-only — like
`checks[].status`, and unlike the READERS, which fold case and still accept the
retired `hold` spelling when reading an artifact off disk. That tolerance exists
for packs already written; the validator certifies freshly emitted ones. A test
in `src/policy/engine.rs` pins each variant's wire spelling to the word the
validator lists, so a rename cannot silently drift the two apart.

Requiring them is what makes the last certification rule possible: **the
validator rejects a `verdict` milder than the axes stated beside it.** The rank
table above is the readers' rule, and the emitter's `legacy_verdict` produces
exactly the same number from the other direction, so a healthy `verdict` IS the
maximum rank of its own axes. Until this was ported, the contract gate certified
packs no reader would honour — `verdict: "PASS"` beside
`analysis_status: "incomplete"`, `merge_recommendation: "block"` and
`policy_allow_merge: false` validated OK, while every reader normalized the same
artifact to `BLOCK`. Readers were already protected; the hole was in
CERTIFICATION, which is a different claim: that the artifact is what it says it
is.

The rule is deliberately ONE-DIRECTIONAL. A verdict HARSHER than its other axes
is legal and must stay so: a semgrep scan that passes with parse errors writes
`merge_recommendation: "approve"` beside `analysis_status: "degraded"`, which the
contract turns into `CONDITIONAL`, so "the verdict equals the maximum of the
OTHER axes" would reject a pack the emitter really produces. A harsher verdict
also misleads no one — every reader publishes it as stated. It is the permissive
direction that certifies a permission the artifact never earned. (The readers
still NAME the harsher case with a `core_inconsistency:` caveat; that is a report
about a pack, not a rejection of it.)

A decision signal present with the wrong JSON type (`merge_recommendation: 7`,
`allow_merge: "false"`) is not the same as an absent one. Absence is the state a
reader forgives, because it is the shape of an older pack; a field that is there
and cannot be typed is a field the reader FAILED to read, and saying nothing
about it publishes a confidence the read does not have. Both readers name it
with an `unreadable_<field>:` caveat — every axis in the ranking table below.
The MCP adapter additionally sets `normalized: true`; the CLI
forces every decision axis conservative (`verdict: "BLOCK"`,
`allow_merge: false`, `merge_recommendation: block`, and therefore `--ci`
exit `1`), because a decision derived from a block this reader only partly read
is not one it may publish as an approval.

Correctly typed signals that CONTRADICT each other are reconciled the same way,
by conservativeness rather than by field order. Each stated axis ranks
`PASS`/`approve`/`allow_merge: true` as 1, `CONDITIONAL`/`review_required`/
`allow_merge: false` as 2 and `BLOCK`/`block` as 3; the highest rank the pack
states wins and every axis is published from it, with a `core_inconsistency:`
caveat naming the originals. So `verdict: "BLOCK"` beside
`merge_recommendation: "approve"` yields `block` on both surfaces — the CLI used
to believe each field in turn and exit `0` on it — and `allow_merge: true`
beside `review_required` never buys a `PASS`. Both readers rank through
`gate::rank_from_verdict` / `gate::rank_from_merge_rec`. A recommendation
outside the `approve` / `review_required` / `block` vocabulary cannot rank, so
it is excluded from the reconciliation and named with an
`unknown_merge_recommendation:` caveat rather than dropped in silence.

`quality_pass` is one of those axes, because the contract permits `PASS` only
when quality passes. A stated `quality_pass: false` therefore ranks 2 — it says
"not a `PASS`", exactly as `allow_merge: false` does — so a pack shaped
`verdict: "PASS"`, `merge_recommendation: "approve"`, `allow_merge: true`,
`quality_pass: false` is published as `review_required` with
`allow_merge: false` on both surfaces, with the `core_inconsistency:` caveat
naming every original including this one. Leaving that axis out of the
reconciliation published the approval verbatim, so automation reading the MCP
surface approved a run whose own artifact said quality had failed. The
asymmetry is deliberate in both directions: `quality_pass: true` states no rank
at all, because a quality-clean run is still held at `CONDITIONAL` by a
breaking-change escalation and one axis may not soften a verdict the others
agree on; and an ABSENT `quality_pass` states nothing either, per the same
per-field tolerance that governs the other signals — reading it as `false`
would turn every pack written before the field into a `CONDITIONAL`. Those two
states leave a third between them, and `quality_pass` is typed through the same
`gate::readable_signal` as the other axes so it does not fall into it: a
`quality_pass` that is PRESENT but not a boolean is neither a stated `false` nor
an older pack. Read with a bare `as_bool()` it was indistinguishable from
absent, so the string `"false"` bought a silent approval on both surfaces —
the one shape that defeats the paragraph above. It now normalizes to `BLOCK`
with an `unreadable_quality_pass:` caveat, like any other signal the reader
could not type.

### Which fields rank, and which deliberately do not

One rule decides membership: **an axis states a rank only when its value RULES
OUT a more permissive outcome.** A value that merely fails to forbid something
states nothing — that is why `quality_pass: true` is silent, and it is the same
reason `analysis_status: "complete"` is. Both are PRECONDITIONS of `PASS`, not
grants of it: a quality-clean, fully-analysed run is still a `BLOCK` when policy
blocks it, and letting either speak in the permissive direction would let one
axis soften a verdict the others agree on.

The decision object is closed, so every field it may carry is accounted for
here. This table is the contract; a field added to `decision` without a row is
an unfinished change.

| Field | Rank | Rule |
|---|---|---|
| `verdict` | 1 / 2 / 3 | `PASS` \| `CONDITIONAL` \| `BLOCK`, via `gate::rank_from_verdict` |
| `merge_recommendation` | 1 / 2 / 3 | `approve` \| `review_required` \| `block`, via `gate::rank_from_merge_rec` |
| `allow_merge` | 1 / 2 | `false` rules out `PASS`; `true` ranks 1 and so never raises |
| `quality_pass` | 2 | Only `false` ranks — `PASS` requires quality to pass |
| `analysis_status` | 2 | Only `degraded` / `incomplete` rank — `PASS` requires `complete` |
| `blocking_issues` | 3 | Non-empty ranks — see below |
| `policy_allow_merge` | 3 | Only `false` ranks — the same fact as a non-empty `blocking_issues` |
| `enforcement_disposition` | — | Orthogonal exit-policy axis; applied only after canonical rank reconciliation |
| `recommended_merge` | — | Legacy restatement of `merge_recommendation == approve`; ranking it counts one axis twice |
| `recommended_label` | — | Human label with an open vocabulary (`e.g.` in its own row); nothing to rank against |
| `quality_failures` and its four classification arrays | — | Non-empty ≠ failed: warning-origin entries populate them without flipping `quality_pass`, which is precisely the false positive `origin` was added to prevent |
| `quality_failure_details` | — | The evidence BEHIND `quality_pass`, not an independent axis; ranking it would recompute that axis from parts and re-introduce the same warning/failure conflation |
| `decision_reason` | — | Prose |
| `review_caveats` | — | Non-blocking by definition |

`blocking_issues` ranks 3 rather than 2 because a blocker is not a doubt: an
entry appears there only when a check reached `PolicyConclusion::Blocked`, whose
`merge_impact` is `Block`, so a pack listing one has already stated a `BLOCK`
whether or not its `verdict` field agrees. `policy_allow_merge` is the same fact
written twice — the emitter computes `policy_allow_merge =
blocking_issues.is_empty()` — so both are read and both rank, which costs
nothing when they agree and covers a pack that states only one. This does not
conflate `policy_allow_merge` with `allow_merge`: the two remain distinct axes,
and only the value that rules `PASS` out speaks. An empty `blocking_issues` and
`policy_allow_merge: true` state nothing at all, because "policy did not
hard-block" is not "merge is allowed".

#### Ranking an absent field is not publishing one

The table above says what an absent field contributes to the RANK: nothing. It
does not say what the CLI summary should then report for that field, and
conflating the two produced a reader split. A pre-`quality_pass` pack —
`{"verdict": "PASS", "merge_recommendation": "approve", "allow_merge": true}` —
is correctly reconciled to `PASS`, because absence adds no rank; but the summary
published `quality_pass: false` from a bare default, derived
`analysis_status: incomplete` from that, and exited `1` under `--ci`, while the
MCP adapter returned a clean approval for the same artifact.

An absent field is therefore PUBLISHED from the reconciled outcome rather than
from a default. The contract permits `PASS` only when quality passes and the
analysis is complete, so a reconciled `PASS` implies both; a decision held below
`PASS` implies nothing about either axis specifically and both stay
conservative. The direction is one-way — the reconciled verdict can only ever
confirm what the contract already requires of a `PASS`, never soften a verdict.

This does not reopen the absent/mistyped split. A field that is present but
unreadable normalizes the whole decision to `BLOCK`, so by the time the summary
is built the reconciled outcome is not a `PASS` and nothing can be inferred as
passing from it. Absence is forgiven; an unreadable value is not.

Every ranking axis is typed through `gate::readable_signal`, so present-but-
unreadable is a third state distinct from both a stated value and an absent one:
`blocking_issues: "Clippy"` (a string, not an array) or `analysis_status: 7`
normalizes to `BLOCK` with an `unreadable_<field>:` caveat instead of being
mistaken for a pack written before the field. A stated `analysis_status` outside
`complete` / `degraded` / `incomplete` cannot rank, so it is excluded and named
with an `unknown_analysis_status:` caveat — the rule already applied to
`merge_recommendation`.

The `core_inconsistency:` caveat reports a disagreement the pack actually
states, so the comparison is made per axis rather than against the winning rank:
the two textual axes are compared to the published verdict, and `allow_merge` to
the `allow_merge` the readers publish. `allow_merge` has only two values and
`false` ranks as `CONDITIONAL`, so measuring it against a rank it can never
reach made every healthy `BLOCK` pack — `verdict: "BLOCK"`,
`merge_recommendation: "block"`, `allow_merge: false` — report a contradiction
it did not contain. `quality_pass` needs no comparison of its own: ranking 2
when false, it makes any axis claiming 1 beside it disagree with the winning
rank already, and a healthy `BLOCK` or `CONDITIONAL` pack states it in agreement
with everything else. It is named in the caveat all the same, so a reader can
see which axis forced the downgrade. The same holds for `analysis_status`,
`blocking_issues` and `policy_allow_merge`: each ranks in one direction only, so
a healthy pack states them in agreement with the winning rank — a `BLOCK` pack
naming its blocker beside `policy_allow_merge: false` and a `complete` analysis
is exactly what this tool writes and reports no contradiction — while a pack
that states one of them AGAINST a permissive verdict is caught by the textual
axes disagreeing with the rank it forced. All three are named in the caveat.
A pack whose verdict was substituted
reports the substitution (`unknown_verdict:`, `unreadable_<field>:`) and is not
additionally accused of contradicting itself.

## Blocking rules

Whether a check's `FAIL` blocks the merge depends on its policy severity:

- `shadow`: never blocks.
- `warn`: blocks only `FAIL + block`.
- `block`: blocks `FAIL + (block | warn)`.

## Shared snapshot integrity signal

When the ledger-owned review snapshot has tracked/index changes against the
original target, a changed HEAD, or an unknown integrity observation, the decision
requires review regardless of an otherwise passing Cargo result. The emitter
raises analysis to at least `degraded`, merge recommendation to at least
`review_required`, and enforcement disposition to at least `review_required`; it
never lowers an existing BLOCK or rewrites a check status, exit code or quality
failure. `decision.review_caveats` names `20_quality/SNAPSHOT_INTEGRITY.md`, whose
JSON sibling records the complete tracked-path list and source identity. This
uses the existing decision fields; no gate schema migration is introduced.

Clean shared snapshots and local runs add no signal. Newly untracked files
(including a generated Cargo.lock) are excluded; modifications to tracked files
(including a tracked Cargo.lock) and changes committed inside the snapshot are
included. The evidence retains non-clean observations before/after live checks plus
the final observation before context tools, so a later restoration does not erase
an observed change. The human caveat retains an observed HEAD change even when
HEAD is restored and the changed-path list is empty. Boundary check names do not
identify the writer. Changes
restored between observations are not guaranteed to be detected. Results that
overlap non-clean observations are not written to cache; raw results are preserved.
Check-boundary comparisons are awaited blocking-worker jobs; worker failure is
retained as `unknown`, not treated as a clean observation or cache permission.

Checks pin the whole review range — the commit resolved for the diff and the
merge-base commits the diff was computed from — before dispatch; moving or
deleting the configured refs cannot redirect snapshot planning to the operator
checkout, nor re-derive a base range that a moved base branch would collapse onto
the target.
Pinned SHAs are looked up as exact commit objects, including when a symbolic
branch has the same hexadecimal name. Semgrep's planner also rejects unavailable
pinned repositories or commits, and refuses to plan a pinned run that carries no
captured base; that refusal is reported as unavailable evidence,
never as one of the declared mode skips, so a `block` policy still blocks. An unavailable pinned commit is a planning error, not permission to publish a
local-tree pack. Every observation uses the snapshot's immutable creation SHA. If that SHA differs
from the resolved target used for the diff, artifact generation fails before
allocating the output directory. No gate or successful pack is published for
mixed review identities; rerun against a stable target. A later commit made
inside the snapshot remains an integrity signal, not a creation-target mismatch.
A run that reviews a commit other than the captured operator `HEAD` and holds no
shared snapshot at all fails the same way, before allocating the output
directory: a missing observation is never permission to publish. A run that holds
no shared snapshot and could not capture the operator checkout at all — an unborn
`HEAD`, or a checkout that moved while provenance was being read, which the
capture discards rather than certify — fails there too: an unknown checkout
identity is not a known-matching one.
