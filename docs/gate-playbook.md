# prview gate rollout playbook

`prview gate` is the automation entry point for local hooks and CI. It runs the
measured fast gate profile, reads the generated `MERGE_GATE.json`, and returns a
stable process exit code. Hook recipes must decide from that exit code only;
they must not parse stdout.

## Exit-code contract

| Exit code | Meaning | Default hook effect |
|-----------|---------|---------------------|
| `0` | `PASS`, or `CONDITIONAL` without `--strict` | Allow |
| `1` | `BLOCK` | Block in Warn and Required |
| `2` | `CONDITIONAL` with `--strict` | Block in Required |
| `3` | Gate execution failed before a trustworthy verdict was available | Block in Warn and Required |

Use `prview gate --json` when CI needs a machine-readable summary, artifact
paths, or SARIF path discovery. Pass/fail still comes from the process exit code.

## Breaking-change escalation

A genuine breaking API change in the diff — a removed public symbol, a changed
public signature, or a newly required environment variable — escalates the
verdict to at least `CONDITIONAL` (never `BLOCK` on its own, and never lowering a
verdict already raised for another reason). Under `--strict` a `CONDITIONAL`
verdict exits `2`, so a breaking change fails a Required gate until an owner
acknowledges it.

The escalation is on by default and controlled by the `[gate]
breaking_escalation` knob in `prview.toml` (see `docs/configuration.md`). Set it
to `false` to keep breaking findings as an **informational caveat only**, with no
effect on the verdict — useful for repositories that intentionally ship breaking
changes on a cadence. Whether on or off, the reason
(`breaking API change detected: <n> finding(s)`) is reported identically on the
console, in `report.json`, and in `MERGE_GATE.json`.

### Which command enforces it

Escalation raises the *verdict*; whether that verdict fails your CI depends on
which command you run. Two contract lines, deliberately distinct:

* **`prview --ci`** — the legacy advisory review exit. It exits `1` only on a
  hard failure (`BLOCK` or a broken quality gate); a `CONDITIONAL` verdict —
  including a breaking-only `CONDITIONAL` — exits `0`, exactly as it does for any
  other `CONDITIONAL` cause. This is the historical review contract and does not
  change with breaking-change escalation.
* **`prview gate`** — the contractual enforcement path. `CONDITIONAL` exits `1`,
  and `prview gate --strict` exits `2` (see the exit-code contract above).

So a breaking change never fails `prview --ci` on its own — to block CI on a
breaking change, run **`prview gate --strict`** as the Required check.

## Rollout ladder: Shadow -> Warn -> Block

Start advisory. A gate that blocks too early trains people to bypass it.

| Stage | Command | Enforcement | Move forward when |
|-------|---------|-------------|-------------------|
| Shadow / advisory | `prview gate` | Report only; hook exits `0` even when the gate exits non-zero | At least 7 days of runs, no false `BLOCK`, no repeated exit `3`, and runtime fits the team budget |
| Warn | `prview gate` | Block only `BLOCK` (`1`) and execution errors (`3`); `CONDITIONAL` remains exit `0` | At least 14 days of runs, false-block rate under 2%, flaky tools fixed, and caveats are triaged quickly |
| Block / required | `prview gate --strict` | Required CI check; blocks `BLOCK`, `CONDITIONAL`, and execution errors | Keep only after owners agree that `CONDITIONAL` is actionable enough to block merges |

Measured baselines from the initial gate profile:

| Repo | Mean wall time | Dominant check |
|------|----------------|----------------|
| `prview-rs` | about 8s | Semgrep plus cached Cargo check |
| `pensieve` | about 47s | Semgrep on a larger repo |

Treat those as starting budgets. If a repo is consistently slower, tune the
policy or Semgrep scope before moving from Shadow to Warn.

## Repository-local hook

This repo ships a raw pre-push hook installer:

```bash
make git-hooks
```

The target is idempotent. It symlinks:

- `tools/githooks/pre-commit` -> `.git/hooks/pre-commit`
- `tools/githooks/pre-push` -> `.git/hooks/pre-push`

The pre-push hook calls `prview gate` and supports rollout modes:

