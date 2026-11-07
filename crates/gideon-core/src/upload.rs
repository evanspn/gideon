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
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes)).map_err(|e| {
        let text = e.to_string();
        // The end-of-central-directory record is the last thing in a ZIP;
        // its absence means the file stops early — damaged at the source, or
        // cut short on its way here. Say that, not the parser's acronym.
        if text.contains("EOCD") {
            Error::InvalidUpload(
                "the archive is damaged or incomplete (no end-of-archive record)".into(),
            )
        } else {
            Error::InvalidUpload(format!("not a readable ZIP: {text}"))
        }
    })?;
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

/// What a file says about itself, for naming it on the device. Either field
/// may be missing; the file name then fills in (see [`nice_name`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BookMeta {
    pub title: Option<String>,
    pub author: Option<String>,
}

/// Title and author from an EPUB's own metadata: `META-INF/container.xml`
/// names the OPF, the OPF's `<dc:title>` / `<dc:creator>` are the book's
/// idea of itself. Any part missing or malformed → the field is `None`;
/// never an error, since the file already validated as an EPUB and a name
/// is the one thing we can always fall back on.
pub fn epub_meta(bytes: &[u8]) -> BookMeta {
    let Ok(archive) = zip::ZipArchive::new(Cursor::new(bytes)) else {
        return BookMeta::default();
    };
    epub_meta_from(archive)
}

/// [`epub_meta`] for a file already on disk, reading only the two small
/// entries it needs — a 200 MB novel must not be pulled into memory just to
/// learn its title.
pub fn epub_meta_file(path: &Path) -> BookMeta {
    let Ok(file) = std::fs::File::open(path) else {
        return BookMeta::default();
    };
    let Ok(archive) = zip::ZipArchive::new(std::io::BufReader::new(file)) else {
        return BookMeta::default();
    };
    epub_meta_from(archive)
}

fn epub_meta_from<R: Read + std::io::Seek>(mut archive: zip::ZipArchive<R>) -> BookMeta {
    let read = |archive: &mut zip::ZipArchive<R>, name: &str| -> Option<String> {
        let mut entry = archive.by_name(name).ok()?;
        let mut s = String::new();
        entry.read_to_string(&mut s).ok()?;
        Some(s)
    };
    let Some(container) = read(&mut archive, "META-INF/container.xml") else {
        return BookMeta::default();
    };
    let Some(opf_path) = opf_path_from_container(&container) else {
        return BookMeta::default();
    };
    let Some(opf) = read(&mut archive, &opf_path) else {
        return BookMeta::default();
    };
    opf_meta(&opf)
}

/// The `full-path` of the first `<rootfile>` in container.xml.
fn opf_path_from_container(xml: &str) -> Option<String> {
    use quick_xml::events::Event;
    let mut reader = quick_xml::Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) if e.local_name().as_ref() == b"rootfile" => {
                return e.attributes().flatten().find_map(|a| {
                    (a.key.local_name().as_ref() == b"full-path")
                        .then(|| a.unescape_value().ok().map(|v| v.into_owned()))
                        .flatten()
                });
            }
            Ok(Event::Eof) | Err(_) => return None,
            _ => {}
        }
    }
}

/// `dc:title` (the first) and every `dc:creator` (joined) from an OPF.
fn opf_meta(xml: &str) -> BookMeta {
    use quick_xml::events::Event;
    let mut reader = quick_xml::Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut title: Option<String> = None;
    let mut creators: Vec<String> = Vec::new();
    let mut current: Option<&'static str> = None;
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => {
                current = match e.local_name().as_ref() {
                    b"title" if title.is_none() => Some("title"),
                    b"creator" => Some("creator"),
                    _ => None,
                };
            }
            Ok(Event::Text(t)) => {
                if let Some(which) = current {
                    let text = t
                        .unescape()
                        .map(|s| s.trim().to_string())
                        .unwrap_or_default();
                    if !text.is_empty() {
                        match which {
                            "title" => title = Some(text),
                            _ => creators.push(text),
                        }
                    }
                }
            }
            Ok(Event::End(_)) => current = None,
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }
    BookMeta {
        title,
        author: (!creators.is_empty()).then(|| creators.join(", ")),
    }
}

