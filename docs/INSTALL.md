# Installing prview

## Binary name

The installed binary is called `prview`.

## Quick install (curl)

```bash
curl -fsSL https://raw.githubusercontent.com/vetcoders/prview-rs/main/install.sh | sh
```

The installer is **fail-closed**: it installs an official, verified release
binary or it installs nothing. There is no source-build fallback — it never
runs `cargo`, never clones, and never compiles on your machine. It never uses
sudo.

> **This path requires a release from the signed release pipeline** (added in
> [PR #41](https://github.com/vetcoders/prview-rs/pull/41)). The installer only
> accepts releases that pipeline produces: a Developer ID-signed, notarized
> macOS binary, and an embedded source commit on every platform. Releases up to
> and including **v0.7.0** predate it and are rejected by design. Until the
> first such release is published, this quick-install command fails for
> everyone — so it must not be advertised before then.

On macOS, the notarization check
(`spctl -a -t open --context context:primary-signature`) needs network access to
Apple's notarization service: the ticket is published by Apple rather than
stapled to a bare binary, so the assessment is an online lookup.

### What it verifies

1. **Platform** — only `aarch64-apple-darwin` (macOS arm64) and
   `x86_64-unknown-linux-gnu` (glibc Linux x86_64) have official binaries.
   Anything else, musl Linux included, is a hard failure.
2. **Version** — `latest` is resolved to a concrete release tag *before*
   downloading, so the run knows which version it is installing.
3. **Download** — the archive and `SHA256SUMS` must both download from the
   release.
4. **Checksum** — the archive must match its exact entry in `SHA256SUMS`. A
   missing entry, a malformed manifest, or a mismatch aborts the install. If no
   SHA-256 tool (`sha256sum` or `shasum`) is available the installer fails
   rather than skipping the check.
5. **Archive contents** — the archive must contain exactly one regular file
   named `prview`: no directories, no `../` paths, no symlinks, no extra files.
   Unpacking happens in a temporary directory, never straight over the binary
   you already have.
6. **macOS identity** — on macOS the binary must pass
   `codesign --verify --strict`, report Team ID `MW223P3NPX`, and be accepted by
   Gatekeeper's primary-signature assessment,
   `spctl -a -t open --context context:primary-signature -vv`, which has to
   report exactly `source=Notarized Developer ID`. A binary signed with a
   Developer ID but never notarized reports plain `source=Developer ID` and is
   rejected, as is any binary Gatekeeper rejects outright. (`spctl --assess
   --type execute` is *not* the check: it rejects every standalone executable,
   Apple's own `/bin/ls` included, so it can never confirm a bare binary.) There
   is no bypass environment variable.
7. **Identity of the build** — the binary is executed: `--version` must match
   the resolved tag, and `--build-source-sha` must print a 40-hex commit rather
   than `unknown`.

Checks 6 and 7 run the binary, so it is first copied to a staging file inside
the install directory (`.prview.<pid>.tmp`, mode 755) and both checks run
against that path. Running it from the install directory rather than `$TMPDIR`
also keeps the checks working on hosts that mount `$TMPDIR` with `noexec`.

The atomic rename of the staging file over `prview` is the **last** action of
the run. Until it happens the install directory's `prview` is untouched, and
after it happens there is nothing left that can fail — so a non-zero exit
always means the binary you already had is still exactly the binary you have.
A staging file from a crashed earlier run is never read, executed, or
installed; the current run's own staging file is removed on any failure.

### Environment variables

| Variable | Default | Purpose |
|---|---|---|
| `PRVIEW_INSTALL_DIR` | `~/.local/bin` | Install directory. Never sudo. |
| `PRVIEW_VERSION` | `latest` | Release to install: `latest`, `X.Y.Z`, or `vX.Y.Z`. |
| `PRVIEW_BASE_URL` | `https://github.com/vetcoders/prview-rs/releases` | Release base URL — mirror/testing hook. A mirror that cannot emit HTTP redirects must publish `<base>/latest/VERSION` containing the tag. |
| `PRVIEW_MACOS_TEAM_ID` | `MW223P3NPX` | Expected Apple Team ID, for forks signing with their own Developer ID. It cannot skip the signature or notarization checks. |
| `PRVIEW_TEST_UNAME_S` | unset | **Test/mirror only.** Overrides `uname -s` for platform detection. Honoured only when `PRVIEW_BASE_URL` is not the official release base; against the official releases it is ignored with an info line. |
| `PRVIEW_TEST_UNAME_M` | unset | **Test/mirror only.** Overrides `uname -m`, under the same `PRVIEW_BASE_URL` condition. |

```bash
# Pin a version and a directory
curl -fsSL https://raw.githubusercontent.com/vetcoders/prview-rs/main/install.sh \
  | PRVIEW_VERSION=0.8.0 PRVIEW_INSTALL_DIR="$HOME/bin" sh
```

The script is idempotent — re-running it overwrites the binary in place with no
prompts. On success it prints the version, the source commit, the install path,
and, if the target directory is not on your `PATH`, the exact lines to add for
zsh and bash.

### Exit codes

| Code | Meaning |
|---|---|
| 0 | Installed and verified |
| 1 | Generic failure / missing tooling (`curl`\|`wget`, `sha256sum`\|`shasum`, `tar`) |
| 2 | Unsupported platform |
| 3 | Release artifact missing or download failed |
| 4 | Checksum mismatch, malformed `SHA256SUMS`, or unsafe archive contents |
| 5 | macOS signature / Team ID / notarization verification failed |
| 6 | Binary verification failed (version or build provenance) |

### Releases the installer will refuse

Fail-closed is retroactive. Releases published before signing and build
provenance existed cannot satisfy the checks above:

- On macOS, releases up to and including **v0.7.0** were never signed and are
  rejected with exit 5 by design. For a newer release, an exit 5 here means a
  corrupted or tampered download — re-download it from the official release
  page rather than bypassing the check, or use the manual path below with
  your own judgement about what you are running.
- Any release whose binary reports `--build-source-sha` as `unknown` is
  rejected with exit 6 on every platform.

This is intentional: an installer that silently accepts an unverifiable binary
is worse than one that stops.

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

These two targets are the whole release contract. The curl installer refuses
everything else (exit 2) rather than building a binary for you. Other platforms
may build the CLI from source themselves via
`cargo install prview --locked --force`; this is a build path, not a promise
that every platform-specific runtime surface is supported. In particular, MCP
`run_review` requires a PID-reuse-safe native process-birth identity and is
supported on Linux, macOS, and Windows. On other targets, use the CLI directly.

## Verifying a release

Official releases are built by `.github/workflows/release.yml` on a pushed `v*`
tag. Every published artifact can be verified offline or against Apple.

### Checksums

`SHA256SUMS` is published with each release and lists one `sha256sum`-format
line per archive:

```bash
curl -fsSLO https://github.com/vetcoders/prview-rs/releases/latest/download/SHA256SUMS
# Linux
sha256sum --ignore-missing -c SHA256SUMS
# macOS
shasum -a 256 --ignore-missing -c SHA256SUMS
```

The workflow regenerates the manifest from the downloaded archives in a
byte-sorted order and verifies it with `sha256sum -c` before the release is
created, so a release can never ship an archive that is missing an entry.

### macOS signature and notarization

The `aarch64-apple-darwin` binary is signed with a Developer ID Application
certificate (Team ID `MW223P3NPX`) and notarized by Apple:

```bash
tar xzf prview-aarch64-apple-darwin.tar.gz
codesign -dv --verbose=2 ./prview
# expect: Authority=Developer ID Application: ... (MW223P3NPX)
#         TeamIdentifier=MW223P3NPX
#         flags=0x10000(runtime)

spctl -a -t open --context context:primary-signature -vv ./prview
# expect: ./prview: accepted
#         source=Notarized Developer ID
```

A standalone command-line executable cannot be stapled, so `spctl` resolves the
notarization ticket online; the check needs network access. `source=Notarized
Developer ID` is the proof that the ticket resolved — a plain
`source=Developer ID` means signed but not notarized.

Do not use `spctl --assess --type execute` here: that assessment type only
passes for bundled applications and rejects every standalone executable
(including Apple's own `/bin/ls`) with "the code is valid but does not seem to
be an app".

The Linux binary is not code-signed — verify it with `SHA256SUMS`.

### Provenance

Official binaries embed the exact commit they were built from:

```bash
prview --build-source-sha
# prints the 40-character source commit of this build
```

A release binary that prints `unknown` did not come from the release workflow.
The workflow asserts on both runners that the built binary reports exactly the
commit being released before anything is packaged.

Each published archive and `SHA256SUMS` also carries a signed GitHub build
provenance attestation:

```bash
gh attestation verify prview-aarch64-apple-darwin.tar.gz --repo vetcoders/prview-rs
```

### Local build health

To confirm the state of a local checkout before tagging:

```bash
make release-gate
```

## For downstream consumers / CI

The install contract for automated consumption:

- **Binary**: `prview`
- **Supported targets**: `aarch64-apple-darwin`, `x86_64-unknown-linux-gnu` —
  no others
- **Archive naming**: `prview-{target}.tar.gz`, containing exactly one regular
  file `prview`
- **Checksum**: `SHA256SUMS` in the same release, `sha256sum -c` compatible
- **Version query**: `prview --version` → `prview <X.Y.Z>`
- **Provenance query**: `prview --build-source-sha` → 40-hex commit of the
  source the binary was built from; `unknown` means an unofficial or dev build
- **macOS signing**: Developer ID Application, Team ID `MW223P3NPX`, notarized
- **Version pinning**: `PRVIEW_VERSION=<X.Y.Z>` for the curl installer
- **Installer exit codes**: 0 ok, 1 tooling, 2 unsupported platform, 3 missing
  artifact, 4 checksum/archive invalid, 5 macOS signature/notarization, 6
  binary verification — a non-zero code always means nothing was installed and
  any `prview` already in the install directory is untouched
- **Minimum invocation**: `prview --quick` (fast local scan, no network)
- **crates.io package**: `prview`
- **GitHub release trigger**: push of `v*` tag to `main`

To pin a specific version in CI:

```bash
# Verified official binary (recommended)
curl -fsSL https://raw.githubusercontent.com/vetcoders/prview-rs/main/install.sh \
  | PRVIEW_VERSION=<version> sh

# Or build from source
cargo install prview@<version> --locked --force
```
