#!/usr/bin/env bash
# Integration tests for the real armv7 Kobo binary, executed under QEMU
# user-mode emulation. Verifies the actual target-architecture code paths:
# zip parsing, image decode, dithering, library and book-shelf scanning and
# progress persistence — plus that the e-ink backend fails gracefully off-device.
#
# Usage: qemu_integration.sh <kobo-binary> <plain-binary>
#   kobo-binary  — built with --features gideon-app/kobo (framebuffer backend)
#   plain-binary — built with default features (memory display reader)
set -euo pipefail

KOBO_BIN=$1
PLAIN_BIN=$2
# qemu-user-static ships qemu-arm-static; on Ubuntu 26.04 runners that package
# is virtual and qemu-user-binfmt ships plain qemu-arm instead. Either runs our
# musl-static binaries. (binfmt registration would make "$KOBO_BIN" alone work
# too, but naming the emulator keeps the failure mode obvious.)
RUN=$(command -v qemu-arm-static || command -v qemu-arm || true)
if [ -z "$RUN" ]; then
    echo "FAIL: no qemu-arm emulator installed (need qemu-user-static or qemu-user-binfmt)" >&2
    exit 1
fi

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

echo "==> [kobo binary] books: the Library's Books tab lists an EPUB by its own metadata"
BOOKS=$($RUN "$KOBO_BIN" books "$WORKDIR/Books")
echo "$BOOKS" | grep -F "1 book(s)" >/dev/null || fail "expected one book on the shelf: $BOOKS"
# The file is named book-0001.epub: the title and author can only have come
# from the OPF inside it.
echo "$BOOKS" | grep -F "Slaughterhouse-Five — Kurt Vonnegut" >/dev/null \
    || fail "book title/author not read from EPUB metadata: $BOOKS"
$RUN "$KOBO_BIN" books "$WORKDIR/nope/Books" | grep -F "No books" >/dev/null \
    || fail "a missing Books folder should be an empty shelf, not an error"

echo "==> [kobo binary] book: the reader flattens an EPUB's spine to chapters and paragraphs"
BOOK=$($RUN "$KOBO_BIN" book "$WORKDIR/Books/Kurt Vonnegut/book-0001.epub")
echo "$BOOK" | grep -F "Slaughterhouse-Five — Kurt Vonnegut" >/dev/null || fail "book title line missing: $BOOK"
echo "$BOOK" | grep -F "2 chapter(s), 5 paragraph(s), 1 picture(s)" >/dev/null || fail "chapter/paragraph/picture count wrong: $BOOK"
# Spine order, headings as chapter titles, CSS text not mistaken for prose.
echo "$BOOK" | tr '\n' ' ' | grep -E "1\. One .*All this happened.*2\. Two .*Listen" >/dev/null \
    || fail "chapters not in spine order with their first paragraphs: $BOOK"
echo "$BOOK" | grep -F "x:y" >/dev/null && fail "stylesheet text leaked into the book"

echo "==> [kobo binary] browse --screenshot: Library renders both tabs headlessly"
$RUN "$KOBO_BIN" browse --library "$WORKDIR/manga" --screenshot "$WORKDIR/library.png" --screen library
$RUN "$KOBO_BIN" browse --library "$WORKDIR/manga" --screenshot "$WORKDIR/books.png" --screen books
$RUN "$KOBO_BIN" browse --library "$WORKDIR/manga" --screenshot "$WORKDIR/reader.png" --screen reader
for shot in library books reader; do
    [ -s "$WORKDIR/$shot.png" ] || fail "$shot screenshot is empty"
    head -c 8 "$WORKDIR/$shot.png" | grep -F "PNG" >/dev/null || fail "$shot screenshot is not a PNG"
done
# Two tabs, two different frames: if the Books tab drew the manga shelf (or
# vice versa) the files would be byte-identical.
cmp -s "$WORKDIR/library.png" "$WORKDIR/books.png" && fail "Manga and Books tabs rendered identically"

echo "==> [plain binary] full read loop with progress persistence"
$RUN "$PLAIN_BIN" library "$WORKDIR/manga" >/dev/null
printf 'n\nq\n' | $RUN "$PLAIN_BIN" read "$CBZ" >/dev/null
$RUN "$PLAIN_BIN" library "$WORKDIR/manga" | grep -F "(page 2/3)" >/dev/null \
    || fail "reading progress did not persist through the library"

echo "==> [plain binary] resume from saved progress"
OUT=$(printf 'q\n' | $RUN "$PLAIN_BIN" read "$CBZ")
echo "$OUT" | grep -F "page 2/3" >/dev/null || fail "reader did not resume at page 2"

echo "ALL QEMU INTEGRATION TESTS PASSED"
