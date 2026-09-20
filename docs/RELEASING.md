# Releasing prview

This page is the operator canon. A release has two explicit decisions:

1. an operator chooses `patch`, `minor`, `major`, or an exact `X.Y.Z`;
2. reviewers choose whether to merge the generated release PR.

The automation never infers SemVer from commits. Merging a valid release PR
creates the tag and starts publication, so the PR body says **merging this PR
triggers publication**. Do not create or push release tags manually.

See [Installing prview](INSTALL.md#verifying-a-release) for the public artifact
and verification contract.

## Operator flow

Before the first automated release, the rules applying to `main` must:

- require the exact **Release PR contract** status check;
- enable strict up-to-date status checks; and
- allow merge commits only (no squash or rebase).

**Prepare Release PR** queries the effective branch rules and fails before
creating anything if any control is absent. This repository's live organization
rules must be updated once after this workflow lands; the current baseline rule
does not yet meet that prerequisite.

Then:

1. Confirm `main` is the intended release source and copy its full SHA:

   ```bash
   git fetch origin main
   git rev-parse origin/main
   ```

2. In Actions, run **Prepare Release PR**. Select `patch`, `minor`, `major`,
   or `exact`. For `exact`, also provide `exact_version`. Always provide
   the copied SHA as `expected_main_sha`.
3. The workflow fails if `main` moved, the version is not strictly newer,
   `[Unreleased]` has no entries, the target tag exists, or a gate fails. On
   success it opens one draft `release/vX.Y.Z-<main-sha-prefix>` PR. Binding
   the branch name to the selected base lets a later run safely prepare the
   same version from a newer `main` without replacing the earlier branch.
4. Review that PR as a publication action. It may change only `Cargo.toml`,
   `Cargo.lock`, and `CHANGELOG.md`; the latter has an empty new
   `[Unreleased]` section followed by the dated promoted release notes.
5. Approve the GitHub-created PR workflow run when GitHub shows its
   approval-required banner. Mark the PR ready and merge it only after review
   and a green **Release PR contract** check. The check runs trusted code from
   the base branch and reads live `main`, never validator code from the PR.
   Strict required-check enforcement reruns it when the base moves. Use a merge
   commit, never squash or rebase. A successful merge is
   the publication authorization; the post-merge validator still fails closed
   if `main` changed in the final race window.
6. Watch **Tag Merged Release PR**, then the separately dispatched **Release**
   run. The final jobs
   cold-install both public artifacts and verify version, source SHA, checksum,
   GitHub provenance, macOS signature/notarization, and the crates.io version.

`make release-plan` prints the short form. `make version TYPE=patch` remains
a local metadata preparation helper, but it never tags or pushes.

## Why an arbitrary merge cannot publish

`release-pr-merged.yml` ignores ordinary PRs. A release PR must have all of:

- the versioned machine marker created by `prepare-release.yml`;
- a same-repository `release/vX.Y.Z-<main-sha-prefix>` head bound to the
  expected base and a matching title;
- the exact expected `main` SHA as the first parent of a two-parent merge;
- one release preparation commit with the canonical subject;
- exactly `Cargo.toml`, `Cargo.lock`, and `CHANGELOG.md` changed;
- matching Cargo and lockfile versions, a dated changelog section, an empty
  `[Unreleased]`, and correct comparison links;
- a version strictly newer than the version at the expected base;
- no existing tag with that name.

Only then does it create an annotated tag at the exact merge commit and push
that one ref without force. If the tag already points to the same commit, a
rerun is a successful no-op; if it points elsewhere, the run fails closed.

The preparation workflow is similarly recoverable: it reuses an already-open
matching PR, and if a prior run pushed the release branch but failed before PR
creation, it validates and reuses that exact branch. A newer `main` produces a
different base-bound branch; the stale draft cannot pass the live-main contract
and may be closed without rewriting or deleting either branch.

## Dry run without publication

`release.yml` still accepts `workflow_dispatch`:

```bash
gh workflow run release.yml --ref <branch>
gh run watch
```

This executes credentials preflight, metadata validation, both builds, macOS
signing and notarization, archive checks, and checksums. GitHub Release,
crates.io publish, and public cold-install verification require the trusted
`repository_dispatch` receipt, so manual dispatch cannot publish.

Use the workflow on the release-automation branch to dogfood the exact HEAD
before merge. It produces workflow artifacts only.

## Publication guarantees

For a validated tag on `main`, `.github/workflows/release.yml`:

1. verifies the tag resolves to the event commit, that commit is on `main`,
   and Cargo/changelog versions match;
2. fails if any signing/notarization secret is missing;
3. builds the two documented targets with the exact source SHA embedded;
4. proves the Linux binary is self-contained;
5. signs the macOS binary with Team ID `MW223P3NPX`, re-runs the hardened
   binary, notarizes it, requires Gatekeeper
   `source=Notarized Developer ID`, and proves the archived code directory
   hash is the one notarized;
6. requires the exact archive set and regenerates/verifies `SHA256SUMS`;
7. creates GitHub build-provenance attestations and the GitHub Release; a replay
   verifies an existing release's exact asset set and checksums and never
   replaces its assets;
8. publishes to crates.io through OIDC trusted publishing;
9. cold-installs from the public release on Linux and macOS, then independently
   rechecks version, source SHA, checksums, attestations, Apple signature,
   notarization, and the crates.io API receipt.

Release concurrency never cancels an in-flight release.

## Required secrets and permissions

Only the trusted Release continuation and its manual dry run receive signing
secrets:

| Secret | Contents |
|--------|----------|
| `MACOS_CERT_P12_BASE64` | Developer ID Application `.p12`, base64 encoded |
| `MACOS_CERT_P12_PASSWORD` | `.p12` export password |
| `NOTARY_API_KEY_ID` | App Store Connect API key id |
| `NOTARY_API_ISSUER_ID` | App Store Connect issuer id |
| `NOTARY_API_KEY_P8_BASE64` | App Store Connect `.p8`, base64 encoded |

No PR-triggered workflow receives these secrets. Prepare Release PR has only
`contents: write` and `pull-requests: write`; its PR CI is read-only. After
a same-repository release PR is merged and fully validated, the tag workflow
sends a secret-free receipt through `repository_dispatch`. The separate
default-branch Release run fetches the merged PR from GitHub, revalidates the
marker, exact merge SHA, tag, deterministic file transformation, and only then
uses signing secrets. The tag workflow itself has only `contents: write` and
`pull-requests: read`.

The crates.io trusted publisher is:

- owner: `vetcoders`
- repository: `prview-rs`
- workflow: `release.yml`
- environment: `release`

It replaces a long-lived `CARGO_REGISTRY_TOKEN`.

## Recovery table

| Failure point | Safe next action |
|---|---|
| Expected SHA mismatch | Re-read `origin/main`, review new commits, dispatch again with the new SHA. |
| Branch pushed, PR absent | Rerun the same inputs; the branch is validated and reused. |
| Matching draft PR exists | The rerun reports its URL and creates no duplicate. |
| Release validation fails after merge | Do not tag manually. Fix automation or prepare a new release PR from current `main`. |
| Tag exists at the validated merge | Rerun is a no-op; continue or retry Release. |
| Tag exists elsewhere | Stop. Tags are immutable; investigate before any new version. |
| Build/sign/notary/checksum fails | If nothing was published, retry the same run after fixing infrastructure; otherwise ship a new version. |
| GitHub Release exists, crates.io failed | Retry the failed publish job; tag and assets remain unchanged. |
| Public verification fails | Treat the release as failed, diagnose, and ship a new version rather than replacing a tag. |

Every failure is visible in the workflow log and job summary. Never overwrite a
tag, force-push a release branch, or publish from a fork/PR workflow.
