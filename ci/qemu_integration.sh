#!/usr/bin/env bash
# Integration tests for the real armv7 Kobo binary, executed under QEMU
# user-mode emulation. Verifies the actual target-architecture code paths:
# zip parsing, image decode, dithering, library scanning and progress
# persistence — plus that the e-ink backend fails gracefully off-device.
#
# Usage: qemu_integration.sh <kobo-binary> <plain-binary>
#   kobo-binary  — built with --features gideon-app/kobo (framebuffer backend)
#   plain-binary — built with default features (memory display reader)
set -euo pipefail

KOBO_BIN=$1
PLAIN_BIN=$2
RUN="qemu-arm-static"

WORKDIR=$(mktemp -d)
trap 'rm -rf "$WORKDIR"' EXIT

fail() {
    echo "FAIL: $1" >&2
    exit 1
}

echo "==> generating fixture library"
python3 "$(dirname "$0")/make_fixture.py" "$WORKDIR/manga"
CBZ="$WORKDIR/manga/Sample Manga/vol1.cbz"

echo "==> [kobo binary] info: metadata and natural page order"
INFO=$($RUN "$KOBO_BIN" info "$CBZ")
echo "$INFO" | grep -F "Sample Manga — Chapter 1" >/dev/null || fail "title not parsed from ComicInfo.xml"
echo "$INFO" | grep -F "Pages:  3" >/dev/null || fail "expected 3 pages"
# Natural ordering: page2 must come before page10.
echo "$INFO" | tr '\n' ' ' | grep -E "page1\.png.*page2\.png.*page10\.png" >/dev/null \
    || fail "pages not in natural order"

echo "==> [kobo binary] render: e-ink pipeline produces a PNG"
$RUN "$KOBO_BIN" render "$CBZ" -p 2 -o "$WORKDIR/page2.png" --width 300 --height 400
[ -s "$WORKDIR/page2.png" ] || fail "rendered PNG is empty"
head -c 8 "$WORKDIR/page2.png" | grep -F "PNG" >/dev/null || fail "output is not a PNG"

echo "==> [kobo binary] read: framebuffer backend fails gracefully off-device"
set +e
READ_OUT=$($RUN "$KOBO_BIN" read "$CBZ" </dev/null 2>&1)
READ_RC=$?
set -e
[ "$READ_RC" -ne 0 ] || fail "kobo read should fail without a framebuffer"
echo "$READ_OUT" | grep -F "failed to open the e-ink framebuffer" >/dev/null \
    || fail "unexpected error message: $READ_OUT"

echo "==> [kobo binary] first boot: missing library dir is created, exit 0"
FIRSTBOOT="$WORKDIR/fresh-device/Manga"
$RUN "$KOBO_BIN" library "$FIRSTBOOT" | grep -q "Library initialized" \
    || fail "first boot should initialize the library directory"
[ -d "$FIRSTBOOT" ] || fail "library directory was not created"
$RUN "$KOBO_BIN" library "$FIRSTBOOT" | grep -q "No CBZ files found" \
    || fail "second boot with empty library should report no CBZ files"

echo "==> [plain binary] full read loop with progress persistence"
$RUN "$PLAIN_BIN" library "$WORKDIR/manga" >/dev/null
printf 'n\nq\n' | $RUN "$PLAIN_BIN" read "$CBZ" >/dev/null
$RUN "$PLAIN_BIN" library "$WORKDIR/manga" | grep -F "(page 2/3)" >/dev/null \
    || fail "reading progress did not persist through the library"

echo "==> [plain binary] resume from saved progress"
OUT=$(printf 'q\n' | $RUN "$PLAIN_BIN" read "$CBZ")
echo "$OUT" | grep -F "page 2/3" >/dev/null || fail "reader did not resume at page 2"

echo "ALL QEMU INTEGRATION TESTS PASSED"
