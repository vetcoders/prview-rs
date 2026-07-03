#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'EOF'
Usage:
  tools/version-bump.sh --patch|--minor|--major [options]
  tools/version-bump.sh --set X.Y.Z [options]

Options:
  --patch           Bump patch version (default)
  --minor           Bump minor version
  --major           Bump major version
  --set X.Y.Z       Set exact version
  --dry-run         Preview changes only
  --force           Allow dirty working tree
  --no-test         Skip cargo test
  --tag             Create annotated git tag after commit
  --push            Push commit (and tag if created)
  --help            Show this help

Examples:
  tools/version-bump.sh --patch
  tools/version-bump.sh --minor --dry-run
  tools/version-bump.sh --set 0.3.0 --tag --push
EOF
}

fail() {
  echo "ERROR: $*" >&2
  exit 1
}

log() {
  printf '%s\n' "$*"
}

ROOT_DIR="$(git rev-parse --show-toplevel 2>/dev/null || pwd)"
cd "$ROOT_DIR"

[ -f Cargo.toml ] || fail "Missing Cargo.toml"
[ -f CHANGELOG.md ] || fail "Missing CHANGELOG.md"

bump_type="patch"
explicit_version=""
dry_run=false
force=false
skip_tests=false
create_tag=false
push_after=false

while [[ $# -gt 0 ]]; do
  case "$1" in
    --patch|--minor|--major)
      bump_type="${1#--}"
      shift
      ;;
    --set)
      explicit_version="${2:-}"
      [ -n "$explicit_version" ] || fail "--set requires a version argument"
      bump_type="explicit"
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
    --tag)
      create_tag=true
      shift
      ;;
    --push)
      push_after=true
      shift
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

current_version() {
  grep '^version = ' Cargo.toml | head -1 | cut -d'"' -f2
}

package_name() {
  grep '^name = ' Cargo.toml | head -1 | cut -d'"' -f2
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
  local version="$1"
  [[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || fail "Invalid SemVer version: $version"
}

bump_version() {
  local current="$1"
  local kind="$2"

  if [[ "$kind" == "explicit" ]]; then
    printf '%s\n' "$explicit_version"
    return
  fi

  IFS='.' read -r major minor patch <<<"$current"
  case "$kind" in
    patch) patch=$((patch + 1)) ;;
    minor) minor=$((minor + 1)); patch=0 ;;
    major) major=$((major + 1)); minor=0; patch=0 ;;
    *) fail "Unsupported bump type: $kind" ;;
  esac
  printf '%s\n' "${major}.${minor}.${patch}"
}

ensure_clean_tree() {
  if [[ -n "$(git status --porcelain)" ]]; then
    fail "Working tree is dirty. Commit/stash changes first, or use --force."
  fi
}

generate_changelog_entry() {
  local version="$1"
  local today
  today="$(date +%Y-%m-%d)"

  local last_tag
  last_tag="$(git describe --tags --abbrev=0 2>/dev/null || true)"

  local tmp_commits
  tmp_commits="$(mktemp)"
  if [[ -n "$last_tag" ]]; then
    git log --oneline -100 "${last_tag}..HEAD" >"$tmp_commits" 2>/dev/null || true
  else
    git log --oneline -50 >"$tmp_commits" 2>/dev/null || true
  fi

  local added=""
  local changed=""
  local fixed=""
  local security=""
  local fallback_changed=""

  while IFS= read -r commit || [[ -n "$commit" ]]; do
    [[ -z "$commit" ]] && continue
    local subject="${commit#* }"
    local msg="$subject"

    case "$subject" in
      feat:*|feat\(*\):*)
        msg="${subject#feat}"
        msg="${msg#\(*\):}"
        msg="${msg#:}"
        msg="${msg# }"
        added="${added}- ${msg}"$'\n'
        ;;
      fix:*|fix\(*\):*)
        msg="${subject#fix}"
        msg="${msg#\(*\):}"
        msg="${msg#:}"
        msg="${msg# }"
        fixed="${fixed}- ${msg}"$'\n'
        ;;
      security:*|security\(*\):*)
        msg="${subject#security}"
        msg="${msg#\(*\):}"
        msg="${msg#:}"
        msg="${msg# }"
        security="${security}- ${msg}"$'\n'
        ;;
      refactor:*|refactor\(*\):*|perf:*|perf\(*\):*|chore:*|chore\(*\):*|docs:*|docs\(*\):*|build:*|build\(*\):*|ci:*|ci\(*\):*|test:*|test\(*\):*|style:*|style\(*\):*|revert:*|revert\(*\):*|*BREAKING*|*breaking*|*!:*)
        changed="${changed}- ${subject}"$'\n'
        ;;
      *)
        fallback_changed="${fallback_changed}- ${subject}"$'\n'
        ;;
    esac
  done <"$tmp_commits"

  rm -f "$tmp_commits"

  {
    echo "## [$version] - $today"
    echo
    [[ -n "$added" ]] && echo "### Added" && printf "%s\n" "$added" && echo
    [[ -n "$changed" ]] && echo "### Changed" && printf "%s\n" "$changed" && echo
    [[ -n "$fixed" ]] && echo "### Fixed" && printf "%s\n" "$fixed" && echo
    [[ -n "$security" ]] && echo "### Security" && printf "%s\n" "$security" && echo
    if [[ -z "$added$changed$fixed$security" && -n "$fallback_changed" ]]; then
      echo "### Changed"
      printf "%s\n" "$fallback_changed"
      echo
    fi
  } | sed '/^$/N;/^\n$/D'
}

