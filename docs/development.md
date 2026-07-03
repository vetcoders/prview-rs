# Developer guide

This document covers working on the `prview` repo.
If you just want to use the tool, see `docs/usage.md`.
If you want to understand the module layout and execution flow, see `docs/architecture.md`.

## Setup

```bash
git clone https://github.com/vetcoders/prview-rs.git
cd prview-rs
cargo build
```

## Development workflow

### Branching model (trunk-based on `main`)

This repo is trunk-based:

- `main` is the trunk and the stable release branch
- feature / fix / chore branches (`feat/*`, `fix/*`, `chore/*`) are created from `main`
- every branch opens a PR back into `main`
- PRs land as merge commits (no squash)
- release tags (`v*`) are cut from `main`

### Build & run

```bash
# Debug build (faster compile)
cargo build
./target/debug/prview --help

# Release build (optimized)
cargo build --release
./target/release/prview --quick
```

### Rebuild on change

```bash
cargo install cargo-watch
cargo watch -x build
```

### Tests

```bash
cargo test --lib
cargo test --test json_contract
```

For Rust review flow, a standard `prview` run executes tests by default.
You disable them only with a lighter preset (`--quick`, `--update`, `--ai-only`)
or an explicit `--skip-tests`. Exception: standard `--remote-only` is now a
deliberately faster preset, so tests must be restored there with `--with-tests`
or by using `--deep`.

### Linting

```bash
cargo clippy -- -D warnings
cargo fmt --all
```

Likewise, lint is on by default for Rust in standard mode. `--with-lint` mostly
matters when you want to restore it after a lighter preset. Under standard
`--remote-only`, heavy Rust lint (`clippy`, `rustfmt`) is trimmed, so `--with-lint`
or `--deep` restores the fuller pass.

### Local gate

Before pushing, run the same gate CI runs:

```bash
make precommit   # fast pre-commit gate
make check       # full local gate (fmt + clippy + tests)
```

### Documentation

```bash
cargo doc --open
```

## Code structure

```
src/
├── main.rs            # Entry point
├── lib.rs             # App orchestration
├── cli/mod.rs         # CLI parsing (clap)
├── config/mod.rs      # Configuration & profile detection
├── git/mod.rs         # Git operations (git2, Patch API)
├── checks/            # Quality checks (trait-based)
├── heuristics/        # Structural analysis (loctree)
├── artifacts/
│   ├── mod.rs         # Core layout, patches, merge gate, ZIP
│   ├── signal/        # High-signal generators (one module per domain)
│   └── dashboard.rs   # HTML dashboard
├── mcp/               # MCP server (stdio) for agent integrations
├── scope/             # Scoped review packs (filter by files/commits)
├── policy/            # Policy parser + blocking semantics
├── regression/        # Regression detection (diff, perf, deps, score)
├── state/             # Incremental repo state
├── storage/           # Persistent run storage ($PRVIEW_HOME)
├── cache/             # Hash-based caching
├── tui/               # Terminal UI mode
├── output/            # Terminal output formatting
├── proc.rs            # Subprocess hardening (process groups, timeouts)
├── check_id.rs        # Stable check identifiers
└── paths.rs           # Repo-bounded path validation
```

## Adding a new check

1. **Create the file** `src/checks/newcheck.rs`:

```rust
use super::{run_command, Check, CheckResult, CheckStatus};
use crate::Config;
use anyhow::Result;
use async_trait::async_trait;

pub struct NewCheck;

#[async_trait]
impl Check for NewCheck {
    fn name(&self) -> &str {
        "NewCheck"
    }

    fn can_run(&self, config: &Config) -> bool {
        // When this check makes sense
        config.profile.has_something
    }

    async fn run(&self, config: &Config) -> Result<CheckResult> {
        let start = std::time::Instant::now();

        let output = run_command("mytool", &["--check"], &config.repo_root).await?;

        let status = if output.status.success() {
            CheckStatus::Passed
        } else {
            CheckStatus::Failed
        };

        Ok(CheckResult {
            name: self.name().to_string(),
            status,
            duration: start.elapsed(),
            output: String::from_utf8_lossy(&output.stdout).to_string(),
            cached: false,
        })
    }
}
```

