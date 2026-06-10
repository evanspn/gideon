#!/bin/sh
# Decide which version the next release should be.
#
#   * If Cargo.toml's version has no tag yet, release exactly that version
#     (a deliberate version bump in a PR controls the release number).
#   * Otherwise bump from the HIGHEST existing release tag (patch by
#     default, or the requested level). Versions live in tags — the release
#     workflow never has to push commits back to main, so strict branch
#     protection needs no bypass.
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

# Highest existing semver tag (sort -V understands dotted versions).
latest=$(git tag -l 'v[0-9]*.[0-9]*.[0-9]*' | sed 's/^v//' | sort -V | tail -1)
[ -n "$latest" ] || latest=$current

IFS=. read -r major minor patch <<EOF
$latest
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
