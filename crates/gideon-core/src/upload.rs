//! Files sent to the Kobo from the phone: what they are, where they go.
//!
//! The dashboard uploads a file into the sync backend and the device fetches
//! it on its next sync. Nothing about that path is trusted here — not the
//! name the phone gave it, not the MIME type a browser guessed, not the
//! extension. [`classify`] looks at the bytes: a PDF starts `%PDF-`; a ZIP
//! is opened and is an EPUB if it has EPUB's container structure, a CBZ if
//! it holds page images, and nothing we want otherwise. The extension then
//! has to agree, because a `.cbz` that is really an EPUB would land in the
//! manga library where nothing can open it, and the person on the phone
//! deserves to be told which it was.
//!
//! Placement follows what reads the file: EPUB and PDF go beside the manga
//! library, inside the Kobo's own library root, for Nickel to index on its
//! next start (gideon's exit); a CBZ goes into the manga library as a
//! one-chapter series, where gideon's own scan finds it.

use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};

use crate::{Error, Result};

/// Image extensions that make a ZIP a comic — the same set [`crate::cbz`]
/// reads as pages, so "it validated" and "it opens" cannot disagree.
const PAGE_EXTENSIONS: &[&str] = &["jpg", "jpeg", "png", "webp", "gif", "bmp"];

/// The largest upload the device will accept, matching the bucket's limit.
pub const MAX_UPLOAD_BYTES: usize = 50 * 1024 * 1024;

/// What an uploaded file turned out to be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UploadKind {
    Epub,
    Pdf,
    Cbz,
}

impl UploadKind {
    pub fn extension(self) -> &'static str {
        match self {
            UploadKind::Epub => "epub",
            UploadKind::Pdf => "pdf",
            UploadKind::Cbz => "cbz",
        }
    }

    /// Who reads it: the Kobo's own reader, or gideon.
    pub fn is_book(self) -> bool {
        matches!(self, UploadKind::Epub | UploadKind::Pdf)
    }
}

/// Decide what `bytes` are, and check that `file_name`'s extension agrees.
pub fn classify(file_name: &str, bytes: &[u8]) -> Result<UploadKind> {
    let invalid = |m: String| Error::InvalidUpload(m);
    if bytes.is_empty() {
        return Err(invalid("the file is empty".into()));
    }
    if bytes.len() > MAX_UPLOAD_BYTES {
        return Err(invalid(format!(
            "the file is {} MB; the limit is {} MB",
            bytes.len() / (1024 * 1024),
            MAX_UPLOAD_BYTES / (1024 * 1024)
        )));
    }
    let ext = extension_of(file_name);

    let kind = if bytes.starts_with(b"%PDF-") {
        UploadKind::Pdf
    } else if bytes.starts_with(b"PK\x03\x04") {
        classify_zip(bytes)?
    } else {
        return Err(invalid(
            "not an EPUB, PDF or CBZ (the file doesn't start like any of them)".into(),
        ));
    };

    if ext != kind.extension() {
        return Err(invalid(format!(
            "named .{} but the contents are {}",
            if ext.is_empty() {
                "(nothing)"
            } else {
                ext.as_str()
            },
            match kind {
                UploadKind::Epub => "an EPUB",
                UploadKind::Pdf => "a PDF",
                UploadKind::Cbz => "a comic archive (CBZ)",
            }
        )));
    }
    Ok(kind)
}

/// A ZIP is an EPUB when it carries EPUB's container structure, a CBZ when
/// it holds page images, and neither otherwise.
fn classify_zip(bytes: &[u8]) -> Result<UploadKind> {
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes))
        .map_err(|e| Error::InvalidUpload(format!("not a readable ZIP: {e}")))?;
    let mut has_container = false;
    let mut epub_mimetype = false;
    let mut pages = 0usize;
    for i in 0..archive.len() {
        let Ok(mut entry) = archive.by_index(i) else {
            continue;
        };
        let name = entry.name().to_string();
        if name == "META-INF/container.xml" {
            has_container = true;
        } else if name == "mimetype" {
            let mut s = String::new();
            if entry.read_to_string(&mut s).is_ok() && s.trim() == "application/epub+zip" {
                epub_mimetype = true;
            }
        } else if is_page_name(&name) {
            pages += 1;
        }
    }
    if has_container || epub_mimetype {
        return Ok(UploadKind::Epub);
    }
    if pages > 0 {
        return Ok(UploadKind::Cbz);
    }
    Err(Error::InvalidUpload(
        "a ZIP, but neither an EPUB nor a comic (no page images inside)".into(),
    ))
}