2. **Register it in** `src/checks/mod.rs`:

```rust
mod newcheck;
pub use newcheck::NewCheck;

fn get_checks_for_profile(config: &Config) -> Vec<Box<dyn Check>> {
    let mut checks = Vec::new();

    // Add in the right place
    if config.run_lint {
        checks.push(Box::new(NewCheck));
    }

    checks
}
```

## Adding a new profile

In `src/config/mod.rs`:

```rust
#[derive(Debug, Clone, Copy)]
pub enum ProfileKind {
    Js,
    Rust,
    Python,
    Mixed,
    Generic,
    NewLanguage,  // Add
}

fn detect_profile(repo_root: &PathBuf, requested: Profile) -> Result<DetectedProfile> {
    // Add detection
    let has_newlang = repo_root.join("newlang.config").exists();

    // ...
}
```

## Adding a new artifact

Two places depending on the type:

### Core artifact (always generated)

In `src/artifacts/mod.rs`, add it to the right numbered-layout section:

```rust
pub fn generate(...) -> Result<PathBuf> {
    // ...
    // 20_quality/
    generate_my_artifact(&quality_dir, diffs)?;
    // ...
}

fn generate_my_artifact(dir: &Path, diffs: &[Diff]) -> Result<()> {
    let content = "...";
    std::fs::write(dir.join("my_artifact.txt"), content)?;
    Ok(())
}
```

### Signal generator (only written when it has meaningful data)

Signal generators live in `src/artifacts/signal/`, one module per domain. Add
`src/artifacts/signal/my_signal.rs`:

```rust
pub fn generate_my_signal(dir: &Path, diffs: &[Diff]) -> Result<()> {
    let findings = analyze(diffs);
    if findings.is_empty() {
        return Ok(()); // No file = no noise
    }
    std::fs::write(dir.join("my_signal.txt"), format_findings(&findings))?;
    Ok(())
}
```

Then register it in `src/artifacts/signal/mod.rs` (`mod my_signal; pub use my_signal::*;`)
and call it from `artifacts/mod.rs` in the signal-generators section, with status logging.

### If you change the artifact-pack contract

When you change the layout, filenames, or review-flow semantics, update these
together with the code:

- `README.md` for the quick entry point
- `docs/usage.md` for the output layout and artifact interpretation
- `docs/architecture.md` for the module map and responsibilities
- `docs/contracts/merge_gate.md` if the `MERGE_GATE.json` schema or paths change

Pay special attention to root-level navigators (`AI_INDEX.md`, `PR_REVIEW.md`,
`report.json`) and the numbered layout in `00_summary/`, `10_diff/`,
`20_quality/`, `30_context/`.

The `--json` stdout is not a full export of the artifact pack. Treat it as a
compact contract for automation; the full truth about a run stays in the
artifacts written to disk.

## Testing

### Unit tests

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_profile_detection() {
        // ...
    }
}
```

### Integration tests

```bash
# Test against a real repo
cd /path/to/some/repo
/path/to/prview/target/debug/prview --quick
```

## Debugging

### Debug prints

```rust
// Temporary debug prints
eprintln!("DEBUG: {:?}", variable);
```

### RUST_BACKTRACE

```bash
RUST_BACKTRACE=1 cargo run -- --quick
```

### RUST_LOG (if you add tracing)

```bash
RUST_LOG=debug cargo run -- --quick
```

## Performance

### Profiling

```bash
cargo build --release
perf record ./target/release/prview --quick
perf report
```

### Benchmarking

```bash
hyperfine './target/release/prview --quick'
```

## Release

### Build the release binary

```bash
cargo build --release
ls -la target/release/prview
```

### Cross-compilation (optional)

```bash
# Linux (from Mac)
cargo build --release --target x86_64-unknown-linux-gnu

# Windows (from Mac)
cargo build --release --target x86_64-pc-windows-gnu
```

## Status note

This document describes the current way of working on the repo and extending the
code. It is not a product roadmap.

For the current state of CLI features:

```bash
prview --help
```

For the latest design decisions:

- check open PRs against `main`
- read `README.md` as the repo-level entry point
- treat historical plans under `docs/plans/` as a record of work, not a source of truth
```
