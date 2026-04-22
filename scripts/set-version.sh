#!/usr/bin/env bash
set -euo pipefail

# Compute release version from workspace Cargo.toml base version + git commit count.
# Base version (e.g., 2026.1) is manually bumped for new version series.
# Patch version is the number of commits since the base was set.
#
# Usage: ./scripts/set-version.sh [--dry-run]

DRY_RUN="${1:-}"

BASE_VERSION=$(grep '^version' Cargo.toml | head -1 | sed 's/.*"\(.*\)\.0"/\1/')
COMMIT_COUNT=$(git rev-list --count HEAD)
FULL_VERSION="${BASE_VERSION}.${COMMIT_COUNT}"

echo "Base version: ${BASE_VERSION}"
echo "Commit count: ${COMMIT_COUNT}"
echo "Full version: ${FULL_VERSION}"

if [ "$DRY_RUN" = "--dry-run" ]; then
    echo "(dry run — no files modified)"
    exit 0
fi

# Patch workspace Cargo.toml
sed -i.bak "s/^version = \"${BASE_VERSION}\\.0\"/version = \"${FULL_VERSION}\"/" Cargo.toml
rm -f Cargo.toml.bak

# Patch workspace dependency versions for internal crates
sed -i.bak "s/version = \"${BASE_VERSION}\\.0\"/version = \"${FULL_VERSION}\"/g" Cargo.toml
rm -f Cargo.toml.bak

# Regenerate Cargo.lock
cargo generate-lockfile --quiet

echo "Version set to ${FULL_VERSION}"
