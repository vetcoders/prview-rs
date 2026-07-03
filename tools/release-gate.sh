#!/usr/bin/env bash
# release-gate.sh — Pre-release verification gate
# Runs all quality + packaging checks that must pass before tagging a release.
# Exit 0 = ship it, exit 1 = blocked.
set -euo pipefail

REPO_ROOT=$(git rev-parse --show-toplevel 2>/dev/null || pwd)
cd "$REPO_ROOT"

VERSION=$(grep '^version = ' Cargo.toml | head -1 | cut -d'"' -f2)
TAG="v${VERSION}"
PASS=0
FAIL=0
WARN=0

pass() { echo "  [PASS] $*"; PASS=$((PASS + 1)); }
fail() { echo "  [FAIL] $*"; FAIL=$((FAIL + 1)); }
warn() { echo "  [WARN] $*"; WARN=$((WARN + 1)); }

echo "=== Release Gate: ${TAG} ==="
echo

# 1. Version consistency
echo "[1/9] Version consistency"
if grep -q "^## \[${VERSION}\]" CHANGELOG.md; then
  pass "CHANGELOG has [${VERSION}] section"
else
  fail "CHANGELOG missing [${VERSION}] section"
fi
if grep -q '^## \[Unreleased\]' CHANGELOG.md; then
  pass "CHANGELOG has [Unreleased] section"
else
  fail "CHANGELOG missing [Unreleased] section"
fi

# 2. Format
echo "[2/9] Format"
if cargo fmt --all --check >/dev/null 2>&1; then
  pass "cargo fmt clean"
else
  fail "cargo fmt has issues (run: make fmt)"
fi

# 3. Clippy
echo "[3/9] Clippy"
if CLIPPY_OUT=$(cargo clippy --all-targets -- -D warnings 2>&1); then
  pass "clippy clean"
else
  echo "$CLIPPY_OUT"
  fail "clippy has warnings"
fi

# 4. Tests
echo "[4/9] Tests"
TEST_OUT=$(cargo test --all-targets 2>&1 || true)
TEST_RESULT=$(echo "$TEST_OUT" | grep "^test result:" | tail -1)
if [ -n "$TEST_RESULT" ] && echo "$TEST_RESULT" | grep -q "0 failed"; then
  PASSED_COUNT=$(echo "$TEST_RESULT" | grep -oE '[0-9]+ passed' | head -1)
  pass "tests: ${PASSED_COUNT}"
elif [ -n "$TEST_RESULT" ]; then
  fail "tests have failures: ${TEST_RESULT}"
else
  # Multiple test binaries — check all results
  ALL_RESULTS=$(echo "$TEST_OUT" | grep "^test result:" || true)
  if [ -z "$ALL_RESULTS" ]; then
    fail "no test results found in output"
  elif echo "$ALL_RESULTS" | grep -vq "0 failed"; then
    fail "some test binaries have failures"
  else
    TOTAL=$(echo "$ALL_RESULTS" | grep -oE '[0-9]+ passed' | awk -F' ' '{s+=$1} END {print s}')
    pass "tests: ${TOTAL} passed across $(echo "$ALL_RESULTS" | wc -l | tr -d ' ') binaries"
  fi
fi

# 5. Release build
echo "[5/9] Release build"
if cargo build --release >/dev/null 2>&1; then
  BIN="target/release/prview"
  if [ -x "$BIN" ]; then
    SIZE=$(du -h "$BIN" | cut -f1 | tr -d ' ')
    pass "release binary exists (${SIZE})"
  else
    fail "release binary not executable"
  fi
else
  fail "release build failed"
fi

# 6. Binary smoke test
echo "[6/9] Binary smoke test"
BIN="target/release/prview"
if [ -x "$BIN" ]; then
  VER_OUT=$("$BIN" --version 2>&1 || true)
  if echo "$VER_OUT" | grep -q "${VERSION}"; then
    pass "binary --version reports ${VERSION}"
  else
    warn "binary --version output: ${VER_OUT}"
  fi

  if "$BIN" --help >/dev/null 2>&1; then
    pass "binary --help exits cleanly"
  else
    fail "binary --help fails"
  fi
else
  fail "no release binary to smoke test"
fi

# 7. Package check
echo "[7/9] Package (cargo package)"
if cargo package --allow-dirty >/dev/null 2>&1; then
  pass "cargo package succeeds"
else
  fail "cargo package fails"
fi

# 8. Release workflow exists
echo "[8/9] Release workflow"
if [ -f .github/workflows/release.yml ]; then
  if grep -Eq 'tags:\s*\["v\*"\]' .github/workflows/release.yml; then
    pass "release.yml triggers on v* tags"
  else
    fail "release.yml missing v* tag trigger"
  fi
  if grep -q 'cargo publish' .github/workflows/release.yml; then
    pass "release.yml publishes to crates.io"
  else
    warn "release.yml does not publish to crates.io"
  fi
  if grep -q 'action-gh-release' .github/workflows/release.yml; then
    pass "release.yml creates GitHub Release"
  else
    fail "release.yml missing GitHub Release step"
  fi
else
  fail ".github/workflows/release.yml not found"
fi

# 9. Tag state
echo "[9/9] Tag state"
if git tag -l "$TAG" | grep -q .; then
  pass "local tag ${TAG} exists"
else
  warn "local tag ${TAG} not created yet (run: make release-tag)"
fi

# Summary
echo
echo "=== Gate Summary ==="
echo "  Pass: ${PASS}  Fail: ${FAIL}  Warn: ${WARN}"
echo
if [ "$FAIL" -gt 0 ]; then
  echo "BLOCKED: ${FAIL} check(s) must be fixed before release."
  exit 1
else
  if [ "$WARN" -gt 0 ]; then
    echo "READY with ${WARN} warning(s). Review before proceeding."
  else
    echo "READY: all checks passed."
  fi
  echo
  echo "Next steps:"
  echo "  1. gh auth login                    # authenticate GitHub CLI"
  echo "  2. make release-tag                 # create v${VERSION} tag"
  echo "  3. make release-push                # push tag → triggers CI release"
  echo "  4. make publish-checklist           # sync GitHub metadata"
  echo "  5. gh release view ${TAG}           # verify release artifacts"
  exit 0
fi
