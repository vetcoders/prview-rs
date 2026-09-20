#!/usr/bin/env bash
set -euo pipefail

ROOT=$(git rev-parse --show-toplevel)
TMP=$(mktemp -d)
cleanup() {
  chmod -R u+w "$TMP" 2>/dev/null || true
  find "$TMP" -depth -delete 2>/dev/null || true
}
trap cleanup EXIT

fail() {
  echo "FAIL: $*" >&2
  exit 1
}

assert_contains() {
  local file=$1
  local literal=$2
  grep -Fq -- "$literal" "$file" || fail "$file does not contain: $literal"
}

FIXTURE="$TMP/repo"
mkdir -p "$FIXTURE/tools"
cp "$ROOT/tools/version-bump.sh" "$ROOT/tools/release-contract.sh" "$FIXTURE/tools/"
chmod +x "$FIXTURE/tools/"*.sh

cat >"$FIXTURE/Cargo.toml" <<'EOF'
[package]
name = "prview"
version = "0.8.0"
repository = "https://github.com/vetcoders/prview-rs"
EOF
cat >"$FIXTURE/Cargo.lock" <<'EOF'
version = 4

[[package]]
name = "prview"
version = "0.8.0"
EOF
cat >"$FIXTURE/CHANGELOG.md" <<'EOF'
# Changelog

## [Unreleased]

### Added

- Release automation fixture.

## [0.8.0] - 2026-09-14

### Added

- Previous release.

[Unreleased]: https://github.com/vetcoders/prview-rs/compare/v0.8.0...HEAD
[0.8.0]: https://github.com/vetcoders/prview-rs/compare/v0.7.0...v0.8.0
EOF

git -C "$FIXTURE" init -q
git -C "$FIXTURE" config user.name "Release Test"
git -C "$FIXTURE" config user.email "release-test@example.invalid"
git -C "$FIXTURE" add .
git -C "$FIXTURE" commit -qm "test: seed release fixture"
BASE=$(git -C "$FIXTURE" rev-parse HEAD)

(
  cd "$FIXTURE"
  PRVIEW_RELEASE_DATE=2026-09-20 tools/version-bump.sh --patch --no-test
  tools/release-contract.sh candidate 0.8.1 "$BASE" HEAD
)
CANDIDATE=$(git -C "$FIXTURE" rev-parse HEAD)

assert_contains "$FIXTURE/Cargo.toml" 'version = "0.8.1"'
assert_contains "$FIXTURE/Cargo.lock" 'version = "0.8.1"'
assert_contains "$FIXTURE/CHANGELOG.md" '## [0.8.1] - 2026-09-20'
assert_contains "$FIXTURE/CHANGELOG.md" '[Unreleased]: https://github.com/vetcoders/prview-rs/compare/v0.8.1...HEAD'
assert_contains "$FIXTURE/CHANGELOG.md" '[0.8.1]: https://github.com/vetcoders/prview-rs/compare/v0.8.0...v0.8.1'
[[ -z "$(git -C "$FIXTURE" tag --list)" ]] || fail "metadata preparation created a tag"

git -C "$FIXTURE" checkout -qb nonempty-unreleased "$CANDIDATE"
awk '
  /^## \[Unreleased\]$/ { print; print ""; print "### Added"; next }
  { print }
' "$FIXTURE/CHANGELOG.md" >"$FIXTURE/CHANGELOG.md.tmp"
mv "$FIXTURE/CHANGELOG.md.tmp" "$FIXTURE/CHANGELOG.md"
git -C "$FIXTURE" add CHANGELOG.md
git -C "$FIXTURE" commit -q --amend --no-edit
if UNRELEASED_OUTPUT=$(cd "$FIXTURE" && tools/release-contract.sh candidate 0.8.1 "$BASE" HEAD 2>&1); then
  fail "candidate contract accepted non-empty Unreleased content"
fi
grep -Fq 'CHANGELOG [Unreleased] must be empty and immediately precede release 0.8.1' <<<"$UNRELEASED_OUTPUT" \
  || fail "candidate contract did not reject non-empty Unreleased content at the strict parser"

