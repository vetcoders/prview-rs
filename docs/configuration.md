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

---

## 2. `.prview-policy.yml` — merge rules (CI/gate)

Where `prview.toml` deals only with analytical drivers and parsers, the optional
`.prview-policy.yml` teaches prview **what counts as an acceptable review and
what fails**. It defines the thresholds for letting a PR through (the *merge
gate*): coverage levels, warning budgets, and so on.

### Action modes

The mode is set by the `mode` field in the file. It can be overridden from the
command line with `--policy-mode`:

* **`shadow` (default)**: the repo is evaluated silently. Policy gaps are only
  flagged as potential "Risk" notes in the generated Markdown. The process exit
  code is always zero.
* **`warn`**: emits a non-blocking alert about violations. The verdict is
  reported, but the run still exits successfully.
* **`block`**: hard failure with a non-zero exit code (`exit 1`), failing the CI
  pipeline and stopping the PR flow.

### Example

```yaml
version: 1
policy:
  mode: shadow # or: warn, block

rules:
  no_todos: true                    # Flag newly introduced "// TODO" or "// FIXME" comments
  max_complexity: 15                # No limit by default (disabled from static analysis)
  require_tests: true               # Require fresh tests when new code is introduced (coverage delta)
  max_warnings: 0                   # Warning budget below which the PR is considered clean
  allow_unsafe: false               # Block new `unsafe` blocks in Rust files
```

For the architectural contract of the gate itself, see
`docs/contracts/merge_gate.md`.
