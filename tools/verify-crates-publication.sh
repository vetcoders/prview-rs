#!/usr/bin/env bash
set -euo pipefail

: "${VERSION:?VERSION is required}"
: "${PRVIEW_RELEASE_SHA:?PRVIEW_RELEASE_SHA is required}"
: "${GITHUB_STEP_SUMMARY:?GITHUB_STEP_SUMMARY is required}"

VERSION=${VERSION#v}
VERSION_URL="https://crates.io/api/v1/crates/prview/$VERSION"
ATTEMPTS=${PRVIEW_CRATES_VERIFY_ATTEMPTS:-18}
DELAY_SECONDS=${PRVIEW_CRATES_VERIFY_DELAY_SECONDS:-10}
[[ "$ATTEMPTS" =~ ^[1-9][0-9]*$ ]] || {
  echo "::error::PRVIEW_CRATES_VERIFY_ATTEMPTS must be a positive integer"
  exit 1
}
[[ "$DELAY_SECONDS" =~ ^[0-9]+$ ]] || {
  echo "::error::PRVIEW_CRATES_VERIFY_DELAY_SECONDS must be a non-negative integer"
  exit 1
}

verify_crate_once() {
  JSON=$(curl -fsSL "$VERSION_URL") || return 1
  REGISTRY_VERSION=$(jq -r '.version.num' <<<"$JSON") || return 1
  REGISTRY_CHECKSUM=$(jq -r '.version.checksum' <<<"$JSON") || return 1
  [[ "$REGISTRY_VERSION" == "$VERSION" ]] || return 1
  curl -fsSL "$VERSION_URL/download" -o "prview-$VERSION.crate" || return 1
  ACTUAL_CHECKSUM=$(sha256sum "prview-$VERSION.crate" | awk '{print $1}') || return 1
  [[ "$ACTUAL_CHECKSUM" == "$REGISTRY_CHECKSUM" ]] || return 1
  CRATE_SOURCE_SHA=$(tar -xOf "prview-$VERSION.crate" \
    "prview-$VERSION/.cargo_vcs_info.json" | jq -r '.git.sha1') || return 1
  [[ "$CRATE_SOURCE_SHA" == "$PRVIEW_RELEASE_SHA" ]] || return 1
}

for ((ATTEMPT = 1; ATTEMPT <= ATTEMPTS; ATTEMPT++)); do
  if verify_crate_once; then
    {
      echo "- crates.io v$VERSION: verified"
      echo "- crate checksum: $REGISTRY_CHECKSUM"
      echo "- crate source: $CRATE_SOURCE_SHA"
    } >>"$GITHUB_STEP_SUMMARY"
    exit 0
  fi
  echo "crates.io artifact not fully verifiable (attempt $ATTEMPT/$ATTEMPTS)"
  if ((ATTEMPT < ATTEMPTS && DELAY_SECONDS > 0)); then
    echo "retrying in ${DELAY_SECONDS}s"
    sleep "$DELAY_SECONDS"
  fi
done

echo "::error::crates.io did not expose a fully verifiable prview v$VERSION artifact"
exit 1
