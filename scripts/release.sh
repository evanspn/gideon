#!/usr/bin/env bash
# Cut a semantically versioned release.
#
# Usage:
#   scripts/release.sh 0.2.0          # set an explicit version
#   scripts/release.sh patch          # 0.1.0 -> 0.1.1
#   scripts/release.sh minor          # 0.1.1 -> 0.2.0
#   scripts/release.sh major          # 0.2.0 -> 1.0.0
#
# Releases are normally fully automatic: every merge to main publishes a
# patch release (see .github/workflows/release.yml). Use this script only
# when you want to pick the version deliberately: it bumps Cargo.toml and
# commits — when that commit reaches main, the release workflow publishes
# exactly that version.
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
git commit -m "version: $new"

echo
echo "Version set to $new. Merge/push this to main and the release"
echo "workflow will build, test and publish v$new automatically."
