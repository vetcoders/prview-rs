# AGENTS.md

Conventions for AI agents working in the `prview-rs` repository. Human
contributors: see `CONTRIBUTING.md`.

## Build, test, and gate

```bash
cargo build --release        # build the binary
cargo test                   # run the test suite
make precommit               # fast pre-commit gate
make check                   # full local gate: fmt + clippy + tests
```

Before proposing any change, the gate must pass:

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

Clippy runs with `-D warnings` (zero-warning policy) and `cargo fmt` is enforced
in CI. Do not suppress a lint to make the gate pass; fix the cause.

## Module layout

`prview` is a single Rust binary. `src/cli/` parses flags into a `Config`
(`src/config/`), `src/git/` resolves branches and diffs via `git2`,
`src/checks/` runs language-aware quality gates, `src/heuristics/` computes
structural signals, and `src/artifacts/` assembles the numbered review pack
(`00_summary/`, `10_diff/`, `20_quality/`, `30_context/`) plus the HTML
dashboard. High-signal generators live in `src/artifacts/signal/` (one module
per domain, each writing an artifact only when it has meaningful data).
`src/mcp/` serves the agent-facing tool surface over stdio; `src/scope/`,
`src/state/`, `src/storage/`, `src/cache/`, `src/policy/`, `src/regression/`,
and `src/tui/` round out the rest. See `docs/architecture.md` for the full map.

## Pull request rules

- Trunk-based on `main`: branch from `main`, PR into `main`, land as merge
  commits (no squash).
- [Conventional Commits](https://www.conventionalcommits.org/): `feat:`,
  `fix:`, `refactor:`, `test:`, `docs:`, `chore:`. Scope is optional but welcome
  (`fix(dashboard): …`).
- Keep commits focused: one logical change per commit, one logical change per PR.
- Any change to runtime behavior ships with tests. Contract changes to the
  artifact pack update the relevant docs (`README.md`, `docs/usage.md`,
  `docs/architecture.md`, `docs/contracts/`) in the same PR.

## Reviewing with prview

When reviewing a branch or PR, generate a pack and start from its entry point:

```bash
prview --quick          # or --deep for the full gate
```

- `AI_INDEX.md` at the pack root is the intended entry point for a reviewer
  agent — it points at the highest-signal files in reading order.
- `00_summary/MERGE_GATE.json` is the canonical merge decision
  (`PASS` / `CONDITIONAL` / `BLOCK`).
- `PR_REVIEW.md` is the narrative; `report.json` is the machine-readable payload.

## MCP interface

`prview mcp` exposes the review surface to agents over MCP (stdio) — `health`,
`state`, `run_review`, `verdict`, `findings`, `read_artifact`. Prefer these tools
over driving the CLI and parsing files. Full reference: `docs/mcp.md`.
