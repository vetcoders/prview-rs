#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'EOF'
Usage:
  tools/version-bump.sh --patch|--minor|--major [options]
  tools/version-bump.sh --set X.Y.Z [options]

Prepare release metadata only. This command never tags or pushes.

Options:
  --patch           Bump patch version (default)
  --minor           Bump minor version
  --major           Bump major version
  --set X.Y.Z       Set an exact version
  --dry-run         Print the selected version without changing files
  --force           Allow a dirty working tree
  --no-test         Skip the local fmt, clippy, and test gate
  --no-commit       Leave the prepared files uncommitted
  --help            Show this help
EOF
}

fail() {
  echo "ERROR: $*" >&2
  exit 1
}

repo_root=$(git rev-parse --show-toplevel 2>/dev/null || pwd)
cd "$repo_root"

for required in Cargo.toml Cargo.lock CHANGELOG.md; do
  [ -f "$required" ] || fail "Missing required file: $required"
done

bump_type="patch"
explicit_version=""
dry_run=false
force=false
skip_tests=false
create_commit=true

while [ $# -gt 0 ]; do
  case "$1" in
    --patch|--minor|--major)
      bump_type=${1#--}
      shift
      ;;
    --set)
      explicit_version=${2:-}
      [ -n "$explicit_version" ] || fail "--set requires a version argument"
      bump_type=exact
      shift 2
      ;;
    --dry-run)
      dry_run=true
      shift
      ;;
    --force)
      force=true
      shift
      ;;
    --no-test)
      skip_tests=true
      shift
      ;;
    --no-commit)
      create_commit=false
      shift
      ;;
    --tag|--push)
      fail "$1 was removed: merge a validated release PR to create and publish the tag"
      ;;
    --help|-h)
      usage
      exit 0
      ;;
    *)
      fail "Unknown option: $1"
      ;;
  esac
done

package_version() {
  grep '^version = ' Cargo.toml | head -1 | cut -d'"' -f2
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

validate_semver() {
  printf '%s' "$1" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+$' || fail "Invalid SemVer version: $1"
}

version_is_greater() {
  awk -v old="$1" -v new="$2" 'BEGIN {
    split(old, a, "."); split(new, b, ".")
    for (i = 1; i <= 3; i++) {
      if ((b[i] + 0) > (a[i] + 0)) exit 0
      if ((b[i] + 0) < (a[i] + 0)) exit 1
    }
    exit 1
  }'
}

bumped_version() {
  local current=$1
  if [ "$bump_type" = exact ]; then
    printf '%s\n' "$explicit_version"
    return
  fi

  local major minor patch
  IFS=. read -r major minor patch <<EOF
$current
EOF
  case "$bump_type" in
    patch) patch=$((patch + 1)) ;;
    minor) minor=$((minor + 1)); patch=0 ;;
    major) major=$((major + 1)); minor=0; patch=0 ;;
    *) fail "Unsupported bump type: $bump_type" ;;
  esac
  printf '%s.%s.%s\n' "$major" "$minor" "$patch"
}

unreleased_has_entries() {
  awk '
    /^## \[Unreleased\]/ { in_unreleased = 1; next }
    in_unreleased && /^## \[[0-9]+\.[0-9]+\.[0-9]+\]/ { exit }
    in_unreleased && /^- / { found = 1 }
    END { exit(found ? 0 : 1) }
  ' CHANGELOG.md
}

promote_unreleased() {
  local version=$1
  local release_date=$2

  awk -v version="$version" -v release_date="$release_date" '
    /^## \[Unreleased\]/ && !inserted {
      print
      print ""
      print "## [" version "] - " release_date
      inserted = 1
      next
    }
    { print }
    END { if (!inserted) exit 42 }
  ' CHANGELOG.md >CHANGELOG.md.tmp || {
    rm -f CHANGELOG.md.tmp
    fail "CHANGELOG.md is missing [Unreleased]"
  }
  mv CHANGELOG.md.tmp CHANGELOG.md
}

update_compare_links() {
  local previous_version=$1
  local new_version=$2
  local repo_url compare_base
  repo_url=$(grep '^repository = ' Cargo.toml | head -1 | cut -d'"' -f2)
  compare_base=${repo_url%/}

  awk -v new_version="$new_version" -v previous_version="$previous_version" -v compare_base="$compare_base" '
    BEGIN { unreleased_done = 0; release_done = 0 }
    /^\[Unreleased\]: / {
      print "[Unreleased]: " compare_base "/compare/v" new_version "...HEAD"
      unreleased_done = 1
      next
    }
    $0 ~ ("^\\[" new_version "\\]: ") { release_done = 1; print; next }
    /^\[[0-9]+\.[0-9]+\.[0-9]+\]: / && !release_done {
      print "[" new_version "]: " compare_base "/compare/v" previous_version "...v" new_version
      release_done = 1
    }
    { print }
    END {
      if (!unreleased_done) print "[Unreleased]: " compare_base "/compare/v" new_version "...HEAD"
      if (!release_done) print "[" new_version "]: " compare_base "/compare/v" previous_version "...v" new_version
    }
  ' CHANGELOG.md >CHANGELOG.md.tmp
  mv CHANGELOG.md.tmp CHANGELOG.md
}

current_version=$(package_version)
previous_changelog_version=$(latest_changelog_version)
new_version=$(bumped_version "$current_version")
release_date=${PRVIEW_RELEASE_DATE:-$(date +%Y-%m-%d)}

validate_semver "$current_version"
validate_semver "$new_version"
[ "$previous_changelog_version" = "$current_version" ] || fail "Cargo.toml version ($current_version) must match latest changelog release ($previous_changelog_version)"
version_is_greater "$current_version" "$new_version" || fail "New version ($new_version) must be greater than current version ($current_version)"
unreleased_has_entries || fail "CHANGELOG.md [Unreleased] must contain at least one entry"
grep -q "^## \[$new_version\]" CHANGELOG.md && fail "CHANGELOG.md already contains release $new_version"

printf 'Current: %s\nNew:     %s\n' "$current_version" "$new_version"
if $dry_run; then
  printf 'Mode:    metadata-only dry run (no files, commit, tag, or push)\n'
  exit 0
fi

if ! $force && [ -n "$(git status --porcelain)" ]; then
  fail "Working tree is dirty. Commit/stash changes first, or use --force."
fi

promote_unreleased "$new_version" "$release_date"
update_compare_links "$previous_changelog_version" "$new_version"

awk -v version="$new_version" '
  BEGIN { done = 0 }
  !done && /^version = / { sub(/^version = ".*"/, "version = \"" version "\""); done = 1 }
  { print }
' Cargo.toml >Cargo.toml.tmp
mv Cargo.toml.tmp Cargo.toml

awk -v version="$new_version" '
  /^name = "prview"$/ {
    print
    getline
    sub(/^version = ".*"/, "version = \"" version "\"")
    print
    next
  }
  { print }
' Cargo.lock >Cargo.lock.tmp
mv Cargo.lock.tmp Cargo.lock

if ! $skip_tests; then
  cargo fmt --all --check
  cargo clippy --all-targets -- -D warnings
  cargo test --all-targets
fi

if $create_commit; then
  git add Cargo.toml Cargo.lock CHANGELOG.md
  git commit -m "chore(release): prepare v$new_version"
fi

printf 'Prepared release metadata: %s -> %s\n' "$current_version" "$new_version"
printf 'No tag or remote ref was created. Merge the validated release PR to continue.\n'
