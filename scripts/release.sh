#!/usr/bin/env bash
# Cut a semantically versioned release.
#
# Usage:
#   scripts/release.sh 0.2.0          # set an explicit version
#   scripts/release.sh patch          # 0.1.0 -> 0.1.1
#   scripts/release.sh minor          # 0.1.1 -> 0.2.0
#   scripts/release.sh major          # 0.2.0 -> 1.0.0
#
# Bumps the workspace version in Cargo.toml (which every crate and the
# binary's --version inherit), refreshes Cargo.lock, commits and creates
# the matching tag. Then:
#
#   git push origin main --follow-tags
#
# The tag triggers .github/workflows/release.yml, which gates on the full
# test suite + QEMU integration before publishing the GitHub Release with
# the versioned Kobo bundle attached.
set -euo pipefail

cd "$(dirname "$0")/.."

if [ $# -ne 1 ]; then
    sed -n '2,18p' "$0" | sed 's/^# \{0,1\}//' >&2
    exit 1
fi

current=$(grep -m1 '^version' Cargo.toml | cut -d'"' -f2)
IFS=. read -r major minor patch <<EOF
$current
EOF
# Strip any pre-release suffix from the patch component for bumping.
patch=${patch%%-*}

case "$1" in
    major) new="$((major + 1)).0.0" ;;
    minor) new="${major}.$((minor + 1)).0" ;;
    patch) new="${major}.${minor}.$((patch + 1))" ;;
    *)
        new=$1
        if ! printf '%s' "$new" | grep -qE '^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$'; then
            echo "error: '$new' is not a valid semantic version (X.Y.Z or X.Y.Z-pre)" >&2
            exit 1
        fi
        ;;
esac

if git rev-parse "v$new" >/dev/null 2>&1; then
    echo "error: tag v$new already exists" >&2
    exit 1
fi

if [ -n "$(git status --porcelain)" ]; then
    echo "error: working tree is not clean; commit or stash first" >&2
    exit 1
fi

echo "Releasing: $current -> $new"
sed -i.bak "0,/^version = \"$current\"/s//version = \"$new\"/" Cargo.toml
rm -f Cargo.toml.bak

# Refresh Cargo.lock with the new workspace version.
cargo update --workspace --quiet

git add Cargo.toml Cargo.lock
git commit -m "release: v$new"
git tag -a "v$new" -m "gideon v$new"

echo
echo "Created commit and tag v$new. To publish:"
echo "  git push origin HEAD --follow-tags"
