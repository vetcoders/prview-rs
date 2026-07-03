#!/usr/bin/env bash
# publish-checklist.sh — Sync release-facing GitHub metadata
set -euo pipefail

usage() {
  cat <<'EOF'
Usage:
  tools/publish-checklist.sh [--create-tag] [--push-tag] [--repo ORG/REPO]

Defaults:
  - updates GitHub topics, homepage, and description
  - does not create or push git tags unless explicitly requested
EOF
}

fail() {
  echo "ERROR: $*" >&2
  exit 1
}

REPO="vetcoders/prview-rs"
CREATE_TAG=0
PUSH_TAG=0

while [ $# -gt 0 ]; do
  case "$1" in
    --create-tag)
      CREATE_TAG=1
      ;;
    --push-tag)
      CREATE_TAG=1
      PUSH_TAG=1
      ;;
    --repo)
      shift
      [ $# -gt 0 ] || fail "--repo requires a value"
      REPO="$1"
      ;;
    --help|-h)
      usage
      exit 0
      ;;
    *)
      fail "Unknown option: $1"
      ;;
  esac
  shift
done

command -v gh >/dev/null 2>&1 || fail "gh CLI is required"

REPO_ROOT=$(git rev-parse --show-toplevel 2>/dev/null || pwd)
cd "$REPO_ROOT"

PACKAGE_NAME=$(grep '^name = ' Cargo.toml | head -1 | cut -d'"' -f2)
VERSION=$(grep '^version = ' Cargo.toml | head -1 | cut -d'"' -f2)
REPO_URL=$(grep '^repository = ' Cargo.toml | head -1 | cut -d'"' -f2)
TAG="v${VERSION}"

echo "=== ${PACKAGE_NAME} publish checklist ==="
echo "Repo:    ${REPO}"
echo "Package: ${PACKAGE_NAME}"
echo "Version: ${VERSION}"
echo

# 1. GitHub topics
echo "[1/5] Setting GitHub topics..."
gh repo edit "$REPO" \
  --add-topic cli \
  --add-topic pr-review \
  --add-topic rust \
  --add-topic code-review \
  --add-topic developer-tools \
  --add-topic sarif \
  --add-topic merge-gate \
  --add-topic code-quality
echo "  Done."

# 2. Homepage URL
echo "[2/5] Setting homepage URL..."
gh repo edit "$REPO" --homepage "${REPO_URL}"
echo "  Done."

# 3. Description
echo "[3/5] Setting description..."
gh repo edit "$REPO" --description "High-signal PR review CLI: cross-language checks, artifact packs, SARIF findings, merge gates"
echo "  Done."

# 4. Optional version tag
echo "[4/5] Tag status..."
if [ "$CREATE_TAG" -eq 1 ]; then
  if git tag -l "$TAG" | grep -q .; then
    echo "  Tag ${TAG} already exists locally, skipping creation."
  else
    echo "  Creating tag ${TAG}..."
    git tag -a "$TAG" -m "Release ${TAG}"
  fi

  if [ "$PUSH_TAG" -eq 1 ]; then
    echo "  Pushing tag ${TAG}..."
    git push origin "$TAG"
  else
    echo "  Tag ready locally. Push with: git push origin ${TAG}"
  fi
else
  echo "  Metadata updated. Tag creation skipped by default."
fi

# 5. Summary
echo
echo "[5/5] Summary"
echo "  Repo:    $REPO"
echo "  Package: ${PACKAGE_NAME}"
echo "  Version: ${TAG}"
echo "  Topics:  cli, pr-review, rust, code-review, developer-tools, sarif, merge-gate, code-quality"
echo "  Assets:  2 tar.gz archives + SHA256SUMS"
echo
echo "Next steps:"
if [ "$CREATE_TAG" -eq 0 ]; then
  echo "  1. make release-tag"
  echo "  2. make release-push"
  echo "  3. gh release view ${TAG} --repo ${REPO}"
else
  echo "  1. gh release view ${TAG} --repo ${REPO}"
fi
echo "  4. cargo search ${PACKAGE_NAME}"
echo "  5. cargo info ${PACKAGE_NAME}"
echo
echo "Note: crates.io search can lag briefly after publish."
