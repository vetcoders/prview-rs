#!/usr/bin/env bash
set -euo pipefail

fail() {
  echo "ERROR: $*" >&2
  exit 1
}

expected_dir=""
cleanup() {
  if [ -n "$expected_dir" ] && [ -d "$expected_dir" ]; then
    chmod -R u+w "$expected_dir" 2>/dev/null || true
    find "$expected_dir" -depth -delete 2>/dev/null || true
  fi
}
trap cleanup EXIT

usage() {
  cat <<'EOF'
Usage:
  tools/release-contract.sh candidate VERSION EXPECTED_BASE_SHA [HEAD_SHA]
  tools/release-contract.sh merged VERSION EXPECTED_BASE_SHA MERGE_SHA

Validate the machine contract for a release PR or its merge commit.
EOF
}

[ $# -ge 3 ] || { usage >&2; exit 1; }
mode=$1
version=$2
expected_base=$3
target=${4:-HEAD}

printf '%s' "$version" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+$' || fail "Invalid release version: $version"
printf '%s' "$expected_base" | grep -Eq '^[0-9a-f]{40}$' || fail "Expected base must be a full commit SHA"

git rev-parse --verify "$expected_base^{commit}" >/dev/null || fail "Expected base commit is not available: $expected_base"
target_sha=$(git rev-parse "$target^{commit}")

case "$mode" in
  candidate)
    [ "$(git rev-parse "$target_sha^")" = "$expected_base" ] || fail "Release candidate must be exactly one commit on expected main SHA $expected_base"
    expected_subject="chore(release): prepare v$version"
    [ "$(git log -1 --format=%s "$target_sha")" = "$expected_subject" ] || fail "Release candidate commit subject must be: $expected_subject"
    ;;
  merged)
    read -r merge_commit first_parent second_parent extra <<EOF
$(git rev-list --parents -n 1 "$target_sha")
EOF
    if [ -z "${second_parent:-}" ] || [ -n "${extra:-}" ]; then
      fail "Release PR must land as a two-parent merge commit"
    fi
    if [ "$merge_commit" != "$target_sha" ] || [ "$first_parent" != "$expected_base" ]; then
      fail "Release merge first parent must be expected main SHA $expected_base"
    fi
    read -r head_commit head_parent head_extra <<EOF
$(git rev-list --parents -n 1 "$second_parent")
EOF
    if [ "$head_commit" != "$second_parent" ] || [ "$head_parent" != "$expected_base" ] || [ -n "${head_extra:-}" ]; then
      fail "Release PR must contain exactly one non-merge commit on the expected main SHA"
    fi
    expected_subject="chore(release): prepare v$version"
    [ "$(git log -1 --format=%s "$second_parent")" = "$expected_subject" ] || fail "Release PR head commit subject must be: $expected_subject"
    ;;
  *)
    usage >&2
    fail "Unknown validation mode: $mode"
    ;;
esac

changed_files=$(git diff --name-only "$expected_base" "$target_sha" | LC_ALL=C sort)
expected_files=$(printf '%s\n' CHANGELOG.md Cargo.lock Cargo.toml | LC_ALL=C sort)
[ "$changed_files" = "$expected_files" ] || {
  echo "Expected release PR files:" >&2
  printf '%s\n' "$expected_files" >&2
  echo "Actual release PR files:" >&2
  printf '%s\n' "$changed_files" >&2
  fail "Release PR may change only Cargo.toml, Cargo.lock, and CHANGELOG.md"
}

cargo_version=$(grep '^version = ' Cargo.toml | head -1 | cut -d'"' -f2)
[ "$cargo_version" = "$version" ] || fail "Cargo.toml version $cargo_version does not match $version"

lock_version=$(awk '
  /^name = "prview"$/ {
    getline
    if ($0 ~ /^version = /) {
      split($0, parts, "\"")
      print parts[2]
      exit
    }
  }
' Cargo.lock)
[ "$lock_version" = "$version" ] || fail "Cargo.lock prview version $lock_version does not match $version"

grep -Eq "^## \[$version\] - [0-9]{4}-[0-9]{2}-[0-9]{2}$" CHANGELOG.md || fail "CHANGELOG.md is missing a dated [$version] release section"
grep -q '^## \[Unreleased\]$' CHANGELOG.md || fail "CHANGELOG.md is missing [Unreleased]"

if ! awk -v version="$version" '
  /^## \[Unreleased\]$/ { in_unreleased = 1; next }
  in_unreleased && $0 == "## [" version "]" { exit 1 }
  in_unreleased && $0 ~ ("^## \\[" version "\\] - [0-9]{4}-[0-9]{2}-[0-9]{2}$") { found = 1; exit }
  in_unreleased && $0 ~ /^[[:space:]]*$/ { next }
  in_unreleased { exit 2 }
  END { exit(found ? 0 : 3) }
' CHANGELOG.md; then
  fail "CHANGELOG [Unreleased] must be empty and immediately precede release $version"
fi

grep -q "^\[Unreleased\]: .*compare/v$version\.\.\.HEAD$" CHANGELOG.md || fail "CHANGELOG [Unreleased] comparison link does not start at v$version"
grep -q "^\[$version\]: .*\.\.\.v$version$" CHANGELOG.md || fail "CHANGELOG release comparison link is missing for $version"

base_version=$(git show "$expected_base:Cargo.toml" | grep '^version = ' | head -1 | cut -d'"' -f2)
awk -v old="$base_version" -v new="$version" 'BEGIN {
  split(old, a, "."); split(new, b, ".")
  for (i = 1; i <= 3; i++) {
    if ((b[i] + 0) > (a[i] + 0)) exit 0
    if ((b[i] + 0) < (a[i] + 0)) exit 1
  }
  exit 1
}' || fail "Release version $version must be greater than base version $base_version"

# Rebuild the only valid metadata transformation from the expected base and
# compare all three files byte-for-byte. The filename allowlist alone would
# still permit unrelated Cargo manifest or changelog edits.
release_date=$(sed -n "s/^## \[$version\] - \([0-9][0-9-]*\)$/\1/p" CHANGELOG.md)
[ -n "$release_date" ] || fail "Could not resolve release date for $version"
expected_dir=$(mktemp -d)
mkdir -p "$expected_dir/tools"
git show "$expected_base:Cargo.toml" >"$expected_dir/Cargo.toml"
git show "$expected_base:Cargo.lock" >"$expected_dir/Cargo.lock"
git show "$expected_base:CHANGELOG.md" >"$expected_dir/CHANGELOG.md"
git show "$expected_base:tools/version-bump.sh" >"$expected_dir/tools/version-bump.sh"
chmod +x "$expected_dir/tools/version-bump.sh"
(
  cd "$expected_dir"
  PRVIEW_RELEASE_DATE="$release_date" tools/version-bump.sh --set "$version" --no-test --no-commit --force >/dev/null
)
for release_file in Cargo.toml Cargo.lock CHANGELOG.md; do
  cmp -s "$expected_dir/$release_file" "$release_file" || fail "$release_file contains changes outside the deterministic release transformation"
done

printf 'Release %s contract passed for %s (%s)\n' "$mode" "$version" "$target_sha"
