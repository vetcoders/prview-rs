#!/bin/sh
# prview installer — fail-closed.
#
# Downloads an official prview release binary, verifies it, and installs it to
# ~/.local/bin. Never uses sudo. There is NO source-build fallback: if anything
# cannot be verified, the script fails and installs nothing.
#
# What is verified, in order:
#   1. the platform is one of the two supported release targets;
#   2. the requested version resolves to a concrete release tag;
#   3. the archive and SHA256SUMS both download;
#   4. the archive matches its exact SHA256SUMS entry;
#   5. the archive contains exactly one regular file named `prview`;
#   6. on macOS: Developer ID signature, Team ID, and notarization — Gatekeeper
#      must report `source=Notarized Developer ID` for the binary;
#   7. the binary reports the expected version and a 40-hex build source SHA.
#
# Checks 6 and 7 execute the binary, so it is first staged inside the install
# directory as `.prview.<pid>.tmp` and every check runs against that staged
# path. The atomic `mv` into place is the last action of the run: until it
# happens, ${PRVIEW_INSTALL_DIR}/prview is untouched, and after it happens
# nothing is left that can fail.
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/vetcoders/prview-rs/main/install.sh | sh
#
# Environment:
#   PRVIEW_INSTALL_DIR   Install directory (default: ~/.local/bin). Never sudo.
#   PRVIEW_VERSION       Release to install: `latest` (default) or `X.Y.Z`/`vX.Y.Z`.
#   PRVIEW_BASE_URL      Release base URL (default:
#                        https://github.com/vetcoders/prview-rs/releases).
#                        Mirror/testing hook. A mirror that cannot emit HTTP
#                        redirects must publish `<base>/latest/VERSION`
#                        containing the tag so `latest` can be resolved.
#   PRVIEW_MACOS_TEAM_ID Expected Apple Team ID (default: MW223P3NPX). For forks
#                        that sign with their own Developer ID. It cannot skip
#                        the signature or notarization checks.
#   PRVIEW_TEST_UNAME_S  Test hook: override `uname -s` for platform detection.
#   PRVIEW_TEST_UNAME_M  Test hook: override `uname -m` for platform detection.
#                        Honoured ONLY when PRVIEW_BASE_URL points somewhere
#                        other than the official release base; against the
#                        official releases they are ignored with an info line.
#                        They only choose which asset is requested; every
#                        verification above still runs, and a binary for a
#                        foreign platform cannot pass step 7.
#
# Exit codes:
#   0  installed and verified
#   1  generic failure / missing tooling (curl|wget, sha256sum|shasum, tar)
#   2  unsupported platform
#   3  release artifact missing or download failed
#   4  checksum mismatch, malformed SHA256SUMS, or unsafe archive contents
#   5  macOS signature / Team ID / notarization verification failed
#   6  binary verification failed (version or build provenance)
#
set -eu

REPO="vetcoders/prview-rs"
BIN="prview"
DOCS_URL="https://github.com/vetcoders/prview-rs/blob/main/docs/INSTALL.md"
SUPPORTED_TARGETS="aarch64-apple-darwin (macOS arm64), x86_64-unknown-linux-gnu (glibc Linux x86_64)"

DEFAULT_BASE_URL="https://github.com/${REPO}/releases"

: "${PRVIEW_INSTALL_DIR:=${HOME}/.local/bin}"
: "${PRVIEW_VERSION:=latest}"
: "${PRVIEW_BASE_URL:=${DEFAULT_BASE_URL}}"
: "${PRVIEW_MACOS_TEAM_ID:=MW223P3NPX}"

INSTALL_DIR="${PRVIEW_INSTALL_DIR}"
BASE_URL="${PRVIEW_BASE_URL%/}"

OS=""
ARCH=""
TARGET=""
TAG=""
VERSION=""
BUILD_SOURCE_SHA=""
SHA_TOOL=""
TMPDIR_INSTALL=""
STAGED_TARGET=""

