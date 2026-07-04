#!/bin/sh
# prview installer
#
# Downloads the latest prview release binary, verifies its SHA-256 checksum,
# and installs it to ~/.local/bin. Never uses sudo. Falls back to
# `cargo install prview --locked --force` when a prebuilt binary is not
# available for this platform (or the download fails).
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/vetcoders/prview-rs/main/install.sh | sh
#
# Environment:
#   PRVIEW_INSTALL_DIR   Override the install directory (default: ~/.local/bin).
#
set -eu

REPO="vetcoders/prview-rs"
BIN="prview"
: "${PRVIEW_INSTALL_DIR:=${HOME}/.local/bin}"
INSTALL_DIR="${PRVIEW_INSTALL_DIR}"
BASE_URL="https://github.com/${REPO}/releases/latest/download"

OS=""
ARCH=""
TARGET=""
INSTALLED_BIN=""
TMPDIR_INSTALL=""

info() {
	printf '%s\n' "$*"
}

err() {
	printf 'error: %s\n' "$*" >&2
	exit 1
}

cleanup() {
	if [ -n "${TMPDIR_INSTALL}" ] && [ -d "${TMPDIR_INSTALL}" ]; then
		rm -rf "${TMPDIR_INSTALL}"
	fi
}
trap cleanup EXIT INT TERM

# Map the running platform to a released target triple. Leaves TARGET empty
# for anything without a prebuilt binary so the caller can fall back to cargo.
detect_target() {
	OS="$(uname -s)"
	ARCH="$(uname -m)"
	case "${OS}" in
		Darwin)
			case "${ARCH}" in
				arm64 | aarch64) TARGET="aarch64-apple-darwin" ;;
				*) TARGET="" ;;
			esac
			;;
		Linux)
			case "${ARCH}" in
				x86_64 | amd64) TARGET="x86_64-unknown-linux-gnu" ;;
				*) TARGET="" ;;
			esac
			;;
		*)
			TARGET=""
			;;
	esac
}

download() {
	url="$1"
	dest="$2"
	if command -v curl >/dev/null 2>&1; then
		curl -fsSL "${url}" -o "${dest}"
	elif command -v wget >/dev/null 2>&1; then
		wget -qO "${dest}" "${url}"
	else
		err "neither curl nor wget is available to download ${url}"
	fi
}

sha256_of() {
	file="$1"
	if command -v sha256sum >/dev/null 2>&1; then
		sha256sum "${file}" | awk '{print $1}'
	elif command -v shasum >/dev/null 2>&1; then
		shasum -a 256 "${file}" | awk '{print $1}'
	else
		return 1
	fi
}

# Verify the archive against the published SHA256SUMS. Returns non-zero on any
# problem so the caller can fall back rather than install an unverified binary.
verify_checksum() {
	dir="$1"
	archive="$2"
	expected="$(grep " ${archive}\$" "${dir}/SHA256SUMS" | awk '{print $1}' | head -1)"
	if [ -z "${expected}" ]; then
		info "checksum: no SHA256SUMS entry for ${archive}"
		return 1
	fi
	actual="$(sha256_of "${dir}/${archive}")" || {
		info "checksum: no sha256 tool (sha256sum/shasum) available"
		return 1
	}
	if [ "${expected}" != "${actual}" ]; then
		info "checksum mismatch for ${archive}: expected ${expected}, got ${actual}"
		return 1
	fi
}

# Download, verify, and unpack the release binary into INSTALL_DIR.
# Returns non-zero on any failure so main() can fall back to cargo.
install_from_release() {
	archive="prview-${TARGET}.tar.gz"
	TMPDIR_INSTALL="$(mktemp -d 2>/dev/null)" || return 1

	download "${BASE_URL}/${archive}" "${TMPDIR_INSTALL}/${archive}" || return 1
	download "${BASE_URL}/SHA256SUMS" "${TMPDIR_INSTALL}/SHA256SUMS" || return 1

	verify_checksum "${TMPDIR_INSTALL}" "${archive}" || return 1

	mkdir -p "${INSTALL_DIR}" || return 1
	tar xzf "${TMPDIR_INSTALL}/${archive}" -C "${INSTALL_DIR}" || return 1
	chmod +x "${INSTALL_DIR}/${BIN}" || return 1

	INSTALLED_BIN="${INSTALL_DIR}/${BIN}"
	return 0
}

cargo_fallback() {
	if ! command -v cargo >/dev/null 2>&1; then
		err "no prebuilt binary for ${OS}/${ARCH} and cargo is not installed.
Install Rust from https://rustup.rs then re-run this script, or run:
  cargo install prview --locked --force"
	fi
	info "Installing prview via cargo (this compiles from source)..."
	cargo install prview --locked --force
	cargo_root="${CARGO_INSTALL_ROOT:-${CARGO_HOME:-${HOME}/.cargo}}"
	INSTALL_DIR="${cargo_root}/bin"
	INSTALLED_BIN="${INSTALL_DIR}/${BIN}"
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

post_install() {
	info ""
	if [ -x "${INSTALLED_BIN}" ]; then
		"${INSTALLED_BIN}" --version
	elif command -v "${BIN}" >/dev/null 2>&1; then
		"${BIN}" --version
	fi
	info "prview installed to ${INSTALL_DIR}/${BIN}"
	path_guidance
}

main() {
	detect_target
	if [ -n "${TARGET}" ] && install_from_release; then
		:
	else
		if [ -z "${TARGET}" ]; then
			info "No prebuilt prview binary for ${OS}/${ARCH}; falling back to cargo."
		else
			info "Release download/verify failed; falling back to cargo."
		fi
		cargo_fallback
	fi
	post_install
}

main "$@"
