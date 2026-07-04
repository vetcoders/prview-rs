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
5. Click **New**.
6. Select **GitHub Actions**.
7. Set owner to `vetcoders`.
8. Set repository to `prview-rs`.
9. Set workflow file to `release.yml`.
10. Set environment to `release`.
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