info() {
	printf '%s\n' "$*"
}

# fail <exit-code> <message...>
fail() {
	code="$1"
	shift
	printf 'error: %s\n' "$*" >&2
	exit "${code}"
}

cleanup() {
	if [ -n "${STAGED_TARGET}" ] && [ -f "${STAGED_TARGET}" ]; then
		rm -f "${STAGED_TARGET}"
	fi
	if [ -n "${TMPDIR_INSTALL}" ] && [ -d "${TMPDIR_INSTALL}" ]; then
		rm -rf "${TMPDIR_INSTALL}"
	fi
}
trap cleanup EXIT INT TERM

have() {
	command -v "$1" >/dev/null 2>&1
}

# --- platform -----------------------------------------------------------------

is_musl_linux() {
	if [ "${OS}" != "Linux" ]; then
		return 1
	fi
	if have getconf && getconf GNU_LIBC_VERSION >/dev/null 2>&1; then
		return 1
	fi
	if have ldd && ldd --version 2>&1 | grep -iq musl; then
		return 0
	fi
	for loader in /lib/ld-musl-*.so.1 /usr/lib/ld-musl-*.so.1; do
		if [ -e "${loader}" ]; then
			return 0
		fi
	done
	return 1
}

# Map the running platform to a released target triple. Anything without an
# official binary leaves TARGET empty, which is a hard failure (exit 2).
detect_target() {
	OS="$(uname -s)"
	ARCH="$(uname -m)"
	# The uname test hooks belong to the mirror/testing path only. Against the
	# official release base they are ignored, so a stray export can never make
	# a real user fetch and install an asset for a platform they are not on.
	if [ -n "${PRVIEW_TEST_UNAME_S:-}${PRVIEW_TEST_UNAME_M:-}" ]; then
		if [ "${BASE_URL}" = "${DEFAULT_BASE_URL}" ]; then
			info "ignoring PRVIEW_TEST_UNAME_S/PRVIEW_TEST_UNAME_M: they apply only with a non-default PRVIEW_BASE_URL"
		else
			OS="${PRVIEW_TEST_UNAME_S:-${OS}}"
			ARCH="${PRVIEW_TEST_UNAME_M:-${ARCH}}"
		fi
	fi
	case "${OS}" in
		Darwin)
			case "${ARCH}" in
				arm64 | aarch64) TARGET="aarch64-apple-darwin" ;;
				*) TARGET="" ;;
			esac
			;;
		Linux)
			case "${ARCH}" in
				x86_64 | amd64)
					if is_musl_linux; then
						TARGET=""
					else
						TARGET="x86_64-unknown-linux-gnu"
					fi
					;;
				*) TARGET="" ;;
			esac
			;;
		*)
			TARGET=""
			;;
	esac

	if [ -z "${TARGET}" ]; then
		fail 2 "unsupported platform ${OS}/${ARCH}.
prview publishes official binaries only for: ${SUPPORTED_TARGETS}.
This installer does not build from source. If you need another platform, see
${DOCS_URL} (it documents the unsupported 'cargo install prview' build path,
which this installer deliberately never runs for you)."
	fi
}

# --- download -----------------------------------------------------------------

download() {
	url="$1"
	dest="$2"
	if have curl; then
		curl -fsSL "${url}" -o "${dest}"
	elif have wget; then
		wget -q -O "${dest}" "${url}"
	else
		fail 1 "neither curl nor wget is available to download ${url}"
	fi
}

