#!/bin/sh
# Setup git hooks for prview-rs
# Run once after cloning or via `make git-hooks`

set -e

REPO_ROOT=$(git rev-parse --show-toplevel)
cd "$REPO_ROOT"

echo "Installing git hooks..."

mkdir -p "$REPO_ROOT/.git/hooks"

# Create symlinks
ln -sf "$REPO_ROOT/tools/githooks/pre-commit" "$REPO_ROOT/.git/hooks/pre-commit"
ln -sf "$REPO_ROOT/tools/githooks/pre-push" "$REPO_ROOT/.git/hooks/pre-push"

# Make sure they're executable
chmod +x tools/githooks/pre-commit
chmod +x tools/githooks/pre-push

echo "Git hooks installed:"
echo "  - pre-commit -> fmt check + cargo check (staged Rust)"
echo "  - pre-push   -> prview gate (default advisory; mode via PRVIEW_GATE_HOOK_MODE)"
echo
echo "Done!"
