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

MOCK_BIN="$TMP/bin"
mkdir -p "$MOCK_BIN"

cat >"$MOCK_BIN/gh" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
if [[ "$1" == api ]]; then
  case "$MOCK_GH_MODE" in
    success) cat "$MOCK_RELEASE_JSON" ;;
    missing)
      echo 'gh: Not Found (HTTP 404)' >&2
      exit 1
      ;;
    error)
      echo 'gh: service unavailable (HTTP 503)' >&2
      exit 1
      ;;
    *) exit 64 ;;
  esac
elif [[ "$1" == release && "$2" == download ]]; then
  while (($#)); do
    if [[ "$1" == --dir ]]; then
      shift
      DEST=$1
      break
    fi
    shift
  done
  : "${DEST:?mock gh download requires --dir}"
  cp "$MOCK_RELEASE_ASSET_DIR"/* "$DEST/"
else
  exit 64
fi
EOF

cat >"$MOCK_BIN/curl" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
OUTPUT=
URL=
while (($#)); do
  case "$1" in
    -o)
      shift
      OUTPUT=$1
      ;;
    http*) URL=$1 ;;
  esac
  shift
done
: "${URL:?mock curl did not receive a URL}"

ATTEMPT=$(cat "$MOCK_CRATE_STATE" 2>/dev/null || echo 0)
if [[ "$URL" != */download ]]; then
  ATTEMPT=$((ATTEMPT + 1))
  echo "$ATTEMPT" >"$MOCK_CRATE_STATE"
  [[ "$MOCK_CRATE_MODE" != metadata_fail ]] || exit 22
  CHECKSUM=$MOCK_GOOD_CHECKSUM
  if [[ "$MOCK_CRATE_MODE" == source_fail && "$ATTEMPT" -eq 1 ]]; then
    CHECKSUM=$MOCK_BAD_SOURCE_CHECKSUM
  elif [[ "$MOCK_CRATE_MODE" == tar_fail && "$ATTEMPT" -eq 1 ]]; then
    CHECKSUM=$MOCK_NOT_TAR_CHECKSUM
  fi
  printf '{"version":{"num":"0.8.1","checksum":"%s"}}\n' "$CHECKSUM"
  exit 0
fi

case "$MOCK_CRATE_MODE:$ATTEMPT" in
  download_fail:1) exit 22 ;;
  checksum_fail:1) printf 'corrupt download' >"$OUTPUT" ;;
  source_fail:1) cp "$MOCK_BAD_SOURCE_CRATE" "$OUTPUT" ;;
  tar_fail:1) cp "$MOCK_NOT_TAR" "$OUTPUT" ;;
  *) cp "$MOCK_GOOD_CRATE" "$OUTPUT" ;;
esac
EOF
chmod +x "$MOCK_BIN/gh" "$MOCK_BIN/curl"

TARGETS='x86_64-unknown-linux-gnu aarch64-apple-darwin'
make_release_assets() {
  local dir=$1
  mkdir -p "$dir"
  printf linux >"$dir/prview-x86_64-unknown-linux-gnu.tar.gz"
  printf macos >"$dir/prview-aarch64-apple-darwin.tar.gz"
  (
    cd "$dir"
    sha256sum prview-*.tar.gz >SHA256SUMS
  )
}

write_release_json() {
  local dir=$1
  local json=$2
  find "$dir" -maxdepth 1 -type f -exec basename {} \; \
    | LC_ALL=C sort \
    | jq -Rsc 'split("\n") | map(select(length > 0) | {name: .})' \
    | jq '{tag_name:"v0.8.1", draft:false, assets:.}' >"$json"
}

run_existing_release() {
  local case_dir=$1
  local mode=$2
  : >"$case_dir/output"
  : >"$case_dir/summary"
  env PATH="$MOCK_BIN:$PATH" \
    MOCK_GH_MODE="$mode" \
    MOCK_RELEASE_JSON="$case_dir/release.json" \
    MOCK_RELEASE_ASSET_DIR="$case_dir/assets" \
    TAG=v0.8.1 \
    GITHUB_REPOSITORY=vetcoders/prview-rs \
    PRVIEW_RELEASE_TARGETS="$TARGETS" \
    GITHUB_OUTPUT="$case_dir/output" \
    GITHUB_STEP_SUMMARY="$case_dir/summary" \
    "$ROOT/tools/verify-existing-release.sh"
}

