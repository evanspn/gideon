#!/bin/sh
# Decide which version the next release should be.
#
# The workspace version in Cargo.toml is the source of truth:
#   * if no tag exists for it yet, release exactly that version (this lets a
#     deliberate version bump in a PR control the release number);
#   * otherwise auto-bump (patch by default, or the requested level).
#
# Usage: auto_version.sh [patch|minor|major]
# Output: "<version> bump=<yes|no>"
set -eu

cd "$(dirname "$0")/.."

level=${1:-patch}
current=$(grep -m1 '^version' Cargo.toml | cut -d'"' -f2)

if ! git rev-parse "v$current" >/dev/null 2>&1; then
    echo "$current bump=no"
    exit 0
fi

IFS=. read -r major minor patch <<EOF
$current
EOF
patch=${patch%%-*}

case "$level" in
    major) echo "$((major + 1)).0.0 bump=yes" ;;
    minor) echo "${major}.$((minor + 1)).0 bump=yes" ;;
    patch) echo "${major}.${minor}.$((patch + 1)) bump=yes" ;;
    *)
        echo "error: unknown bump level '$level' (patch|minor|major)" >&2
        exit 1
        ;;
esac
