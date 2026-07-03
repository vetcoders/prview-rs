# prview Build System
# Local developer flow + release helpers

.PHONY: all build install install-bin install-cargo git-hooks
.PHONY: precommit precheck test check fmt fmt-check clippy semgrep ci
.PHONY: version version-show version-check release-plan release-check release-tag release-push publish-checklist
.PHONY: release-gate clean help unlock

# Default target
all: build

.PHONY: smoke-test
smoke-test:
	@echo "=== Smoke Test (Installation Verification) ==="
	@if ! command -v prview >/dev/null 2>&1; then \
		echo "prview binary not found in PATH. Ensure $(CARGO_BIN) is in your PATH."; \
		exit 1; \
	fi
	@echo "Checking version..."
	@prview --version
	@echo "Checking doctor..."
	@prview doctor
	@echo "=== Smoke Test Passed ==="

# Determine cargo bin dir
CARGO_BIN ?= $(if $(CARGO_HOME),$(CARGO_HOME)/bin,$(HOME)/.cargo/bin)

LOCK_ROOT ?= $(if $(TMPDIR),$(TMPDIR),/tmp)
LOCK_USER ?= $(shell id -u 2>/dev/null || printf unknown)
LOCKDIR ?= $(LOCK_ROOT)/prview-make-$(LOCK_USER).lock

HOOK_SETUP_SCRIPT := ./tools/setup_hooks.sh
SEMVER_SCRIPT := ./tools/semver.sh
VERSION_BUMP_SCRIPT := ./tools/version-bump.sh
PUBLISH_CHECKLIST_SCRIPT := ./tools/publish-checklist.sh
RELEASE_GATE_SCRIPT := ./tools/release-gate.sh

TYPE ?= patch

# Build release
build:
	cargo build --release

# Install prview binary with atomic lock (mkdir is atomic on all POSIX systems)
install: install-bin git-hooks
	@echo "Installed: prview + git hooks"
	@echo "Run 'make smoke-test' to verify installation."

install-bin:
	@if ! mkdir "$(LOCKDIR)" 2>/dev/null; then \
		old_pid=$$(cat "$(LOCKDIR)/pid" 2>/dev/null); \
		if [ -n "$$old_pid" ] && kill -0 "$$old_pid" 2>/dev/null; then \
			echo "Another build running (PID $$old_pid). Aborting."; \
			exit 1; \
		fi; \
		echo "Removing stale lock (PID $$old_pid dead)"; \
		rm -rf "$(LOCKDIR)"; \
		mkdir "$(LOCKDIR)"; \
	fi
	@echo $$$$ > "$(LOCKDIR)/pid"
	@trap 'rm -rf "$(LOCKDIR)"' EXIT; \
	set -e; \
	cargo build --release; \
	mkdir -p "$(CARGO_BIN)"; \
	install -m 755 target/release/prview "$(CARGO_BIN)/prview"; \
	echo "Installed: prview → $(CARGO_BIN)"

# Install with cargo install (alternative method)
install-cargo:
	cargo install --path . --force
	@echo "Installed via cargo: prview → $(CARGO_BIN)"

# Install local git hooks
git-hooks:
	@chmod +x "$(HOOK_SETUP_SCRIPT)" tools/githooks/pre-commit tools/githooks/pre-push
	@"$(HOOK_SETUP_SCRIPT)"

# Fast pre-commit gate (safe to run manually)
precommit:
	@echo "=== Pre-commit (fast Rust gate) ==="
	@STAGED_RS=$$(git diff --cached --name-only --diff-filter=ACM | grep '\.rs$$' || true); \
	if [ -z "$$STAGED_RS" ]; then \
		echo "No staged Rust files. Skipping Rust pre-commit checks."; \
		exit 0; \
	fi; \
	echo "[1/2] Checking formatting..."; \
	cargo fmt --all --check || { \
		echo "Formatting issues. Run 'make fmt' to fix them."; \
		exit 1; \
	}; \
	echo "[2/2] Running cargo check..."; \
	cargo check --all-targets --quiet

# Quick compile gate
precheck:
	cargo check --all-targets

# Run tests
test:
	cargo test --all-targets

# Full quality gate (format + clippy + tests + release build + semgrep)
check:
	@echo "=== Quality Gate ==="
	@echo "[1/5] Checking formatting..."
	@cargo fmt --all --check || (echo "Run 'make fmt' to fix formatting." && exit 1)
	@echo "[2/5] Running clippy..."
	@cargo clippy --all-targets -- -D warnings
	@echo "[3/5] Running tests..."
	@cargo test --all-targets
	@echo "[4/5] Checking release build..."
	@cargo build --release
	@echo "[5/5] Running Semgrep (if available)..."
	@if command -v semgrep >/dev/null 2>&1 || command -v pipx >/dev/null 2>&1; then \
		SEMGREP=$$(command -v semgrep || echo "pipx run semgrep"); \
		$$SEMGREP scan --config semgrep.yml --error --quiet . --exclude target; \
	else \
		echo "[!] Semgrep not available, skipping (install: pipx install semgrep)"; \
	fi
	@echo "=== All checks passed ==="

