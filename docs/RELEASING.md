# Releasing prview

This repository already keeps the release flow in the Makefile and release
helper scripts. Use those as the executable source of truth:

- `make release-gate` runs the full pre-release verification gate.
- `make release-tag` creates the `v<version>` tag after the version and
  changelog are ready.
- `make release-push` pushes the tag and triggers
  `.github/workflows/release.yml`.
- `make publish-checklist` syncs release-facing GitHub metadata.

See also [Installing prview](INSTALL.md#verifying-a-release) for the public
install and verification contract.

## What the release workflow guarantees

`.github/workflows/release.yml` runs the same jobs for a pushed `v*` tag and for
a manual dry run:

1. **preflight** — every required signing/notarization secret is present and
   non-empty. A missing secret fails the run; no signing step is skippable.
2. **validate** — Cargo.toml version matches the tag (on a tag push) and
   `CHANGELOG.md` has both `[Unreleased]` and a section for the version.
3. **build** — the binary is built with `PRVIEW_SOURCE_SHA` set to the released
   commit, then the built binary is executed on its native runner and must print
   exactly that commit from `--build-source-sha` and `prview <version>` from
   `--version`. The Linux binary must additionally not link `libssl`,
   `libcrypto`, `libssh2`, `libgit2` or `libcurl`.
4. **build (macOS only)** — Developer ID signing, strict signature
   verification, a `TeamIdentifier` assertion, a re-run of `--version` and
   `--build-source-sha` against the *signed* binary plus an `otool -L` check
   that every linked library lives under `/usr/lib` or `/System`, Apple
   notarization that must reach `Accepted`, a Gatekeeper check
   (`spctl -a -t open --context context:primary-signature -vv`) that must exit 0
   and report `source=Notarized Developer ID`, and a code directory hash
   comparison proving the archived binary is the notarized one. `--type execute`
   is deliberately not used: it rejects every standalone Mach-O with "does not
   seem to be an app". The signed binary is re-run rather than trusting the
   pre-signing run: the hardened runtime (`--options runtime`) makes dyld refuse
   any non-platform dylib whose Team ID differs from the binary's, so a build
   that links e.g. Homebrew OpenSSL signs, notarizes and passes Gatekeeper while
   aborting on every launch.
5. **checksums** — the set of produced archives must match
   `PRVIEW_RELEASE_TARGETS` exactly, so a dropped build target fails the release
   instead of silently shipping a partial one; `SHA256SUMS` is then regenerated
   deterministically from the archives and verified with `sha256sum -c`.

Only after all of that do the tag-gated `release` (GitHub Release) and
`publish` (crates.io) jobs run.

## Dry run

The workflow accepts `workflow_dispatch`, which executes the identical
preflight, validate, build, sign, notarize and checksum jobs and uploads the
archives plus `SHA256SUMS` as workflow artifacts. The `release` and `publish`
jobs are gated on `github.event_name == 'push'` with a `refs/tags/v` ref, so a
dispatch can never create a release or publish a crate.

```bash
gh workflow run release.yml --ref <branch>
gh run watch
```

Use it to prove the signing and notarization path works before cutting a tag.

## Required secrets

These are read by the release workflow and must be visible to
`vetcoders/prview-rs` — as organization secrets with repository access granted
to this repo, or as repository secrets:

| Secret | Contents |
|--------|----------|
| `MACOS_CERT_P12_BASE64` | base64 of the Developer ID Application `.p12` bundle |
| `MACOS_CERT_P12_PASSWORD` | export password of that `.p12` |
| `NOTARY_API_KEY_ID` | App Store Connect API key id |
| `NOTARY_API_ISSUER_ID` | App Store Connect issuer id |
| `NOTARY_API_KEY_P8_BASE64` | base64 of the App Store Connect `.p8` private key |

The expected Team ID (`MW223P3NPX`) is **not** a secret — it lives in the
workflow `env` as `PRVIEW_MACOS_TEAM_ID` because it is embedded in every
signature. The signing identity itself is derived from the imported certificate
rather than stored separately: the workflow requires exactly one
`Developer ID Application` identity and refuses to sign if its Team ID differs.

Export the certificate with:

```bash
base64 -i Certificates.p12 | pbcopy
```

If the `.p12` was exported by an older Keychain Access it is in legacy PKCS#12
format; `openssl pkcs12 -legacy` is needed to *inspect* it locally, but
`security import` on the runner reads it as-is, so no conversion is required.

## Trusted publishing setup

The release workflow publishes to crates.io through Trusted Publishing (OIDC)
instead of a long-lived `CARGO_REGISTRY_TOKEN` secret. The first manual
crates.io publish for `prview` has already happened, so crate owners can create
the trusted publisher configuration before merging this PR.

Configure crates.io before the next tag release:

1. Open <https://crates.io/crates/prview>.
2. Sign in as an owner of the `prview` crate.
3. Go to **Settings**.
4. Open **Trusted Publishing**.
5. Click **Add**.
6. Select **GitHub Actions**.
7. Set **Repository owner** to `vetcoders`.
8. Set **Repository name** to `prview-rs`.
9. Set **Workflow filename** to `release.yml`.
10. Set **Environment** to `release`.
11. Save the trusted publisher configuration.

After the first successful OIDC-based publish from `.github/workflows/release.yml`,
remove the old organization secret:

1. Open the `vetcoders` GitHub organization.
2. Go to **Settings** -> **Secrets and variables** -> **Actions**.
3. Open **Organization secrets**.
4. Delete `CARGO_REGISTRY_TOKEN`.
5. Confirm that no repository or environment secret with the same name remains
   for `vetcoders/prview-rs`.

Do not delete `CARGO_REGISTRY_TOKEN` before the first OIDC publish succeeds;
until crates.io accepts the trusted publisher configuration, the next release is
not runtime-verified.
