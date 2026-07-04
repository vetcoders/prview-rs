# Installing prview

## Binary name

The installed binary is called `prview`.

## Quick install (curl)

The installer downloads the latest release binary for your platform, verifies
its SHA-256 against the published `SHA256SUMS` before unpacking, and installs it
to `~/.local/bin` — never using sudo. If no prebuilt binary exists for your
platform (or the download fails), it falls back to `cargo install prview
--locked --force`.

```bash
curl -fsSL https://raw.githubusercontent.com/vetcoders/prview-rs/main/install.sh | sh
```

Override the target directory with `PRVIEW_INSTALL_DIR`:

```bash
curl -fsSL https://raw.githubusercontent.com/vetcoders/prview-rs/main/install.sh \
  | PRVIEW_INSTALL_DIR="$HOME/bin" sh
```

The script is idempotent — re-running it overwrites the binary in place with no
prompts. After installing it prints `prview --version` and, if the target
directory is not on your `PATH`, the exact lines to add for zsh and bash.

## From crates.io

```bash
cargo install prview --locked --force
```

`--force` overwrites any older `prview` already on `PATH`, so upgrades are
seamless; on a clean machine it is harmless. `--locked` builds against the
published `Cargo.lock` for a reproducible result.

To pin a specific version in CI:

```bash
cargo install prview@<version> --locked --force
```

## From a GitHub Release (manual)

Download the pre-built archive and checksums from the
[GitHub Releases](https://github.com/vetcoders/prview-rs/releases) page, verify,
then unpack into `~/.local/bin` (no sudo):

```bash
# macOS (Apple Silicon)
mkdir -p "$HOME/.local/bin"
cd "$(mktemp -d)"
curl -fsSLO https://github.com/vetcoders/prview-rs/releases/latest/download/prview-aarch64-apple-darwin.tar.gz
curl -fsSLO https://github.com/vetcoders/prview-rs/releases/latest/download/SHA256SUMS
shasum -a 256 --ignore-missing -c SHA256SUMS
tar xzf prview-aarch64-apple-darwin.tar.gz -C "$HOME/.local/bin"
prview --version
```

```bash
# Linux (x86_64)
mkdir -p "$HOME/.local/bin"
cd "$(mktemp -d)"
curl -fsSLO https://github.com/vetcoders/prview-rs/releases/latest/download/prview-x86_64-unknown-linux-gnu.tar.gz
curl -fsSLO https://github.com/vetcoders/prview-rs/releases/latest/download/SHA256SUMS
sha256sum --ignore-missing -c SHA256SUMS
tar xzf prview-x86_64-unknown-linux-gnu.tar.gz -C "$HOME/.local/bin"
prview --version
```

SHA-256 checksums are published alongside each release as `SHA256SUMS`.

## From source

```bash
git clone https://github.com/vetcoders/prview-rs.git
cd prview-rs
make install
```

This builds a release binary and installs it to `$HOME/.cargo/bin/prview`.

## PATH setup

The curl and manual paths install to `~/.local/bin`. If it is not already on
your `PATH`, add it:

```bash
# zsh
echo 'export PATH="$HOME/.local/bin:$PATH"' >> "$HOME/.zshrc"

# bash
echo 'export PATH="$HOME/.local/bin:$PATH"' >> "$HOME/.bashrc"
```

Then restart your shell. `cargo install` uses `~/.cargo/bin`, which rustup
already adds to `PATH`.

## Platform support

| Target                        | CI tested | Release binary |
|-------------------------------|-----------|----------------|
| `aarch64-apple-darwin`        | yes       | yes            |
| `x86_64-unknown-linux-gnu`    | yes       | yes            |

Other platforms build from source via `cargo install prview --locked --force`.

## Verifying a release

Run the built-in release gate to confirm local build health:

```bash
make release-gate
```

## For downstream consumers / CI

The install contract for automated consumption:

- **Binary**: `prview`
- **Archive naming**: `prview-{target}.tar.gz`
- **Checksum**: `SHA256SUMS` in the same release
- **Version query**: `prview --version`
- **Minimum invocation**: `prview --quick` (fast local scan, no network)
- **crates.io package**: `prview`
- **GitHub release trigger**: push of `v*` tag to `main`

To pin a specific version in CI:

```bash
cargo install prview@<version> --locked --force
```
