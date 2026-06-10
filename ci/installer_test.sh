#!/usr/bin/env bash
# Tests for installer/install.sh against a fake Kobo root.
#
# The property under test: no install, upgrade or (non-purge) uninstall may
# ever lose user data.
set -euo pipefail

INSTALLER="$(cd "$(dirname "$0")/.." && pwd)/installer/install.sh"
WORKDIR=$(mktemp -d)
trap 'rm -rf "$WORKDIR"' EXIT

fail() {
    echo "FAIL: $1" >&2
    exit 1
}

# A fake Kobo root: just needs the .kobo marker directory.
ROOT="$WORKDIR/KOBOeReader"
mkdir -p "$ROOT/.kobo" "$ROOT/.adds/nm" "$ROOT/Manga"

# Fake gideon "binaries" we can tell apart.
echo "binary-v1" > "$WORKDIR/gideon-v1"
echo "binary-v2" > "$WORKDIR/gideon-v2"

echo "==> refuses a root that isn't a Kobo"
if sh "$INSTALLER" --binary "$WORKDIR/gideon-v1" --root "$WORKDIR/not-a-kobo" 2>/dev/null; then
    fail "should refuse a directory without .kobo"
fi

echo "==> fresh install"
sh "$INSTALLER" --binary "$WORKDIR/gideon-v1" --root "$ROOT" >/dev/null
[ -x "$ROOT/.adds/gideon/bin/gideon" ] || fail "binary not installed"
grep -q "binary-v1" "$ROOT/.adds/gideon/bin/gideon" || fail "wrong binary installed"
[ -d "$ROOT/.adds/gideon/data" ] || fail "data dir not created"
[ -f "$ROOT/.adds/nm/gideon" ] || fail "NickelMenu entry not installed"

echo "==> simulate user data accumulating between installs"
mkdir -p "$ROOT/.adds/gideon/data"
echo '{"progress":{"Berserk/vol1.cbz":{"current_page":42}}}' > "$ROOT/.adds/gideon/data/progress.json"
echo '{"fit":"contain"}' > "$ROOT/.adds/gideon/data/settings.json"
mkdir -p "$ROOT/Manga/.gideon"
echo '{"progress":{}}' > "$ROOT/Manga/.gideon/progress.json"

echo "==> upgrade preserves user data and creates a backup"
sh "$INSTALLER" --binary "$WORKDIR/gideon-v2" --root "$ROOT" >/dev/null
grep -q "binary-v2" "$ROOT/.adds/gideon/bin/gideon" || fail "binary not upgraded"
grep -q '"current_page":42' "$ROOT/.adds/gideon/data/progress.json" \
    || fail "UPGRADE LOST USER PROGRESS"
grep -q '"fit":"contain"' "$ROOT/.adds/gideon/data/settings.json" \
    || fail "UPGRADE LOST USER SETTINGS"
[ -f "$ROOT/Manga/.gideon/progress.json" ] || fail "upgrade touched library progress"
BACKUPS=$(find "$ROOT/.adds/gideon/backups" -name "data-*.tar.gz" 2>/dev/null | wc -l)
[ "$BACKUPS" -eq 1 ] || fail "expected 1 backup after upgrade, found $BACKUPS"

echo "==> backup actually contains the data"
tar tzf "$(find "$ROOT/.adds/gideon/backups" -name "data-*.tar.gz" | head -1)" | grep -q "data/progress.json" \
    || fail "backup is missing progress.json"

echo "==> backups rotate (keep 3 most recent)"
for _ in 1 2 3 4; do
    # Distinct timestamps: the backup name has second granularity.
    sleep 1
    sh "$INSTALLER" --binary "$WORKDIR/gideon-v2" --root "$ROOT" >/dev/null
done
BACKUPS=$(find "$ROOT/.adds/gideon/backups" -name "data-*.tar.gz" | wc -l)
[ "$BACKUPS" -le 3 ] || fail "backups not rotated: $BACKUPS remain"

echo "==> --no-backup skips backup but still preserves data"
BEFORE=$(find "$ROOT/.adds/gideon/backups" -name "data-*.tar.gz" | wc -l)
sh "$INSTALLER" --binary "$WORKDIR/gideon-v1" --root "$ROOT" --no-backup >/dev/null
AFTER=$(find "$ROOT/.adds/gideon/backups" -name "data-*.tar.gz" | wc -l)
[ "$BEFORE" -eq "$AFTER" ] || fail "--no-backup still created a backup"
grep -q '"current_page":42' "$ROOT/.adds/gideon/data/progress.json" \
    || fail "--no-backup lost user data"

echo "==> uninstall keeps user data by default"
sh "$INSTALLER" --uninstall --root "$ROOT" >/dev/null
[ ! -e "$ROOT/.adds/gideon/bin" ] || fail "uninstall left the binary"
[ ! -e "$ROOT/.adds/nm/gideon" ] || fail "uninstall left the NickelMenu entry"
grep -q '"current_page":42' "$ROOT/.adds/gideon/data/progress.json" \
    || fail "UNINSTALL LOST USER DATA"

echo "==> uninstall --purge removes everything"
sh "$INSTALLER" --uninstall --purge --root "$ROOT" >/dev/null
[ ! -e "$ROOT/.adds/gideon" ] || fail "--purge left the app directory"
[ -f "$ROOT/Manga/.gideon/progress.json" ] || fail "--purge must not touch the manga library"

echo "ALL INSTALLER TESTS PASSED"