insert_changelog_entry() {
  local new_version="$1"
  local entry_file
  entry_file="$(mktemp)"
  generate_changelog_entry "$new_version" >"$entry_file"

  awk -v entry_file="$entry_file" '
    /^## \[[0-9]/ && !inserted {
      while ((getline line < entry_file) > 0) print line
      close(entry_file)
      print ""
      inserted = 1
    }
    { print }
    END {
      if (!inserted) {
        while ((getline line < entry_file) > 0) print line
        close(entry_file)
      }
    }
  ' CHANGELOG.md > CHANGELOG.md.tmp

  mv CHANGELOG.md.tmp CHANGELOG.md
  rm -f "$entry_file"
}

update_compare_links() {
  local previous_version="$1"
  local new_version="$2"
  local repo_url
  repo_url="$(grep '^repository = ' Cargo.toml | head -1 | cut -d'"' -f2)"
  local compare_base="${repo_url%/}"

  awk \
    -v new_version="$new_version" \
    -v previous_version="$previous_version" \
    -v compare_base="$compare_base" '
    BEGIN {
      unreleased_done = 0
      release_link_done = 0
    }
    /^\[Unreleased\]: / {
      print "[Unreleased]: " compare_base "/compare/v" new_version "...HEAD"
      unreleased_done = 1
      next
    }
    $0 ~ ("^\\[" new_version "\\]: ") {
      release_link_done = 1
      print
      next
    }
    /^\[[0-9]+\.[0-9]+\.[0-9]+\]: / && !release_link_done {
      if (previous_version != "") {
        print "[" new_version "]: " compare_base "/compare/v" previous_version "...v" new_version
      } else {
        print "[" new_version "]: " compare_base "/releases/tag/v" new_version
      }
      release_link_done = 1
    }
    { print }
    END {
      if (!unreleased_done) {
        print "[Unreleased]: " compare_base "/compare/v" new_version "...HEAD"
      }
      if (!release_link_done) {
        if (previous_version != "") {
          print "[" new_version "]: " compare_base "/compare/v" previous_version "...v" new_version
        } else {
          print "[" new_version "]: " compare_base "/releases/tag/v" new_version
        }
      }
    }
  ' CHANGELOG.md > CHANGELOG.md.tmp

  mv CHANGELOG.md.tmp CHANGELOG.md
}

CURRENT_VERSION="$(current_version)"
PACKAGE_NAME="$(package_name)"
PREVIOUS_CHANGELOG_VERSION="$(latest_changelog_version)"
NEW_VERSION="$(bump_version "$CURRENT_VERSION" "$bump_type")"

validate_semver "$CURRENT_VERSION"
validate_semver "$NEW_VERSION"

if ! $force && ! $dry_run; then
  ensure_clean_tree
fi

log "Package: $PACKAGE_NAME"
log "Current: $CURRENT_VERSION"
log "New:     $NEW_VERSION"

if $dry_run; then
  log
  log "[dry-run] Would update: Cargo.toml to version $NEW_VERSION"
  log "[dry-run] Would update: CHANGELOG.md"
  log "[dry-run] Would run: cargo fmt --all"
  log "[dry-run] Would run: cargo clippy --all-targets -- -D warnings"
  if ! $skip_tests; then
    log "[dry-run] Would run: cargo test --all-targets"
  fi
  log "[dry-run] Would commit: chore(release): bump version to v$NEW_VERSION"
  $create_tag && log "[dry-run] Would create tag: v$NEW_VERSION"
  $push_after && log "[dry-run] Would push commit/tag to origin"
  exit 0
fi

insert_changelog_entry "$NEW_VERSION"
update_compare_links "$PREVIOUS_CHANGELOG_VERSION" "$NEW_VERSION"

  # Update the first `version = "..."` line (the [package] version). Uses awk,
  # not `sed -i` with a `0,/re/` address: that address is a GNU extension and is
  # silently a no-op on BSD/macOS sed, which left Cargo.toml unbumped.
  awk -v v="$NEW_VERSION" '
    BEGIN { done = 0 }
    !done && /^version = / { sub(/^version = ".*"/, "version = \"" v "\""); done = 1 }
    { print }
  ' Cargo.toml >Cargo.toml.tmp && mv Cargo.toml.tmp Cargo.toml
  # Keep Cargo.lock's own package version in sync so the lockfile is not left
  # pointing at the previous version after the bump.
  awk -v v="$NEW_VERSION" '
    /^name = "prview"$/ { print; getline; sub(/^version = ".*"/, "version = \"" v "\""); print; next }
    { print }
  ' Cargo.lock >Cargo.lock.tmp && mv Cargo.lock.tmp Cargo.lock

log
log "Running quality checks..."
cargo fmt --all
cargo clippy --all-targets -- -D warnings
if ! $skip_tests; then
  cargo test --all-targets
fi

git add Cargo.toml Cargo.lock CHANGELOG.md
git commit -m "chore(release): bump version to v$NEW_VERSION"

if $create_tag; then
  git tag -a "v$NEW_VERSION" -m "Release v$NEW_VERSION"
fi

if $push_after; then
  git push origin HEAD
  if $create_tag; then
    git push origin "v$NEW_VERSION"
  fi
fi

log
log "Version bump complete: $CURRENT_VERSION -> $NEW_VERSION"