/// A ZIP entry that counts as a comic page: an image file, not a directory,
/// not archive junk (`__MACOSX/`, dotfiles).
fn is_page_name(name: &str) -> bool {
    if name.ends_with('/') || name.starts_with("__MACOSX/") {
        return false;
    }
    let base = name.rsplit('/').next().unwrap_or(name);
    if base.starts_with('.') {
        return false;
    }
    PAGE_EXTENSIONS.contains(&extension_of(base).as_str())
}

fn extension_of(name: &str) -> String {
    Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_default()
}

/// The stem of a file name, made safe for a FAT32 card: the characters that
/// break paths become `_`, leading/trailing dots and spaces go, an empty
/// result becomes "untitled", and it is capped so a long title can't blow
/// the path limit. Shared with the manga storage so one rule covers both.
pub fn sanitize_filename(raw: &str) -> String {
    let cleaned: String = raw
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();
    let trimmed = cleaned.trim_matches(|c: char| c == '.' || c.is_whitespace());
    if trimmed.is_empty() {
        "untitled".to_string()
    } else {
        trimmed.chars().take(120).collect()
    }
}

/// Where a validated upload lives on the device. `books_dir` is the Kobo's
/// Books folder (beside the manga library); `library_dir` is the manga
/// library the CBZ scan reads.
pub fn destination(
    kind: UploadKind,
    books_dir: &Path,
    library_dir: &Path,
    file_name: &str,
) -> PathBuf {
    let stem = Path::new(file_name)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(file_name);
    let stem = sanitize_filename(stem);
    let file = format!("{stem}.{}", kind.extension());
    if kind.is_book() {
        books_dir.join(file)
    } else {
        // A one-chapter series named after the file, so it shows on the
        // shelf like anything downloaded from a source.
        library_dir.join(&stem).join(file)
    }
}

