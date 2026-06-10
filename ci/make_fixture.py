#!/usr/bin/env python3
"""Generate a small CBZ fixture library for CI smoke tests.

Usage: make_fixture.py <output-dir>

Creates <output-dir>/Sample Manga/vol1.cbz with three solid-gray PNG pages
(named so natural sorting matters) and a ComicInfo.xml.
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


if __name__ == "__main__":
    main()
