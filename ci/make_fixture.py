#!/usr/bin/env python3
"""Generate a small CBZ fixture library for CI smoke tests.

Usage: make_fixture.py <output-dir>

Creates <output-dir>/Sample Manga/vol1.cbz with three solid-gray PNG pages
(named so natural sorting matters) and a ComicInfo.xml, plus a minimal EPUB
beside the library at <output-dir>/../Books/Kurt Vonnegut/book-0001.epub.
"""

import os
import struct
import sys
import zipfile
import zlib


def png(width: int, height: int, value: int) -> bytes:
    raw = b"".join(b"\x00" + bytes([value]) * width for _ in range(height))

    def chunk(tag: bytes, data: bytes) -> bytes:
        body = tag + data
        return struct.pack(">I", len(data)) + body + struct.pack(">I", zlib.crc32(body) & 0xFFFFFFFF)

    return (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", struct.pack(">IIBBBBB", width, height, 8, 0, 0, 0, 0))
        + chunk(b"IDAT", zlib.compress(raw))
        + chunk(b"IEND", b"")
    )


def main() -> None:
    out_dir = sys.argv[1]
    manga_dir = os.path.join(out_dir, "Sample Manga")
    os.makedirs(manga_dir, exist_ok=True)

    comic_info = (
        "<ComicInfo>"
        "<Series>Sample Manga</Series>"
        "<Title>Chapter 1</Title>"
        "<Number>1</Number>"
        "</ComicInfo>"
    )

    with zipfile.ZipFile(os.path.join(manga_dir, "vol1.cbz"), "w") as z:
        z.writestr("ComicInfo.xml", comic_info)
        # Deliberately unsorted names: natural order should yield 1, 2, 10.
        for page_number, gray in [(10, 0), (2, 128), (1, 255)]:
            z.writestr(f"page{page_number}.png", png(60, 90, gray))

    print(f"fixture written to {manga_dir}/vol1.cbz")

    # A book beside the manga: Books/<Author>/<Title>.epub, the layout gideon
    # writes for OPDS downloads and phone sends. Its metadata names it; the
    # file name deliberately does not, so a title in the output proves the
    # EPUB was actually opened and its OPF read on the target architecture.
    books_dir = os.path.join(os.path.dirname(out_dir.rstrip("/")), "Books", "Kurt Vonnegut")
    os.makedirs(books_dir, exist_ok=True)
    container = (
        '<?xml version="1.0"?><container version="1.0" '
        'xmlns="urn:oasis:names:tc:opendocument:xmlns:container"><rootfiles>'
        '<rootfile full-path="OEBPS/content.opf" media-type="application/oebps-package+xml"/>'
        "</rootfiles></container>"
    )
    opf = (
        '<?xml version="1.0"?><package xmlns="http://www.idpf.org/2007/opf" '
        'xmlns:dc="http://purl.org/dc/elements/1.1/" version="3.0"><metadata>'
        "<dc:title>Slaughterhouse-Five</dc:title><dc:creator>Kurt Vonnegut</dc:creator>"
        "</metadata><manifest>"
        '<item id="nav" href="nav.xhtml" media-type="application/xhtml+xml" properties="nav"/>'
        '<item id="c1" href="ch1.xhtml" media-type="application/xhtml+xml"/>'
        '<item id="c2" href="ch2.xhtml" media-type="application/xhtml+xml"/>'
        '</manifest><spine><itemref idref="c1"/><itemref idref="c2"/></spine></package>'
    )
    nav = (
        '<html xmlns:epub="http://www.idpf.org/2007/ops"><body><nav epub:type="toc"><ol>'
        '<li><a href="ch1.xhtml">One</a></li><li><a href="ch2.xhtml">Two</a></li>'
        "</ol></nav></body></html>"
    )
    epub_path = os.path.join(books_dir, "book-0001.epub")
    with zipfile.ZipFile(epub_path, "w") as z:
        z.writestr("mimetype", "application/epub+zip", compress_type=zipfile.ZIP_STORED)
        z.writestr("META-INF/container.xml", container)
        z.writestr("OEBPS/content.opf", opf)
        z.writestr("OEBPS/nav.xhtml", nav)
        z.writestr(
            "OEBPS/ch1.xhtml",
            "<html><head><style>p{x:y}</style></head><body><h1>One</h1>"
            "<p>All this happened, more or less.</p><p>So it goes.</p></body></html>",
        )
        z.writestr(
            "OEBPS/ch2.xhtml",
            "<html><body><h1>Two</h1><p>Listen: Billy Pilgrim has come unstuck in time.</p>"
            '<img src="Images/plate.png" alt="plate"/></body></html>',
        )
        # An illustration, so the reader's image path is exercised on the
        # target too (decoded, scaled, drawn).
        z.writestr("OEBPS/Images/plate.png", png(120, 80, 64))
    print(f"fixture written to {epub_path}")


if __name__ == "__main__":
    main()