# Print the concrete tag behind `<base>/latest`, or nothing.
resolve_latest_tag() {
	location=""
	if have curl; then
		location="$(curl -fsS -o /dev/null -w '%{redirect_url}' "${BASE_URL}/latest" 2>/dev/null || true)"
	elif have wget; then
		location="$(wget -q --max-redirect=0 --server-response -O /dev/null "${BASE_URL}/latest" 2>&1 |
			awk 'tolower($1) == "location:" { print $2 }' | tail -1 || true)"
	fi
	case "${location}" in
		*/tag/*)
			printf '%s\n' "${location##*/tag/}"
			return 0
			;;
	esac
	# Mirrors (and file:// bases) that cannot redirect publish a tag marker.
	if download "${BASE_URL}/latest/VERSION" "${TMPDIR_INSTALL}/LATEST_VERSION" 2>/dev/null; then
		tr -d ' \t\r\n' <"${TMPDIR_INSTALL}/LATEST_VERSION"
		printf '\n'
		return 0
	fi
	return 0
}

resolve_version() {
	requested="${PRVIEW_VERSION}"
	if [ "${requested}" = "latest" ]; then
		requested="$(resolve_latest_tag)"
		if [ -z "${requested}" ]; then
			fail 3 "could not resolve the latest prview release tag from ${BASE_URL}/latest"
		fi
	fi
	case "${requested}" in
		v*) TAG="${requested}" ;;
		*) TAG="v${requested}" ;;
	esac
	VERSION="${TAG#v}"
	if [ -z "${VERSION}" ]; then
		fail 3 "empty release version resolved from ${BASE_URL}"
	fi
}

# --- verification -------------------------------------------------------------

select_sha_tool() {
	if have sha256sum; then
		SHA_TOOL="sha256sum"
	elif have shasum; then
		SHA_TOOL="shasum"
	else
		fail 1 "no SHA-256 tool found (need sha256sum or shasum); refusing to install an unverified binary"
	fi
}

sha256_of() {
	if [ "${SHA_TOOL}" = "sha256sum" ]; then
		sha256sum "$1" | awk '{print $1}'
	else
		shasum -a 256 "$1" | awk '{print $1}'
	fi
}

# Exact-match the archive against its SHA256SUMS entry. Any doubt is exit 4.
verify_checksum() {
	dir="$1"
	archive="$2"
	sums="${dir}/SHA256SUMS"

	if [ ! -s "${sums}" ]; then
		fail 4 "SHA256SUMS for ${TAG} is empty or missing; refusing to install ${archive}"
	fi
	if ! grep -Eq '^[0-9a-fA-F]{64}[[:blank:]]+\*?[^[:blank:]]+$' "${sums}"; then
		fail 4 "SHA256SUMS for ${TAG} is malformed (no '<sha256>  <filename>' lines); refusing to install ${archive}"
	fi

	matches="$(awk -v f="${archive}" '$2 == f || $2 == "*" f { print $1 }' "${sums}")"
	count="$(printf '%s' "${matches}" | grep -c . || true)"
	if [ "${count}" != "1" ]; then
		fail 4 "SHA256SUMS for ${TAG} has ${count} entries for ${archive}; expected exactly one"
	fi

	actual="$(sha256_of "${dir}/${archive}")"
	if [ "${matches}" != "${actual}" ]; then
		fail 4 "checksum mismatch for ${archive}: expected ${matches}, got ${actual}"
	fi
}

# The archive must hold exactly one regular file named `prview`.
extract_archive() {
	dir="$1"
	archive="$2"
	extract_dir="${dir}/extract"

	if ! have tar; then
		fail 1 "tar is not available to unpack ${archive}"
	fi

	listing="$(tar tzf "${dir}/${archive}" 2>/dev/null)" ||
		fail 4 "${archive} is not a readable gzip tar archive"
	if [ "${listing}" != "${BIN}" ]; then
		fail 4 "${archive} must contain exactly one entry named '${BIN}'; it contains: $(printf '%s' "${listing}" | tr '\n' ' ')"
	fi

	verbose_line="$(tar tvzf "${dir}/${archive}" 2>/dev/null | head -1)"
	case "${verbose_line}" in
		-*) ;;
		*) fail 4 "${archive} entry '${BIN}' is not a regular file (${verbose_line})" ;;
	esac

	mkdir -p "${extract_dir}" || fail 1 "could not create ${extract_dir}"
	tar xzf "${dir}/${archive}" -C "${extract_dir}" || fail 4 "could not unpack ${archive}"

	if [ -L "${extract_dir}/${BIN}" ] || [ ! -f "${extract_dir}/${BIN}" ]; then
		fail 4 "${archive} did not yield a regular file at ${extract_dir}/${BIN}"
	fi
	chmod 755 "${extract_dir}/${BIN}" || fail 1 "could not make ${extract_dir}/${BIN} executable"
}

