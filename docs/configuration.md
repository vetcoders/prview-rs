# prview configuration

`prview` is driven by two optional configuration files at the repository root
(usually next to the `.git` folder):

1. `prview.toml`
2. `.prview-policy.yml`

The tool is designed to be **zero-config**: in most cases auto-detection works
without either file, falling back to heuristics. For complex projects (monorepos
or unusual directory layouts) these files give you full control.

---

## 1. `prview.toml` — project and environment

The technical manifest for the repository. It controls **how and where** the
individual scanners run (as opposed to policy analysis, which decides how the
results are judged).

### Example

```toml
[project]
# Path (relative to the repo root) that anchors Cargo-based tooling.
# Overrides the default search and pins this directory as the Rust crate root.
cargo_root = "src-tauri"

[lint]
# Optional ignore globs, added on top of the built-in exclusion lists.
# These ESLint/Stylelint globs silence noisy false positives from
# machine-generated directories.
# Note: absolute bases such as "**/node_modules/**", "**/target/**",
# and "**/coverage/**" are always ignored by default.
ignore_patterns = [
    "**/tauri-codegen-assets/**",
    "**/.next/**",
    "**/build/**"
]

[gate]
# Whether a detected breaking API change escalates the merge verdict.
# Default: true (escalation on out-of-the-box).
breaking_escalation = true

[scope]
# Changed paths this repository knows are not inputs to TEST SELECTION.
# Additive globs over repo-relative paths, on top of the built-in list.
non_participating = ["design/**", "*.drawio"]
# Set to false to drop the built-in list entirely and escalate everything
# that is not recognised source.
non_participating_builtins = true
```

### Options

#### `[project]`

Pins the language root paths explicitly, so prview does not have to guess.

* **`cargo_root`**: forces the starting root for the Cargo toolchain. If you omit
  this key, `prview` first searches the repo root, then falls back to a set of
  common folders (`src-tauri`, `rust`, `crates`, `*_rs`). Listing it here keeps
  detection deterministic.

#### `[lint]`

Adjusts the physical scan surface — mainly to teach `prview` to ignore
project-specific artifacts (for example bundler output).

* **`ignore_patterns`**: a list of `glob` paths that keep ESLint/Stylelint from
  reporting errors in machine-generated code. Applied **in addition to** the
  built-in exclusion list.

#### `[gate]`

Tunes how the merge-gate verdict reacts to structural signals.

* **`breaking_escalation`** (default `true`): when a genuine breaking API change
  is detected (a removed, changed, relocated, or visibility-changed public Rust
  fact; a legacy JS/TS break; or a new required environment variable), the verdict is raised to at least
  `CONDITIONAL`. It never forces a `BLOCK` on its own and never lowers a verdict
  that is already `CONDITIONAL`/`BLOCK` for another reason. Set to `false` to
  keep the breaking findings visible as an **informational caveat only**, with
  no effect on the verdict. The escalation and its reason
  (`breaking API change detected: <n> finding(s)`) appear identically on the
  console summary, `report.json`'s gate block, and `MERGE_GATE.json`'s decision.
  Added-only Rust API touch is informational. Typed unknown Rust regions are a
  confidence failure rather than a confirmed break: they require review even
  when breaking escalation is disabled, but never force `BLOCK` by themselves.

#### `[scope]`

Tunes which changed paths are allowed to participate in deciding **how much of a
test suite has to run**. It affects test selection only: a path listed here is
still in the diff, the artifacts, the signals and the verdict.

* **`non_participating`**: additional `glob` patterns for paths this repository
  knows no test runner reads. They are added to the built-in list
  (`CHANGELOG*` and `LICENSE*`/`LICENCE*` at the repository root, prose under a
  top-level `docs/`/`doc/`, and `.github/workflows/*.yml`). Recognised Rust and
  JS/TS source always wins: a repository cannot declare its own `src/**` neutral
  and quietly stop testing it. An unparsable pattern is dropped with a warning
  rather than applied loosely — a rule prview cannot compile must not neutralise
  a path by accident.
* **`non_participating_builtins`** (default `true`): set to `false` to disable
  the built-in list. This restores strictly escalating behaviour, where every
  path that is not recognised source runs the full suite. Safer and far slower;
  in practice almost every pull request also touches a CHANGELOG or a document.

Everything declared here is **published**: each neutralised path appears with
the rule that named it in the `scope` object on the test rows and as a
`## Test scope` table in `MERGE_GATE.md`, so a reviewer who disagrees with the
call can argue with the rule rather than read the source.

### Not in the manifest: the run deadline

`prview.toml` describes the repository, not the invocation. The run deadline is
a property of *this* run — an operator waiting at a terminal and an unattended
CI job want different bounds from the same checkout — so it lives on the command
line only (`--deadline <TIME>` / `--no-deadline`, defaults in
[`docs/usage.md`](usage.md#run-deadline)) and has no manifest key. The same is
true of `--resource-budget`.

---

## 2. `.prview-policy.yml` — merge rules (CI/gate)

The optional `.prview-policy.yml` assigns policy severity to checks and controls
when a failed check becomes a policy block. It does not select which checks run
or configure coverage thresholds, complexity limits, or warning budgets.

The loader reads `version`, `mode`, `default_severity`, and `checks` at the YAML
root. `checks` maps canonical check IDs, such as `cargo_audit`, `cargo_test`,
`pytest`, and `semgrep_scan`, to `block`, `warn`, or `ignore`.

### Action modes

The mode is set by the `mode` field in the file. It can be overridden from the
command line with `--policy-mode`:

* **`shadow`**: no check becomes a policy block.
* **`warn` (default)**: a failed check blocks only when its configured severity
  is `block`.
* **`block`**: a failed check blocks when its configured severity is `block` or
  `warn`.

Severity `ignore` never creates a policy block. These rules apply to failed or
errored checks, not to every warning. Quality failures and incomplete analysis
remain visible even when policy does not block, so `shadow` does not guarantee
a `PASS` verdict. Process exit codes also depend on the invocation (`--ci`,
`gate`, and their enforcement flags); see [merge gate](contracts/merge_gate.md).

Each severity applies to its check ID. The aggregate `inline_findings` is
evaluated separately, so setting `semgrep_scan: ignore` does not also ignore
that aggregate.

Without a policy file, the defaults are version `1`, mode `warn`, and severity
`warn`, with no per-check overrides. When a policy file is loaded, omitted
values use those defaults and `cargo_audit` defaults to severity `block` unless
the file explicitly overrides it.

### Example

```yaml
version: 1
mode: warn # or: shadow, block
default_severity: warn
checks:
  cargo_audit: block
  cargo_test: block
  semgrep_scan: warn
  inline_findings: warn
```

For the architectural contract of the gate itself, see
`docs/contracts/merge_gate.md`.
