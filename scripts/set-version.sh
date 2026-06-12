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

# Sync the lockfile's workspace-member versions ONLY. Never use
# `cargo generate-lockfile` here: it re-resolves every third-party
# dependency to the newest compatible version, silently discarding
# the committed pins. That is how the 2026-06-12 release broke —
# time 0.3.48 (published that day) re-entered the lock and hit the
# rcgen 0.14.8 E0119 coherence conflict, despite every CI step
# passing --locked. `cargo update --workspace` touches only the
# path-dependency (workspace member) entries.
cargo update --workspace --quiet

echo "Version set to ${FULL_VERSION}"