git -C "$FIXTURE" checkout -qb main "$BASE"
git -C "$FIXTURE" merge -q --no-ff "$CANDIDATE" -m "Merge release fixture"
MERGE=$(git -C "$FIXTURE" rev-parse HEAD)
(cd "$FIXTURE" && tools/release-contract.sh merged 0.8.1 "$BASE" "$MERGE")

git -C "$FIXTURE" checkout -qb tampered "$CANDIDATE"
awk '
  /^repository = / { print "repository = \"https://attacker.invalid/repository\""; next }
  { print }
' "$FIXTURE/Cargo.toml" >"$FIXTURE/Cargo.toml.tmp"
mv "$FIXTURE/Cargo.toml.tmp" "$FIXTURE/Cargo.toml"
git -C "$FIXTURE" add Cargo.toml
git -C "$FIXTURE" commit -q --amend --no-edit
if (cd "$FIXTURE" && tools/release-contract.sh candidate 0.8.1 "$BASE" HEAD >/dev/null 2>&1); then
  fail "candidate contract accepted unrelated Cargo.toml changes"
fi

git -C "$FIXTURE" checkout -qb two-commit "$CANDIDATE"
git -C "$FIXTURE" commit -qm "chore(release): prepare v0.8.1" --allow-empty
TWO_COMMIT=$(git -C "$FIXTURE" rev-parse HEAD)
git -C "$FIXTURE" checkout -qb main-two "$BASE"
git -C "$FIXTURE" merge -q --no-ff "$TWO_COMMIT" -m "Merge two-commit release fixture"
if (cd "$FIXTURE" && tools/release-contract.sh merged 0.8.1 "$BASE" HEAD >/dev/null 2>&1); then
  fail "merged contract accepted a two-commit release branch"
fi

if (cd "$FIXTURE" && tools/version-bump.sh --patch --tag >/dev/null 2>&1); then
  fail "legacy --tag option must fail closed"
fi

PREP="$ROOT/.github/workflows/prepare-release.yml"
MERGED="$ROOT/.github/workflows/release-pr-merged.yml"
RELEASE="$ROOT/.github/workflows/release.yml"
PR_CHECK="$ROOT/.github/workflows/release-pr-contract.yml"
CI="$ROOT/.github/workflows/ci.yml"
RECOVERY_TEST="$ROOT/tools/tests/test_release_publication_recovery.sh"

