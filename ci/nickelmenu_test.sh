#!/usr/bin/env bash
# Tests for installer/nickelmenu.sh against a fake Kobo root.
set -euo pipefail
TOOL="$(cd "$(dirname "$0")/.." && pwd)/installer/nickelmenu.sh"
W=$(mktemp -d); trap 'rm -rf "$W"' EXIT
fail() { echo "FAIL: $1" >&2; exit 1; }
R="$W/KOBOeReader"; mkdir -p "$R/.kobo" "$R/.adds/nm" "$R/.adds/gideon/data"
echo "state" > "$R/.adds/gideon/data/progress"; echo "m" > "$R/.adds/nm/gideon"
SUMS() { (cd "$R/.adds" && find . -type f | sort | xargs shasum -a 256); }
BEFORE=$(SUMS)
# the real pinned tgz is fetched only in dry-run-free CI-safe form: build a fake and override via sed'd copy
SHA=$(sed -n 's/^NM_SHA256="\(.*\)"/\1/p' "$TOOL")
echo "fake-tgz" > "$W/fake.tgz"
FAKESHA=$(shasum -a 256 "$W/fake.tgz" | cut -d' ' -f1)
T2="$W/tool.sh"; sed "s/$SHA/$FAKESHA/" "$TOOL" > "$T2"

echo "==> refuses non-Kobo"; sh "$T2" install --root "$W/nope" --tgz "$W/fake.tgz" 2>/dev/null && fail "accepted non-Kobo"
echo "==> status"; OUT=$(sh "$T2" status --root "$R"); case "$OUT" in *"Pending:   none"*) ;; *) fail "status";; esac
echo "==> bad checksum refused"; echo bad > "$W/bad.tgz"; sh "$T2" install --root "$R" --tgz "$W/bad.tgz" 2>/dev/null && fail "bad tgz accepted"
[ ! -e "$R/.kobo/KoboRoot.tgz" ] || fail "wrote on bad checksum"
echo "==> dry run writes nothing"; sh "$T2" install --root "$R" --tgz "$W/fake.tgz" --dry-run >/dev/null; [ ! -e "$R/.kobo/KoboRoot.tgz" ] || fail "dry-run wrote"
echo "==> install stages tgz"; sh "$T2" install --root "$R" --tgz "$W/fake.tgz" >/dev/null; cmp "$W/fake.tgz" "$R/.kobo/KoboRoot.tgz" || fail "not staged"
echo "==> idempotent"; OUT=$(sh "$T2" install --root "$R" --tgz "$W/fake.tgz"); case "$OUT" in *already*) ;; *) fail "not idempotent";; esac
echo "==> foreign pending tgz protected"; echo firmware > "$R/.kobo/KoboRoot.tgz"
sh "$T2" install --root "$R" --tgz "$W/fake.tgz" 2>/dev/null && fail "overwrote pending"
grep -q firmware "$R/.kobo/KoboRoot.tgz" || fail "pending changed"
sh "$T2" install --root "$R" --tgz "$W/fake.tgz" --force >/dev/null; cmp "$W/fake.tgz" "$R/.kobo/KoboRoot.tgz" || fail "force failed"
echo "==> .adds byte-identical"; [ "$BEFORE" = "$(SUMS)" ] || fail ".adds changed"
echo "OK"