/// Write `bytes` to `dest` atomically (temp file + rename), creating the
/// folders. A download that dies halfway must never leave a half file where
/// Nickel or the library scan would pick it up. An existing file is replaced:
/// sending the same file again is "I want the newer one".
pub fn place(dest: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = dest.with_extension("part");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, dest)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn zip_of(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut buf = Cursor::new(Vec::new());
        {
            let mut w = zip::ZipWriter::new(&mut buf);
            let opts = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);
            for (name, body) in entries {
                w.start_file(*name, opts).unwrap();
                w.write_all(body).unwrap();
            }
            w.finish().unwrap();
        }
        buf.into_inner()
    }

    fn epub() -> Vec<u8> {
        zip_of(&[
            ("mimetype", b"application/epub+zip"),
            ("META-INF/container.xml", b"<container/>"),
            ("OEBPS/content.opf", b"<package/>"),
        ])
    }

    fn cbz() -> Vec<u8> {
        zip_of(&[
            ("vol1/001.jpg", b"\xff\xd8\xff"),
            ("vol1/002.png", b"\x89PNG"),
            ("__MACOSX/._001.jpg", b"junk"),
            ("vol1/.DS_Store", b"junk"),
            ("ComicInfo.xml", b"<ComicInfo/>"),
        ])
    }

    #[test]
    fn the_three_kinds_are_recognised_by_their_bytes() {
        assert_eq!(classify("book.epub", &epub()).unwrap(), UploadKind::Epub);
        assert_eq!(
            classify("Berserk v01.cbz", &cbz()).unwrap(),
            UploadKind::Cbz
        );
        assert_eq!(
            classify("paper.pdf", b"%PDF-1.7\n%...").unwrap(),
            UploadKind::Pdf
        );
        // Case of the extension doesn't matter; the bytes do.
        assert_eq!(classify("BOOK.EPUB", &epub()).unwrap(), UploadKind::Epub);
    }

    #[test]
    fn an_epub_is_known_by_its_container_even_without_a_mimetype_entry() {
        let no_mimetype = zip_of(&[
            ("META-INF/container.xml", b"<container/>"),
            ("cover.jpg", b"\xff\xd8"), // an image inside does NOT make it a comic
        ]);
        assert_eq!(classify("x.epub", &no_mimetype).unwrap(), UploadKind::Epub);
    }

    #[test]
    fn a_wrong_extension_is_refused_and_says_what_it_really_is() {
        let err = classify("volume.cbz", &epub()).unwrap_err().to_string();
        assert!(
            err.contains("named .cbz") && err.contains("an EPUB"),
            "{err}"
        );
        let err = classify("novel.epub", &cbz()).unwrap_err().to_string();
        assert!(
            err.contains("named .epub") && err.contains("comic"),
            "{err}"
        );
        let err = classify("scan.pdf", &cbz()).unwrap_err().to_string();
        assert!(err.contains("named .pdf"), "{err}");
        let err = classify("noext", b"%PDF-1.4").unwrap_err().to_string();
        assert!(err.contains("(nothing)"), "{err}");
    }

    #[test]
    fn things_that_are_none_of_the_three_are_refused() {
        let err = classify("x.epub", b"<html>Too many requests</html>").unwrap_err();
        assert!(matches!(err, Error::InvalidUpload(_)), "{err}");
        // A ZIP with no pages and no EPUB structure.
        let junk = zip_of(&[("readme.txt", b"hi")]);
        let err = classify("x.cbz", &junk).unwrap_err().to_string();
        assert!(err.contains("neither an EPUB nor a comic"), "{err}");
        // Empty.
        assert!(classify("x.pdf", b"").is_err());
        // Starts like a ZIP but isn't one.
        assert!(classify("x.cbz", b"PK\x03\x04garbage").is_err());
    }

    #[test]
    fn oversize_is_refused_before_any_parsing() {
        let big = vec![b'%'; MAX_UPLOAD_BYTES + 1];
        let err = classify("x.pdf", &big).unwrap_err().to_string();
        assert!(err.contains("limit is 50 MB"), "{err}");
    }

    #[test]
    fn books_go_beside_the_library_and_comics_into_it() {
        let books = Path::new("/mnt/onboard/Books");
        let manga = Path::new("/mnt/onboard/Manga");
        assert_eq!(
            destination(UploadKind::Epub, books, manga, "Slaughterhouse-Five.epub"),
            Path::new("/mnt/onboard/Books/Slaughterhouse-Five.epub")
        );
        assert_eq!(
            destination(UploadKind::Pdf, books, manga, "paper.PDF"),
            Path::new("/mnt/onboard/Books/paper.pdf")
        );
        assert_eq!(
            destination(UploadKind::Cbz, books, manga, "Berserk v01.cbz"),
            Path::new("/mnt/onboard/Manga/Berserk v01/Berserk v01.cbz")
        );
        // Names from a phone are not paths: any directory part is discarded
        // (file_stem keeps only the last component) and the rest sanitised.
        assert_eq!(
            destination(UploadKind::Epub, books, manga, "../../etc/passwd.epub"),
            Path::new("/mnt/onboard/Books/passwd.epub"),
        );
        assert_eq!(
            destination(UploadKind::Cbz, books, manga, "a:b*c.cbz"),
            Path::new("/mnt/onboard/Manga/a_b_c/a_b_c.cbz"),
        );
    }

    #[test]
    fn place_is_atomic_and_replaces() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("Books").join("a.epub");
        place(&dest, b"one").unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"one");
        assert!(!dest.with_extension("part").exists());
        place(&dest, b"two").unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"two");
    }

    #[test]
    fn sanitize_matches_the_manga_storage_rule() {
        assert_eq!(
            sanitize_filename("One Piece: Chapter 1/2?"),
            "One Piece_ Chapter 1_2_"
        );
        assert_eq!(sanitize_filename("../../etc/passwd"), "_.._etc_passwd");
        assert_eq!(sanitize_filename("  ...  "), "untitled");
        assert!(sanitize_filename(&"x".repeat(500)).len() <= 120);
    }
}
