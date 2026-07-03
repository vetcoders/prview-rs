#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'EOF'
Usage:
  tools/semver.sh show
  tools/semver.sh check [--allow-dirty] [--allow-existing-tag]
  tools/semver.sh plan
  tools/semver.sh tag
  tools/semver.sh push

Commands:
  show   Print current package version, binary name, changelog status, and tag state
  check  Validate Cargo.toml, CHANGELOG.md, release workflow, and release tag assumptions
  plan   Print the recommended post-merge release flow for the current Cargo.toml version
  tag    Create annotated tag vX.Y.Z from Cargo.toml after strict checks
  push   Push the current release tag to origin
EOF
}

fail() {
  echo "ERROR: $*" >&2
  exit 1
}

require_file() {
  [ -f "$1" ] || fail "Missing required file: $1"
}

repo_root=$(git rev-parse --show-toplevel 2>/dev/null || pwd)
cd "$repo_root"

require_file Cargo.toml
require_file CHANGELOG.md

package_version() {
  grep '^version = ' Cargo.toml | head -1 | cut -d'"' -f2
}

package_name() {
  grep '^name = ' Cargo.toml | head -1 | cut -d'"' -f2
}

repository_url() {
  grep '^repository = ' Cargo.toml | head -1 | cut -d'"' -f2
}

build_commit() {
  local short
  short=$(git rev-parse --short=12 HEAD 2>/dev/null || echo "unknown")
  if [ -n "$(git status --porcelain 2>/dev/null)" ]; then
    echo "${short}-dirty"
  else
    echo "${short}"
  fi
}

binary_name() {
  awk '
    /^\[\[bin\]\]/ { in_bin = 1; next }
    /^\[/ { if ($0 != "[[bin]]") in_bin = 0 }
    in_bin && /^name = / {
      gsub(/.*"/, "", $0)
      gsub(/".*/, "", $0)
      print $0
      exit
    }
  ' Cargo.toml
}

latest_changelog_version() {
  awk '
    /^## \[[0-9]+\.[0-9]+\.[0-9]+\]/ {
      line = $0
      sub(/^## \[/, "", line)
      sub(/\].*$/, "", line)
      print line
      exit
    }
  ' CHANGELOG.md
}

has_unreleased_section() {
  grep -q '^## \[Unreleased\]' CHANGELOG.md
}

release_workflow_valid() {
  require_file .github/workflows/release.yml
  grep -Eq 'tags:\s*\["v\*"\]' .github/workflows/release.yml \
    && grep -Eq 'cargo publish' .github/workflows/release.yml
}

remote_tag_state() {
  if ! git remote get-url origin >/dev/null 2>&1; then
    echo "no-origin"
    return
  fi

  local out
  if out=$(git ls-remote --tags origin "refs/tags/${TAG}" 2>/dev/null); then
    if [ -n "$out" ]; then
      echo "present"
    else
      echo "absent"
    fi
  else
    echo "unknown"
  fi
}

local_tag_exists() {
  git rev-parse -q --verify "refs/tags/${TAG}" >/dev/null 2>&1
}

ensure_clean_tree() {
  if [ -n "$(git status --porcelain)" ]; then
    fail "Working tree is dirty. Commit or stash changes before release operations."
  fi
}

validate_version_string() {
  if ! printf '%s' "$VERSION" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+$'; then
    fail "Cargo.toml version '$VERSION' is not valid SemVer (expected X.Y.Z)"
  fi
}

check_semver() {
  local allow_dirty=0
  local allow_existing_tag=0

  while [ $# -gt 0 ]; do
    case "$1" in
      --allow-dirty)
        allow_dirty=1
        ;;
      --allow-existing-tag)
        allow_existing_tag=1
        ;;
      --help|-h)
        usage
        exit 0
        ;;
      *)
        fail "Unknown option for check: $1"
        ;;
    esac
    shift
  done

  validate_version_string

  if ! has_unreleased_section; then
    fail "CHANGELOG.md must contain an [Unreleased] section"
  fi

  if [ -z "$CHANGELOG_VERSION" ]; then
    fail "CHANGELOG.md is missing a released version section"
  fi

  if [ "$CHANGELOG_VERSION" != "$VERSION" ]; then
    fail "Cargo.toml version ($VERSION) does not match latest CHANGELOG release ($CHANGELOG_VERSION)"
  fi

  if ! release_workflow_valid; then
    fail ".github/workflows/release.yml must trigger on v* tags and publish the crate"
  fi

  if [ "$allow_dirty" -ne 1 ]; then
    ensure_clean_tree
  fi

  if [ "$allow_existing_tag" -ne 1 ]; then
    if local_tag_exists; then
      fail "Local tag ${TAG} already exists"
    fi

    case "$REMOTE_TAG_STATE" in
      present)
        fail "Remote tag ${TAG} already exists on origin"
        ;;
      unknown)
        echo "[!] Could not verify remote tag state on origin; continuing with local checks."
        ;;
    esac
  fi

  echo "SemVer checks passed for ${TAG}"
}

