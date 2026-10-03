#!/usr/bin/env bash
set -euo pipefail

LEVEL="${1:-}"

if [[ "$LEVEL" != "patch" && "$LEVEL" != "minor" && "$LEVEL" != "major" ]]; then
    echo "Usage: $0 <patch|minor|major>"
    exit 1
fi

readonly REPO_ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd "${REPO_ROOT}"

# Bump version in all workspace crates and create a git tag.
# --no-publish   : do not publish to crates.io
# --no-push      : we push manually
# --execute      : actually apply (cargo-release defaults to dry-run)
cargo release "$LEVEL" \
    --tag-prefix "" \
    --tag-name 'v{{version}}' \
    --no-publish \
    --no-push \
    --execute