# Developer ID signature, Team ID, and notarization. No bypass environment.
verify_macos_identity() {
	binary="$1"
	if ! have codesign; then
		fail 5 "codesign is not available; cannot verify the macOS signature of ${BIN}"
	fi
	if ! codesign --verify --strict --verbose=2 "${binary}" >/dev/null 2>&1; then
		fail 5 "macOS code signature verification failed for ${TAG} (codesign --verify --strict).
Releases up to and including v0.7.0 were never signed and are rejected by design.
For a newer release, this means a corrupted or tampered download — re-download it from the official release page rather than bypassing this check. See ${DOCS_URL}."
	fi
	signing_info="$(codesign -dv --verbose=2 "${binary}" 2>&1 || true)"
	if ! printf '%s\n' "${signing_info}" | grep -q "^TeamIdentifier=${PRVIEW_MACOS_TEAM_ID}$"; then
		fail 5 "macOS Team ID mismatch for ${TAG}: expected ${PRVIEW_MACOS_TEAM_ID}, got $(printf '%s\n' "${signing_info}" | grep '^TeamIdentifier=' || printf 'none')"
	fi
	if ! have spctl; then
		fail 5 "spctl is not available; cannot verify notarization of ${BIN}"
	fi
	# `spctl --assess --type execute` cannot answer this question: it rejects
	# every standalone executable, Apple's own /bin/ls included, with "the code
	# is valid but does not seem to be an app". The assessment that does is the
	# primary-signature one, whose `source=` line names where the verdict comes
	# from: a notarized Developer ID build reports `source=Notarized Developer
	# ID`, one signed with a Developer ID but never notarized reports plain
	# `source=Developer ID`, and an ad-hoc or unsigned binary is rejected with no
	# `source=` line at all. Only the first is accepted. The verdict lines go to
	# stderr, so stdout and stderr are captured together.
	notarization_status=0
	notarization_info="$(spctl -a -t open --context context:primary-signature -vv "${binary}" 2>&1)" ||
		notarization_status=$?
	notarization_source="$(printf '%s\n' "${notarization_info}" | grep '^source=' || true)"
	if [ "${notarization_status}" != "0" ] || [ "${notarization_source}" != "source=Notarized Developer ID" ]; then
		fail 5 "notarization check failed for ${TAG}: Gatekeeper reported '${notarization_source:-no source line}', expected 'source=Notarized Developer ID'.
Only a notarized Developer ID build is accepted; a signed-but-not-notarized or
ad-hoc binary is rejected by design. See ${DOCS_URL}."
	fi
}

# Run the binary and confirm it is the official build we resolved.
verify_binary() {
	binary="$1"
	stage="$2"

	reported="$("${binary}" --version 2>/dev/null)" ||
		fail 6 "${stage}: ${binary} --version did not run successfully"
	if [ "${reported}" != "${BIN} ${VERSION}" ]; then
		fail 6 "${stage}: version mismatch — expected '${BIN} ${VERSION}', got '${reported}'"
	fi

	source_sha="$("${binary}" --build-source-sha 2>/dev/null)" ||
		fail 6 "${stage}: ${binary} --build-source-sha did not run successfully"
	# grep is line-oriented, so reject multi-line output before matching: a
	# binary printing "unknown\n<40 hex>" must not satisfy this check.
	if [ "$(printf '%s\n' "${source_sha}" | wc -l | tr -d ' ')" != "1" ] ||
		! printf '%s' "${source_sha}" | grep -Eq '^[0-9a-f]{40}$'; then
		fail 6 "${stage}: build provenance missing — --build-source-sha reported '${source_sha}' instead of a 40-hex commit.
Releases published before build provenance was recorded are rejected by design; see ${DOCS_URL}."
	fi
	BUILD_SOURCE_SHA="${source_sha}"
}