show_status() {
  echo "Package name:         ${PACKAGE_NAME}"
  echo "Package version:      ${VERSION}"
  echo "Binary name:          ${BIN_NAME}"
  echo "Repository:           ${REPO_URL}"
  echo "Release tag:          ${TAG}"
  echo "Current branch:       ${CURRENT_BRANCH:-detached}"
  echo "Build commit:         ${BUILD_COMMIT}"
  echo "CHANGELOG latest:     ${CHANGELOG_VERSION:-missing}"
  echo "CHANGELOG Unreleased: ${HAS_UNRELEASED}"
  echo "Local tag present:    ${LOCAL_TAG_PRESENT}"
  echo "Remote tag on origin: ${REMOTE_TAG_STATE}"
}

print_plan() {
  cat <<EOF
Release flow for ${TAG}

1. git checkout main
2. git pull origin main
3. make release-check
4. make release-tag
5. make release-push
6. make publish-checklist
7. make install-bin
8. ${BIN_NAME} --version
9. gh release view ${TAG} --repo vetcoders/prview-rs
10. cargo search ${PACKAGE_NAME}
11. cargo info ${PACKAGE_NAME}

Notes:
- Package name on crates.io is '${PACKAGE_NAME}'.
- Binary name is '${BIN_NAME}'.
- GitHub Release should contain two tar.gz archives plus SHA256SUMS.
- crates.io search/index can lag briefly after publish.
EOF
}

create_tag() {
  check_semver
  git tag -a "${TAG}" -m "Release ${TAG}"
  echo "Created tag ${TAG}"
}

push_tag() {
  if ! local_tag_exists; then
    fail "Local tag ${TAG} does not exist. Run 'make release-tag' first."
  fi

  git push origin "${TAG}"
  echo "Pushed tag ${TAG} to origin"
}

VERSION=$(package_version)
PACKAGE_NAME=$(package_name)
REPO_URL=$(repository_url)
BUILD_COMMIT=$(build_commit)
BIN_NAME=$(binary_name)
CHANGELOG_VERSION=$(latest_changelog_version)
TAG="v${VERSION}"
CURRENT_BRANCH=$(git branch --show-current 2>/dev/null || true)
HAS_UNRELEASED="no"
if has_unreleased_section; then
  HAS_UNRELEASED="yes"
fi
LOCAL_TAG_PRESENT="no"
if local_tag_exists; then
  LOCAL_TAG_PRESENT="yes"
fi
REMOTE_TAG_STATE=$(remote_tag_state)

BIN_NAME=${BIN_NAME:-prview}

command="${1:-}"
if [ -z "$command" ]; then
  usage
  exit 1
fi
shift

case "$command" in
  show)
    show_status
    ;;
  check)
    check_semver "$@"
    ;;
  plan)
    print_plan
    ;;
  tag)
    if [ $# -gt 0 ]; then
      fail "tag does not accept extra options"
    fi
    create_tag
    ;;
  push)
    if [ $# -gt 0 ]; then
      fail "push does not accept extra options"
    fi
    push_tag
    ;;
  --help|-h|help)
    usage
    ;;
  *)
    fail "Unknown command: $command"
    ;;
esac