# Format code
fmt:
	cargo fmt --all

fmt-check:
	cargo fmt --all --check

# Run clippy
clippy:
	cargo clippy --all-targets -- -D warnings

# Optional standalone Semgrep run
semgrep:
	@if command -v semgrep >/dev/null 2>&1 || command -v pipx >/dev/null 2>&1; then \
		SEMGREP=$$(command -v semgrep || echo "pipx run semgrep"); \
		$$SEMGREP scan --config semgrep.yml --error --quiet . --exclude target; \
	else \
		echo "[!] Semgrep not available, skipping (install: pipx install semgrep)"; \
	fi

# Alias for local CI parity
ci: check
	@echo "CI-equivalent local checks passed."

# SemVer + release helpers
version:
	@chmod +x "$(VERSION_BUMP_SCRIPT)"
	@"$(VERSION_BUMP_SCRIPT)" $(if $(SET),--set $(SET),--$(TYPE)) $(if $(DRY),--dry-run) $(if $(FORCE),--force) $(if $(NO_TEST),--no-test) $(if $(TAG),--tag) $(if $(PUSH),--push)

version-show:
	@chmod +x "$(SEMVER_SCRIPT)"
	@"$(SEMVER_SCRIPT)" show

version-check:
	@chmod +x "$(SEMVER_SCRIPT)"
	@"$(SEMVER_SCRIPT)" check --allow-dirty --allow-existing-tag

release-plan:
	@chmod +x "$(SEMVER_SCRIPT)"
	@"$(SEMVER_SCRIPT)" plan

release-check:
	@chmod +x "$(SEMVER_SCRIPT)"
	@"$(SEMVER_SCRIPT)" check
	@echo "[extra] Verifying release package..."
	@cargo package --locked
	@$(MAKE) check
	@echo "Release readiness passed."

release-tag:
	@chmod +x "$(SEMVER_SCRIPT)"
	@"$(SEMVER_SCRIPT)" tag

release-push:
	@chmod +x "$(SEMVER_SCRIPT)"
	@"$(SEMVER_SCRIPT)" push

publish-checklist:
	@chmod +x "$(PUBLISH_CHECKLIST_SCRIPT)"
	@"$(PUBLISH_CHECKLIST_SCRIPT)"

# Full pre-release verification gate
release-gate:
	@chmod +x "$(RELEASE_GATE_SCRIPT)"
	@"$(RELEASE_GATE_SCRIPT)"

# Clean build artifacts
clean:
	cargo clean

# Remove stale build lock
unlock:
	@rm -rf "$(LOCKDIR)" && echo "Lock removed" || echo "No lock"

# Help
help:
	@echo "prview Build System"
	@echo ""
	@echo "Core Commands:"
	@echo "  make build           - Build release binary"
	@echo "  make install         - Install prview + local git hooks"
	@echo "  make install-bin     - Install only the prview binary"
	@echo "  make install-cargo   - Install via cargo install --path ."
	@echo "  make git-hooks       - Install pre-commit + pre-push hooks"
	@echo "  make precommit       - Fast staged-file Rust gate"
	@echo "  make precheck        - Quick cargo check"
	@echo "  make check           - Full local gate (fmt, clippy, test, build, semgrep)"
	@echo "  make test            - Run all tests"
	@echo "  make fmt             - Format all Rust code"
	@echo "  make clean           - Clean build artifacts"
	@echo ""
	@echo "SemVer / Release:"
	@echo "  make version TYPE=patch - Local version bump + changelog + checks + commit"
	@echo "    Options: TYPE=patch|minor|major, SET=X.Y.Z, DRY=1, FORCE=1, NO_TEST=1, TAG=1, PUSH=1"
	@echo "  make version-show    - Show package version, tag state, binary name"
	@echo "  make version-check   - Validate Cargo.toml + CHANGELOG structure"
	@echo "  make release-plan    - Print the post-merge release flow"
	@echo "  make release-check   - Strict release readiness gate"
	@echo "  make release-tag     - Create annotated tag from Cargo.toml version"
	@echo "  make release-push    - Push the release tag to origin"
	@echo "  make publish-checklist - Sync GitHub metadata for release"
	@echo ""
	@echo "Quick start:"
	@echo "  make install         - Contributor setup"
	@echo "  make check           - Full local verification"
	@echo "  make version TYPE=patch - Local patch bump with generated changelog"
	@echo "  make release-gate    - Full pre-release verification (smoke + packaging + workflow)"
	@echo "  make release-plan    - Review release flow after PR merge"