# --- install ------------------------------------------------------------------

# Copy the unpacked binary to a private path inside the install directory. The
# checks that execute the binary then run from there, which also survives a
# `noexec` $TMPDIR. The path is unique to this process, and the only file this
# script ever reads back is the one it just wrote: a stale `.prview.*.tmp` left
# by an earlier crashed run is never picked up, never executed, and never
# installed. Nothing outside this path is touched until finalize_install.
stage_binary() {
	source_binary="$1"
	mkdir -p "${INSTALL_DIR}" || fail 1 "could not create ${INSTALL_DIR}"
	STAGED_TARGET="${INSTALL_DIR}/.${BIN}.$$.tmp"
	# Remove, never write through, anything already sitting at that path — a
	# leftover symlink must not redirect the staged copy somewhere else.
	rm -f "${STAGED_TARGET}" || fail 1 "could not clear the stale staging path ${STAGED_TARGET}"
	install -m 755 "${source_binary}" "${STAGED_TARGET}" ||
		fail 1 "could not stage ${BIN} in ${INSTALL_DIR} (is it writable?)"
}

# The last mutation of the run: every check has already passed against the
# staged file, so this rename is the single moment the install directory's
# `prview` changes, and nothing after it can fail.
finalize_install() {
	mv -f "${STAGED_TARGET}" "${INSTALL_DIR}/${BIN}" ||
		fail 1 "could not install ${BIN} into ${INSTALL_DIR}"
	STAGED_TARGET=""
}

path_guidance() {
	case ":${PATH}:" in
		*":${INSTALL_DIR}:"*)
			return 0
			;;
	esac
	info ""
	info "${INSTALL_DIR} is not on your PATH. Add it:"
	info ""
	info "  # zsh (~/.zshrc)"
	info "  echo 'export PATH=\"${INSTALL_DIR}:\$PATH\"' >> \"\${HOME}/.zshrc\""
	info ""
	info "  # bash (~/.bashrc)"
	info "  echo 'export PATH=\"${INSTALL_DIR}:\$PATH\"' >> \"\${HOME}/.bashrc\""
	info ""
	info "Then restart your shell or run: export PATH=\"${INSTALL_DIR}:\$PATH\""
}

main() {
	detect_target
	select_sha_tool

	TMPDIR_INSTALL="$(mktemp -d 2>/dev/null)" || fail 1 "could not create a temporary directory"

	resolve_version
	archive="${BIN}-${TARGET}.tar.gz"
	release_url="${BASE_URL}/download/${TAG}"

	info "Installing ${BIN} ${VERSION} for ${TARGET}..."

	download "${release_url}/${archive}" "${TMPDIR_INSTALL}/${archive}" ||
		fail 3 "no official artifact for ${TARGET} at ${release_url}/${archive}"
	download "${release_url}/SHA256SUMS" "${TMPDIR_INSTALL}/SHA256SUMS" ||
		fail 3 "no official artifact for ${TARGET} at ${release_url}/SHA256SUMS"

	verify_checksum "${TMPDIR_INSTALL}" "${archive}"
	extract_archive "${TMPDIR_INSTALL}" "${archive}"

	stage_binary "${TMPDIR_INSTALL}/extract/${BIN}"

	if [ "${TARGET}" = "aarch64-apple-darwin" ]; then
		verify_macos_identity "${STAGED_TARGET}"
	fi
	verify_binary "${STAGED_TARGET}" "staged binary"

	finalize_install

	info ""
	info "${BIN} ${VERSION} installed to ${INSTALL_DIR}/${BIN}"
	info "source commit: ${BUILD_SOURCE_SHA}"
	path_guidance
}

main "$@"