MISSING="$TMP/release-missing"
mkdir -p "$MISSING/assets"
run_existing_release "$MISSING" missing
grep -Fxq 'exists=false' "$MISSING/output" \
  || fail "a 404 was not classified as an absent release"

API_ERROR="$TMP/release-api-error"
mkdir -p "$API_ERROR/assets"
if run_existing_release "$API_ERROR" error >/dev/null 2>&1; then
  fail "a release API error was treated as absence"
fi

VALID="$TMP/release-valid"
make_release_assets "$VALID/assets"
write_release_json "$VALID/assets" "$VALID/release.json"
run_existing_release "$VALID" success
grep -Fxq 'exists=true' "$VALID/output" \
  || fail "a valid existing release was not accepted"

MISSING_ASSET="$TMP/release-missing-asset"
make_release_assets "$MISSING_ASSET/assets"
find "$MISSING_ASSET/assets" -name 'prview-aarch64-apple-darwin.tar.gz' -delete
write_release_json "$MISSING_ASSET/assets" "$MISSING_ASSET/release.json"
if run_existing_release "$MISSING_ASSET" success >/dev/null 2>&1; then
  fail "an existing release with a missing asset was accepted"
fi

EXTRA_ASSET="$TMP/release-extra-asset"
make_release_assets "$EXTRA_ASSET/assets"
printf unexpected >"$EXTRA_ASSET/assets/unexpected.zip"
write_release_json "$EXTRA_ASSET/assets" "$EXTRA_ASSET/release.json"
if run_existing_release "$EXTRA_ASSET" success >/dev/null 2>&1; then
  fail "an existing release with an extra asset was accepted"
fi

CORRUPT_ASSET="$TMP/release-corrupt-asset"
make_release_assets "$CORRUPT_ASSET/assets"
printf tampered >>"$CORRUPT_ASSET/assets/prview-x86_64-unknown-linux-gnu.tar.gz"
write_release_json "$CORRUPT_ASSET/assets" "$CORRUPT_ASSET/release.json"
if run_existing_release "$CORRUPT_ASSET" success >/dev/null 2>&1; then
  fail "an existing release with a checksum mismatch was accepted"
fi

MISSING_CHECKSUM="$TMP/release-missing-checksum"
make_release_assets "$MISSING_CHECKSUM/assets"
sed -n '1p' "$MISSING_CHECKSUM/assets/SHA256SUMS" \
  >"$MISSING_CHECKSUM/assets/SHA256SUMS.partial"
mv "$MISSING_CHECKSUM/assets/SHA256SUMS.partial" \
  "$MISSING_CHECKSUM/assets/SHA256SUMS"
write_release_json "$MISSING_CHECKSUM/assets" "$MISSING_CHECKSUM/release.json"
if run_existing_release "$MISSING_CHECKSUM" success >/dev/null 2>&1; then
  fail "a checksum manifest missing an expected archive was accepted"
fi

EXTRA_CHECKSUM="$TMP/release-extra-checksum"
make_release_assets "$EXTRA_CHECKSUM/assets"
printf '%064d  prview-unexpected-target.tar.gz\n' 0 \
  >>"$EXTRA_CHECKSUM/assets/SHA256SUMS"
write_release_json "$EXTRA_CHECKSUM/assets" "$EXTRA_CHECKSUM/release.json"
if run_existing_release "$EXTRA_CHECKSUM" success >/dev/null 2>&1; then
  fail "a checksum manifest with an extra archive entry was accepted"
fi

DUPLICATE_CHECKSUM="$TMP/release-duplicate-checksum"
make_release_assets "$DUPLICATE_CHECKSUM/assets"
FIRST_CHECKSUM_LINE=$(sed -n '1p' "$DUPLICATE_CHECKSUM/assets/SHA256SUMS")
printf '%s\n' "$FIRST_CHECKSUM_LINE" >>"$DUPLICATE_CHECKSUM/assets/SHA256SUMS"
write_release_json "$DUPLICATE_CHECKSUM/assets" "$DUPLICATE_CHECKSUM/release.json"
if run_existing_release "$DUPLICATE_CHECKSUM" success >/dev/null 2>&1; then
  fail "a checksum manifest with a duplicate archive entry was accepted"
fi