```bash
# Shadow/advisory: never blocks the push
git push

# Warn: blocks BLOCK and execution errors
PRVIEW_GATE_HOOK_MODE=warn git push

# Strict local dry-run of the required CI behavior
PRVIEW_GATE_HOOK_MODE=strict git push
```

Set `PRVIEW_GATE_HOOK_MODE=warn` in your shell environment once the repo has
cleared the Shadow criteria.

## Hook recipes

### Lefthook

`lefthook.yml`:

```yaml
pre-push:
  commands:
    prview-gate:
      run: prview gate
```

Install with:

```bash
lefthook install
```

Advisory Shadow variant:

```yaml
pre-push:
  commands:
    prview-gate:
      run: prview gate || status=$?; echo "prview gate advisory exit: ${status:-0}"; exit 0
```

Required strict variant:

```yaml
pre-push:
  commands:
    prview-gate:
      run: prview gate --strict
```

### pre-commit

`.pre-commit-config.yaml`:

```yaml
default_install_hook_types: [pre-push]

repos:
  - repo: local
    hooks:
      - id: prview-gate
        name: prview gate
        entry: prview gate
        language: system
        pass_filenames: false
        stages: [pre-push]
```

Install with:

```bash
pre-commit install --hook-type pre-push
```

Advisory Shadow variant:

```yaml
repos:
  - repo: local
    hooks:
      - id: prview-gate-advisory
        name: prview gate advisory
        entry: sh -c 'prview gate || status=$?; echo "prview gate advisory exit: ${status:-0}"; exit 0'
        language: system
        pass_filenames: false
        stages: [pre-push]
```

Required strict variant:

```yaml
repos:
  - repo: local
    hooks:
      - id: prview-gate-strict
        name: prview gate strict
        entry: prview gate --strict
        language: system
        pass_filenames: false
        stages: [pre-push]
```

### Husky

Install Husky, then create `.husky/pre-push`:

```sh
prview gate
```

Advisory Shadow variant:

```sh
prview gate
status=$?
echo "prview gate advisory exit: $status"
exit 0
```

Required strict variant:

```sh
prview gate --strict
```

### Raw Git hook

`.git/hooks/pre-push`:

```sh
#!/bin/sh
set -u

if ! command -v prview >/dev/null 2>&1; then
  echo "prview is required: cargo install prview --locked --force" >&2
  exit 3
fi

prview gate
```

Install with:

```bash
chmod +x .git/hooks/pre-push
```

Advisory Shadow variant:

```sh
#!/bin/sh
set -u

prview gate
status=$?
echo "prview gate advisory exit: $status"
exit 0
```

Required strict variant:

```sh
#!/bin/sh
set -u

prview gate --strict
```

## CI required check

Use the composite Action for the final required stage:

```yaml
- uses: vetcoders/prview-rs@main # pin to a released tag once one ships `prview gate`
  id: prview
  with:
    strict: "true"
    version: "latest"
```

Use `strict: "false"` during advisory CI rollout. `CONDITIONAL` remains exit
`0`, while `BLOCK` remains exit `1`.

## Troubleshooting

| Symptom | Likely cause | Fix |
|---------|--------------|-----|
| Hook blocks with exit `3` | `prview` is missing, the repo is not a valid git checkout, or a required tool failed before a verdict was produced | Install `prview`, run `prview gate` manually, and inspect the printed error |
| Hook blocks with exit `1` | Merge gate verdict is `BLOCK` | Open the generated run directory and read `00_summary/MERGE_GATE.json` and `PR_REVIEW.md` |
| Required CI blocks with exit `2` | Verdict is `CONDITIONAL` and CI uses `--strict` | Either fix the caveat or move the repo back to Warn until conditionals are actionable |
| Hook is too slow | Semgrep or language checks dominate the measured budget | Stay in Shadow, tune policy/check scope, then re-measure before Warn |
| SARIF is missing | No inline findings/advisories were produced | This is normal; upload only when `30_context/INLINE_FINDINGS.sarif` exists |

External hook-manager references:

- [Lefthook configuration](https://lefthook.dev/configuration/)
- [pre-commit stages and pre-push installation](https://pre-commit.com/#confining-hooks-to-run-at-certain-stages)
- [Husky get started](https://typicode.github.io/husky/get-started.html)
- [Git pre-push hook contract](https://git-scm.com/docs/githooks#_pre_push)