assert_contains "$PREP" 'expected_main_sha:'
assert_contains "$PREP" 'strict_required_status_checks_policy == true'
assert_contains "$PREP" '.parameters.allowed_merge_methods == ["merge"]'
assert_contains "$PREP" '<!-- prview-release-pr:v1'
assert_contains "$PREP" '**Merging this PR triggers publication.**'
assert_contains "$PREP" "release/v\$VERSION-\${EXPECTED_MAIN_SHA:0:12}"
assert_contains "$PREP" "\"\$VALIDATOR\" candidate"
assert_contains "$PR_CHECK" "EXPECTED_BRANCH=\"release/v\$VERSION-\${EXPECTED_MAIN_SHA:0:12}\""
assert_contains "$MERGED" "github.event.pull_request.merged == true"
assert_contains "$MERGED" "\"\$VALIDATOR\" merged"
assert_contains "$MERGED" "EXPECTED_BRANCH=\"release/v\$VERSION-\${EXPECTED_MAIN_SHA:0:12}\""
assert_contains "$MERGED" "git push origin \"refs/tags/\$TAG\""
assert_contains "$MERGED" 'event_type=prview-release-publish'
assert_contains "$RELEASE" 'workflow_dispatch:'
assert_contains "$RELEASE" 'repository_dispatch:'
assert_contains "$RELEASE" "github.event_name == 'repository_dispatch'"
assert_contains "$RELEASE" 'pull-requests: read'
assert_contains "$RELEASE" "git fetch --no-tags origin \"refs/tags/\$TAG:refs/tags/\$TAG\""
assert_contains "$RELEASE" "TAG_COMMIT=\$(git rev-list -n 1 \"refs/tags/\$TAG\")"
assert_contains "$RELEASE" "BASE_SHA=\$(git rev-parse \"\$PRVIEW_RELEASE_SHA^1\")"
assert_contains "$RELEASE" "grep -q \"^expected_main_sha=\$BASE_SHA\$\""
assert_contains "$PR_CHECK" "PR_BODY=\${PR_BODY//\$'\\r\\n'/\$'\\n'}"
assert_contains "$MERGED" "PR_BODY=\${PR_BODY//\$'\\r\\n'/\$'\\n'}"
assert_contains "$RELEASE" "PR_BODY=\${PR_BODY//\$'\\r\\n'/\$'\\n'}"
assert_contains "$RELEASE" "tag_name: \${{ env.PRVIEW_RELEASE_TAG }}"
assert_contains "$RELEASE" "target_commitish: \${{ env.PRVIEW_RELEASE_SHA }}"
assert_contains "$RELEASE" 'Verify an existing release without replacing assets'
assert_contains "$RELEASE" "if: steps.existing_release.outputs.exists != 'true'"
assert_contains "$RELEASE" 'tools/verify-existing-release.sh'
assert_contains "$RELEASE" "EXPECTED_BRANCH=\"release/v\$VERSION-\${BASE_SHA:0:12}\""
assert_contains "$RELEASE" 'tools/verify-crates-publication.sh'
assert_contains "$RELEASE" 'Verify Published Release'
assert_contains "$RELEASE" 'gh attestation verify'
assert_contains "$PR_CHECK" 'pull_request_target:'
assert_contains "$PR_CHECK" 'git ls-remote origin refs/heads/main'
assert_contains "$PR_CHECK" "git show \"\$EXPECTED_MAIN_SHA:tools/release-contract.sh\""
assert_contains "$PR_CHECK" 'release-shaped PR is missing the exact prview-release-pr:v1 marker'
assert_contains "$MERGED" 'release-shaped PR is missing the exact prview-release-pr:v1 marker'
assert_contains "$CI" 'actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1 # v7'
assert_contains "$CI" 'persist-credentials: false'
assert_contains "$CI" 'types: [opened, synchronize, reopened, ready_for_review]'
assert_contains "$CI" 'tools/tests/test_release_publication_recovery.sh'
assert_contains "$RECOVERY_TEST" 'run_crates_case download_fail 2'
assert_contains "$RECOVERY_TEST" 'run_crates_case checksum_fail 2'
assert_contains "$RECOVERY_TEST" 'run_crates_case source_fail 2'
assert_contains "$RECOVERY_TEST" 'run_crates_case tar_fail 2'
assert_contains "$ROOT/tools/semver.sh" 'git fetch origin main'

CRLF_BODY=$'<!-- prview-release-pr:v1\r\nversion=0.8.1\r\nexpected_main_sha=0123456789012345678901234567890123456789\r\n-->'
CRLF_BODY=${CRLF_BODY//$'\r\n'/$'\n'}
grep -q '^<!-- prview-release-pr:v1$' <<<"$CRLF_BODY" \
  || fail "CRLF normalization did not preserve the exact release marker"
grep -q '^version=0.8.1$' <<<"$CRLF_BODY" \
  || fail "CRLF normalization did not preserve release marker fields"
MALFORMED_BODY=$'<!-- prview-release-pr:v\r1\nversion=0.8.1\n-->'
MALFORMED_BODY=${MALFORMED_BODY//$'\r\n'/$'\n'}
if grep -q '^<!-- prview-release-pr:v1$' <<<"$MALFORMED_BODY"; then
  fail "CRLF normalization accepted a standalone carriage return inside the marker"
fi

if grep -Eq 'MACOS_CERT|NOTARY_API|CARGO_REGISTRY_TOKEN' "$PREP"; then
  fail "PR workflows must not reference release secrets"
fi
if grep -Eq 'force-with-lease|git push --force|git tag -f' "$PREP" "$MERGED"; then
  fail "release automation must not force-update branches or tags"
fi
if [[ $(make -s -C "$ROOT" help | grep -Fc 'make release-plan') -ne 1 ]]; then
  fail "make help must list release-plan exactly once"
fi

echo "Release automation contract tests passed."
