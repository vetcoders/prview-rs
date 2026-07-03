# Installing prview

## Binary name

The installed binary is called `prview`.

## Install methods

### From GitHub Release (recommended for CI)

Download the pre-built binary for your platform from the
[GitHub Releases](https://github.com/vetcoders/prview-rs/releases) page.

```bash
# macOS (Apple Silicon)
curl -fsSL https://github.com/vetcoders/prview-rs/releases/latest/download/prview-aarch64-apple-darwin.tar.gz \
  | tar xz -C /usr/local/bin

# Linux (x86_64)
curl -fsSL https://github.com/vetcoders/prview-rs/releases/latest/download/prview-x86_64-unknown-linux-gnu.tar.gz \
  | tar xz -C /usr/local/bin
```

Verify the download:

```bash
prview --version
```

SHA256 checksums are published alongside each release as `SHA256SUMS`.

### From crates.io

```bash
cargo install prview
```

### From source

```bash
git clone https://github.com/vetcoders/prview-rs.git
cd prview-rs
make install
```

This builds a release binary and installs it to `$HOME/.cargo/bin/prview`.

## Platform support

| Target                        | CI tested | Release binary |
|-------------------------------|-----------|----------------|
| `aarch64-apple-darwin`        | yes       | yes            |
| `x86_64-unknown-linux-gnu`    | yes       | yes            |

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
cargo install prview@<version>
```