/// Series/title and writer from a CBZ's `ComicInfo.xml`, when it has one.
pub fn cbz_meta(bytes: &[u8]) -> BookMeta {
    let Ok(mut archive) = zip::ZipArchive::new(Cursor::new(bytes)) else {
        return BookMeta::default();
    };
    let Ok(mut entry) = archive.by_name("ComicInfo.xml") else {
        return BookMeta::default();
    };
    let mut xml = String::new();
    if entry.read_to_string(&mut xml).is_err() {
        return BookMeta::default();
    }
    let Ok(info) = crate::ComicInfo::parse(&xml) else {
        return BookMeta::default();
    };
    BookMeta {
        // The series is what a library folder is named after; the chapter
        // title stays in the file name.
        title: info
            .series
            .or(info.title)
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty()),
        author: info
            .writer
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty()),
    }
}

/// A readable title out of a file name from the wild. Ebook files tend to
/// arrive as "Title -- Author -- Publisher, Year -- isbn -- hash -- Site.epub",
/// and with ':' turned into '_' by whoever saved them; the title is the part
/// before the first " -- ", with "_ " read back as ": ". Only a fallback —
/// the file's own metadata wins when it has any.
pub fn nice_name(file_name: &str) -> String {
    let stem = Path::new(file_name)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(file_name);
    let head = stem.split(" -- ").next().unwrap_or(stem);
    let head = head.replace("_ ", ": ");
    let collapsed: String = head.split_whitespace().collect::<Vec<_>>().join(" ");
    // A name that is nothing but dots or spaces is no name.
    let out = collapsed
        .trim_matches(|c: char| c == '.' || c.is_whitespace())
        .to_string();
    if out.is_empty() {
        "untitled".to_string()
    } else {
        out
    }
}

