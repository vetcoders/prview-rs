#!/usr/bin/env bash
# publish-checklist.sh — Sync release-facing GitHub metadata
set -euo pipefail

usage() {
  cat <<'EOF'
Usage:
  tools/publish-checklist.sh [--repo ORG/REPO]

Defaults:
  - updates GitHub topics, homepage, and description
  - never creates or pushes git tags
EOF
}

fail() {
  echo "ERROR: $*" >&2
  exit 1
}

REPO="vetcoders/prview-rs"
while [ $# -gt 0 ]; do
  case "$1" in
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

# 4. Release automation status
echo "[4/5] Release automation..."
echo "  Tags are created only after a validated release PR merge."

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
echo "  1. make release-plan"
echo "  2. review and merge the generated release PR"
echo "  3. gh release view ${TAG} --repo ${REPO}"
echo "  4. cargo search ${PACKAGE_NAME}"
echo "  5. cargo info ${PACKAGE_NAME}"
echo
echo "Note: crates.io search can lag briefly after publish."