GOOD_SHA=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
BAD_SHA=bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb
CRATES="$TMP/crates"
mkdir -p "$CRATES/good/prview-0.8.1" "$CRATES/bad/prview-0.8.1"
printf '{"git":{"sha1":"%s"}}\n' "$GOOD_SHA" \
  >"$CRATES/good/prview-0.8.1/.cargo_vcs_info.json"
printf '{"git":{"sha1":"%s"}}\n' "$BAD_SHA" \
  >"$CRATES/bad/prview-0.8.1/.cargo_vcs_info.json"
tar czf "$CRATES/good.crate" -C "$CRATES/good" prview-0.8.1
tar czf "$CRATES/bad-source.crate" -C "$CRATES/bad" prview-0.8.1
printf 'not a tar archive' >"$CRATES/not-tar.crate"
GOOD_CHECKSUM=$(sha256sum "$CRATES/good.crate" | awk '{print $1}')
BAD_SOURCE_CHECKSUM=$(sha256sum "$CRATES/bad-source.crate" | awk '{print $1}')
NOT_TAR_CHECKSUM=$(sha256sum "$CRATES/not-tar.crate" | awk '{print $1}')

run_crates_case() {
  local mode=$1
  local expected_attempts=$2
  local case_dir="$TMP/crates-$mode"
  mkdir -p "$case_dir"
  : >"$case_dir/state"
  : >"$case_dir/summary"
  (
    cd "$case_dir"
    env PATH="$MOCK_BIN:$PATH" \
      MOCK_CRATE_MODE="$mode" \
      MOCK_CRATE_STATE="$case_dir/state" \
      MOCK_GOOD_CRATE="$CRATES/good.crate" \
      MOCK_BAD_SOURCE_CRATE="$CRATES/bad-source.crate" \
      MOCK_NOT_TAR="$CRATES/not-tar.crate" \
      MOCK_GOOD_CHECKSUM="$GOOD_CHECKSUM" \
      MOCK_BAD_SOURCE_CHECKSUM="$BAD_SOURCE_CHECKSUM" \
      MOCK_NOT_TAR_CHECKSUM="$NOT_TAR_CHECKSUM" \
      VERSION=v0.8.1 \
      PRVIEW_RELEASE_SHA="$GOOD_SHA" \
      PRVIEW_CRATES_VERIFY_ATTEMPTS=3 \
      PRVIEW_CRATES_VERIFY_DELAY_SECONDS=0 \
      GITHUB_STEP_SUMMARY="$case_dir/summary" \
      "$ROOT/tools/verify-crates-publication.sh"
  )
  [[ "$(cat "$case_dir/state")" == "$expected_attempts" ]] \
    || fail "$mode did not use $expected_attempts attempts"
  grep -Fq 'crates.io v0.8.1: verified' "$case_dir/summary" \
    || fail "$mode did not finish with a verified crate"
}

run_crates_case download_fail 2
run_crates_case checksum_fail 2
run_crates_case source_fail 2
run_crates_case tar_fail 2

NEVER="$TMP/crates-never"
mkdir -p "$NEVER"
: >"$NEVER/state"
: >"$NEVER/summary"
if (
  cd "$NEVER"
  env PATH="$MOCK_BIN:$PATH" \
    MOCK_CRATE_MODE=metadata_fail \
    MOCK_CRATE_STATE="$NEVER/state" \
    MOCK_GOOD_CRATE="$CRATES/good.crate" \
    MOCK_BAD_SOURCE_CRATE="$CRATES/bad-source.crate" \
    MOCK_NOT_TAR="$CRATES/not-tar.crate" \
    MOCK_GOOD_CHECKSUM="$GOOD_CHECKSUM" \
    MOCK_BAD_SOURCE_CHECKSUM="$BAD_SOURCE_CHECKSUM" \
    MOCK_NOT_TAR_CHECKSUM="$NOT_TAR_CHECKSUM" \
    VERSION=v0.8.1 \
    PRVIEW_RELEASE_SHA="$GOOD_SHA" \
    PRVIEW_CRATES_VERIFY_ATTEMPTS=3 \
    PRVIEW_CRATES_VERIFY_DELAY_SECONDS=0 \
    GITHUB_STEP_SUMMARY="$NEVER/summary" \
    "$ROOT/tools/verify-crates-publication.sh"
) >/dev/null 2>&1; then
  fail "exhausted crates.io retries returned success"
fi
[[ "$(cat "$NEVER/state")" == 3 ]] \
  || fail "crates.io retry exhaustion did not consume all attempts"

echo "Release publication recovery tests passed."