/// Where a validated upload lives, named from what the file says about
/// itself with the file name as fallback. Books go `Books/<Author>/<Title>`
/// — the same layout the OPDS downloads use, so one shelf, one rule — and a
/// comic goes into the manga library under its series, as a chapter named
/// after the file.
pub fn destination_named(
    kind: UploadKind,
    books_dir: &Path,
    library_dir: &Path,
    file_name: &str,
    meta: &BookMeta,
) -> PathBuf {
    let fallback = nice_name(file_name);
    let title = meta
        .title
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .unwrap_or(&fallback);
    if kind.is_book() {
        let author = meta
            .author
            .as_deref()
            .map(str::trim)
            .filter(|a| !a.is_empty())
            .unwrap_or("Unknown author");
        books_dir.join(sanitize_filename(author)).join(format!(
            "{}.{}",
            sanitize_filename(title),
            kind.extension()
        ))
    } else {
        library_dir
            .join(sanitize_filename(title))
            .join(format!("{}.cbz", sanitize_filename(&fallback)))
    }
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
        // Starts like a ZIP but isn't one: a truncated archive, in the words
        // the phone should show.
        let err = classify("x.cbz", b"PK\x03\x04garbage")
            .unwrap_err()
            .to_string();
        assert!(err.contains("damaged or incomplete"), "{err}");
        // A real archive with its tail chopped off says the same.
        let mut cut = cbz();
        cut.truncate(cut.len() - 30);
        let err = classify("x.cbz", &cut).unwrap_err().to_string();
        assert!(err.contains("damaged or incomplete"), "{err}");
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

    fn epub_with_meta(title: &str, authors: &[&str]) -> Vec<u8> {
        let creators: String = authors
            .iter()
            .map(|a| format!("<dc:creator>{a}</dc:creator>"))
            .collect();
        let opf = format!(
            r#"<?xml version="1.0"?><package xmlns="http://www.idpf.org/2007/opf" xmlns:dc="http://purl.org/dc/elements/1.1/" version="3.0"><metadata><dc:title>{title}</dc:title>{creators}<dc:language>en</dc:language></metadata><manifest><item id="t" href="t.xhtml" media-type="application/xhtml+xml"/></manifest></package>"#
        );
        zip_of(&[
            ("mimetype", b"application/epub+zip"),
            (
                "META-INF/container.xml",
                br#"<?xml version="1.0"?><container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container"><rootfiles><rootfile full-path="OEBPS/content.opf" media-type="application/oebps-package+xml"/></rootfiles></container>"#,
            ),
            ("OEBPS/content.opf", opf.as_bytes()),
        ])
    }

    #[test]
    fn an_epub_names_itself_from_its_opf() {
        let bytes = epub_with_meta(
            "The Apothecary Diaries: Volume 5",
            &["Natsu Hyuuga", "Touko Shino"],
        );
        let meta = epub_meta(&bytes);
        assert_eq!(
            meta.title.as_deref(),
            Some("The Apothecary Diaries: Volume 5")
        );
        assert_eq!(meta.author.as_deref(), Some("Natsu Hyuuga, Touko Shino"));
        // And that, not the file name from the wild, is where it lands.
        let dest = destination_named(
            UploadKind::Epub,
            Path::new("/mnt/onboard/Books"),
            Path::new("/mnt/onboard/Manga"),
            "The Apothecary Diaries_ Volume 5 (Light Novel) -- Natsu Hyuuga -- J-Novel Club -- Anna's Archive.epub",
            &meta,
        );
        assert_eq!(
            dest,
            Path::new("/mnt/onboard/Books/Natsu Hyuuga, Touko Shino/The Apothecary Diaries_ Volume 5.epub")
        );
        // An EPUB with no metadata at all is not an error: the name fills in.
        assert_eq!(epub_meta(&epub()), BookMeta::default());
    }

    #[test]
    fn a_file_name_from_the_wild_becomes_a_readable_fallback_title() {
        assert_eq!(
            nice_name("The Apothecary Diaries_ Volume 5 (Light Novel) -- Natsu Hyuuga (author) -- J-Novel Club -- isbn13 9781718361263 -- 25cafa7 -- Anna's Archive.epub"),
            "The Apothecary Diaries: Volume 5 (Light Novel)"
        );
        assert_eq!(nice_name("Slaughterhouse-Five.epub"), "Slaughterhouse-Five");
        assert_eq!(nice_name("  spaced   out .pdf"), "spaced out");
        assert_eq!(nice_name("....epub"), "untitled");
        // Fallback naming for a PDF (no embedded title we trust) and a comic.
        let books = Path::new("/b");
        let manga = Path::new("/m");
        assert_eq!(
            destination_named(
                UploadKind::Pdf,
                books,
                manga,
                "paper -- someone.pdf",
                &BookMeta::default()
            ),
            Path::new("/b/Unknown author/paper.pdf")
        );
        assert_eq!(
            destination_named(
                UploadKind::Cbz,
                books,
                manga,
                "Berserk v01.cbz",
                &BookMeta::default()
            ),
            Path::new("/m/Berserk v01/Berserk v01.cbz")
        );
        // A comic with ComicInfo files under its series.
        let meta = BookMeta {
            title: Some("Berserk".into()),
            author: Some("Kentaro Miura".into()),
        };
        assert_eq!(
            destination_named(UploadKind::Cbz, books, manga, "Berserk v01.cbz", &meta),
            Path::new("/m/Berserk/Berserk v01.cbz")
        );
    }

    #[test]
    fn a_cbz_names_its_series_from_comicinfo() {
        let bytes = zip_of(&[
            ("001.jpg", b"\xff\xd8"),
            ("ComicInfo.xml", b"<ComicInfo><Series>Berserk</Series><Title>The Black Swordsman</Title><Writer>Kentaro Miura</Writer></ComicInfo>"),
        ]);
        let meta = cbz_meta(&bytes);
        assert_eq!(meta.title.as_deref(), Some("Berserk"));
        assert_eq!(meta.author.as_deref(), Some("Kentaro Miura"));
        assert_eq!(
            cbz_meta(&cbz()),
            BookMeta {
                title: None,
                author: None
            },
            "{:?}",
            cbz_meta(&cbz())
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
