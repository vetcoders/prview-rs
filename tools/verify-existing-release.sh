#!/usr/bin/env bash
set -euo pipefail

: "${TAG:?TAG is required}"
: "${GITHUB_REPOSITORY:?GITHUB_REPOSITORY is required}"
: "${PRVIEW_RELEASE_TARGETS:?PRVIEW_RELEASE_TARGETS is required}"
: "${GITHUB_OUTPUT:?GITHUB_OUTPUT is required}"
: "${GITHUB_STEP_SUMMARY:?GITHUB_STEP_SUMMARY is required}"

TMP=$(mktemp -d)
cleanup() {
  chmod -R u+w "$TMP" 2>/dev/null || true
  find "$TMP" -depth -delete 2>/dev/null || true
}
trap cleanup EXIT

RELEASE_JSON="$TMP/release.json"
RELEASE_ERROR="$TMP/release.error"
if gh api "repos/$GITHUB_REPOSITORY/releases/tags/$TAG" \
  >"$RELEASE_JSON" 2>"$RELEASE_ERROR"; then
  jq -e --arg tag "$TAG" \
    '.tag_name == $tag and .draft == false' "$RELEASE_JSON" >/dev/null || {
    echo "::error::existing release metadata is not a public $TAG release"
    exit 1
  }
elif grep -q '(HTTP 404)' "$RELEASE_ERROR"; then
  echo "exists=false" >>"$GITHUB_OUTPUT"
  exit 0
else
  cat "$RELEASE_ERROR" >&2
  echo "::error::could not determine whether release $TAG already exists"
  exit 1
fi

RELEASE_DIR="$TMP/assets"
EXPECTED_ARCHIVES="$TMP/expected-archives"
EXPECTED_ASSETS="$TMP/expected-assets"
ACTUAL_ASSETS="$TMP/actual-assets"
MANIFEST_NAMES="$TMP/manifest-names"
mkdir -p "$RELEASE_DIR"
gh release download "$TAG" --repo "$GITHUB_REPOSITORY" --dir "$RELEASE_DIR"
printf '%s\n' "$PRVIEW_RELEASE_TARGETS" \
  | tr ' ' '\n' \
  | sed '/^$/d; s|^|prview-|; s|$|.tar.gz|' \
  | LC_ALL=C sort >"$EXPECTED_ARCHIVES"
{
  cat "$EXPECTED_ARCHIVES"
  echo SHA256SUMS
} | LC_ALL=C sort >"$EXPECTED_ASSETS"
jq -r '.assets[].name' "$RELEASE_JSON" | LC_ALL=C sort >"$ACTUAL_ASSETS"
diff -u "$EXPECTED_ASSETS" "$ACTUAL_ASSETS" || {
  echo "::error::existing release $TAG does not contain the exact expected asset set"
  exit 1
}

while IFS= read -r LINE; do
  CHECKSUM=${LINE%%  *}
  NAME=${LINE#*  }
  [[ "$LINE" == "$CHECKSUM  $NAME" && "$CHECKSUM" =~ ^[0-9a-f]{64}$ \
    && "$NAME" =~ ^prview-[A-Za-z0-9._-]+\.tar\.gz$ ]] || {
    echo "::error::existing release $TAG has an unsafe or malformed SHA256SUMS entry"
    exit 1
  }
  echo "$NAME" >>"$MANIFEST_NAMES"
done <"$RELEASE_DIR/SHA256SUMS"
LC_ALL=C sort -o "$MANIFEST_NAMES" "$MANIFEST_NAMES"
diff -u "$EXPECTED_ARCHIVES" "$MANIFEST_NAMES" || {
  echo "::error::existing release $TAG SHA256SUMS does not cover each expected archive exactly once"
  exit 1
}
(cd "$RELEASE_DIR" && sha256sum -c SHA256SUMS)

echo "exists=true" >>"$GITHUB_OUTPUT"
printf '### Existing release replay\n\n%s was verified and left unchanged.\n' "$TAG" \
  >>"$GITHUB_STEP_SUMMARY"
