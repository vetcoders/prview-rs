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
