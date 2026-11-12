# Contributing to prview

Thank you for your interest in contributing. This document covers the essentials.

## Prerequisites

- Rust stable toolchain (edition 2024) -- install via [rustup](https://rustup.rs/)
- Git

## Setup

```bash
git clone https://github.com/vetcoders/prview-rs.git
cd prview-rs
make install
make precheck
```

## Code style

- Format: `cargo fmt` (enforced in CI)
- Lint: `cargo clippy -- -D warnings` (zero warnings policy)

Run both before submitting a PR:

```bash
make precommit
make check
```

Process fixtures that use PID-file existence as a readiness barrier must write
the complete record to a sibling temporary file, close it, and rename it into
place. Creating the final path before writing its contents lets the parent
observe an empty record. The detached MCP reaper fixture follows this protocol.

Binary contract tests must set `PRVIEW_HOME` to a test-owned temporary directory
and keep that directory alive until all child processes finish. This prevents
fixture packs and lock state from leaking into the caller's storage. Preserve
each test's intended tool environment: gate exit-code fixtures deliberately
exclude Semgrep, since its absence is part of the verdict being tested.

## Making changes

This repo is trunk-based on `main`.

1. Fork the repository
2. Create a feature / fix / chore branch from `main`
3. Make your changes
4. Run the local gates before pushing:
   - `make precommit` — fast pre-commit gate
   - `make check` — full local gate (`cargo fmt` + `cargo clippy -- -D warnings` + `cargo test`)
5. Open a pull request against `main`; PRs land as merge commits (no squash)

## Commit conventions

This project uses [Conventional Commits](https://www.conventionalcommits.org/):

```
feat: add risk scoring system
fix: handle empty diff in remote mode
refactor: consolidate check orchestration
test: add regression detector coverage
docs: update CLI help examples
chore: bump dependency versions
```

Scope is optional but welcome: `fix(dashboard): XSS in copy button`.

## Pull request guidelines

- Keep PRs focused -- one logical change per PR
- Include a brief description of what changed and why
- Ensure CI passes (fmt, clippy, tests)
- Link related issues if applicable

## Dependency maintenance

`.github/dependabot.yml` checks Cargo dependencies and GitHub Actions weekly
on Mondays at 07:00 Europe/Warsaw. Cargo version updates have at most two
open PRs; the Loctree library, AST crate, and report renderer are grouped.
Actions updates share one group and at most one open version-update PR.
These limits apply to version updates; GitHub handles security-update PRs
separately ([Dependabot options](https://docs.github.com/en/code-security/reference/supply-chain-security/dependabot-options-reference)).

The bot proposes changes for review and the normal CI gates; this repository
configuration does not enable auto-merge. Dependabot reads its configuration
from the default branch, so adding this file on a feature branch does not yet
activate scheduled updates.

The CI Security Audit uses `python3 tools/audit_dependencies.py`. It applies a
maintainer-approved exceptions for
[RUSTSEC-2026-0457](https://rustsec.org/advisories/RUSTSEC-2026-0457.html) and
[RUSTSEC-2026-0462](https://rustsec.org/advisories/RUSTSEC-2026-0462.html).
Their OAuth and Streamable HTTP client implementations are not compiled by the
current default Cargo feature graph. Loctree 0.15.1 still requires rmcp 2.x,
so updating PrView's direct SDK dependency alone would retain the affected
transitive version.
These exceptions do not remediate that version.

Before applying the exception, the runner requires complete locked Cargo
metadata, exactly one crates.io rmcp 2.0.0 node with the reviewed feature set,
and no OAuth/HTTP dependencies. Any changed source, version, feature set,
missing or inconsistent metadata, or failed metadata command stops before
the exceptions can be used. The metadata covers all resolved platforms and
matches the default feature selection used by this repository's build/test
jobs. A workflow that changes feature selection must update this proof.

The exceptions are passed only to that guarded audit invocation; there is no
new global ignore in `.cargo/audit.toml`. Raw `cargo audit`, including PrView's
ordinary Cargo audit check, still reports both advisories. Existing quick-xml
exceptions are unchanged. Remove the rmcp exceptions and their guard once a
published, validated Loctree dependency chain supports patched rmcp >=3.9.0;
the guard already refuses any rmcp version or feature change pending review.

## Release maintenance

Maintainers should use the repo-level helpers instead of ad-hoc commands:

```bash
make version TYPE=patch
make version-show
make version-check
make release-check
make release-plan
make release-tag
make release-push
make publish-checklist
```

## Reporting bugs

Use the [bug report template](https://github.com/vetcoders/prview-rs/issues/new?template=bug_report.yml)
on GitHub Issues.

## License

By contributing, you agree that your contributions will be licensed under the
[Business Source License 1.1](LICENSE).
