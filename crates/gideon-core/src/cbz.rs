//! CBZ (comic book zip) document handling.

use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

use image::DynamicImage;
use zip::ZipArchive;

use crate::comicinfo::ComicInfo;
use crate::natsort::natural_cmp;
use crate::{Error, Result};

/// Image extensions recognized as pages inside a CBZ archive.
const PAGE_EXTENSIONS: &[&str] = &["jpg", "jpeg", "png", "webp", "gif", "bmp"];

/// An opened CBZ document.
///
/// Pages are exposed in natural reading order (so `2.jpg` comes before
/// `10.jpg`), with archive junk (`__MACOSX/`, dotfiles, thumbnails,
/// non-image entries) filtered out.
pub struct CbzDocument {
    path: PathBuf,
    archive: ZipArchive<File>,
    pages: Vec<String>,
    comic_info: Option<ComicInfo>,
}

impl CbzDocument {
    /// Open a CBZ file and index its pages.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let file = File::open(&path)?;
        let mut archive = ZipArchive::new(file).map_err(|source| Error::OpenArchive {
            path: path.clone(),
            source,
        })?;

        let mut pages: Vec<String> = archive
            .file_names()
            .filter(|name| is_page_entry(name))
            .map(str::to_owned)
            .collect();
        pages.sort_by(|a, b| natural_cmp(a, b));

        if pages.is_empty() {
            return Err(Error::EmptyArchive);
        }

        let comic_info = read_comic_info(&mut archive);

        Ok(Self {
            path,
            archive,
            pages,
            comic_info,
        })
    }

    /// Path the document was opened from.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Number of pages in reading order.
    pub fn page_count(&self) -> usize {
        self.pages.len()
    }

    /// Page entry names in reading order.
    pub fn page_names(&self) -> &[String] {
        &self.pages
    }

    /// Embedded `ComicInfo.xml` metadata, if present.
    pub fn comic_info(&self) -> Option<&ComicInfo> {
        self.comic_info.as_ref()
    }

    /// Display title: ComicInfo title/series if available, else file stem.
    pub fn title(&self) -> String {
        if let Some(info) = &self.comic_info {
            if let Some(title) = info.display_title() {
                return title;
            }
        }
        self.path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.path.display().to_string())
    }

    /// Read the raw (encoded) bytes of page `index`.
    pub fn read_page(&mut self, index: usize) -> Result<Vec<u8>> {
        let count = self.pages.len();
        let name = self
            .pages
            .get(index)
            .ok_or(Error::PageOutOfBounds { index, count })?
            .clone();

        let mut entry = self
            .archive
            .by_name(&name)
            .map_err(|source| Error::ReadPage {
                name: name.clone(),
                source,
            })?;
        let mut buf = Vec::with_capacity(entry.size() as usize);
        entry.read_to_end(&mut buf)?;
        Ok(buf)
    }

    /// Read and decode page `index` into an image.
    pub fn decode_page(&mut self, index: usize) -> Result<DynamicImage> {
        let bytes = self.read_page(index)?;
        let name = self.pages[index].clone();
        image::load_from_memory(&bytes).map_err(|source| Error::DecodeImage { name, source })
    }
}

/// Decide whether an archive entry is a readable page.
fn is_page_entry(name: &str) -> bool {
    if name.ends_with('/') {
        return false;
    }

    // Skip macOS resource forks and hidden files anywhere in the path.
    let mut components = name.split('/').peekable();
    while let Some(component) = components.next() {
        let is_last = components.peek().is_none();
        if component == "__MACOSX" || (component.starts_with('.') && !is_last) {
            return false;
        }
        if is_last && component.starts_with('.') {
            return false;
        }
    }

    let lower = name.to_ascii_lowercase();
    if lower.ends_with("thumbs.db") {
        return false;
    }

    match lower.rsplit_once('.') {
        Some((_, ext)) => PAGE_EXTENSIONS.contains(&ext),
        None => false,
    }
}

/// Look for a `ComicInfo.xml` entry (any casing, any directory level) and
/// parse it. Failures are silently ignored — metadata is best-effort.
fn read_comic_info(archive: &mut ZipArchive<File>) -> Option<ComicInfo> {
    let entry_name = archive
        .file_names()
        .find(|name| {
            name.rsplit('/')
                .next()
                .is_some_and(|base| base.eq_ignore_ascii_case("comicinfo.xml"))
        })?
        .to_owned();

    let mut entry = archive.by_name(&entry_name).ok()?;
    let mut xml = String::new();
    entry.read_to_string(&mut xml).ok()?;
    ComicInfo::parse(&xml).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use zip::write::SimpleFileOptions;
    use zip::ZipWriter;

    /// Build a small in-memory PNG so decoded pages are real images.
    fn tiny_png(width: u32, height: u32) -> Vec<u8> {
        let img = image::RgbImage::from_pixel(width, height, image::Rgb([120, 130, 140]));
        let mut buf = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgb8(img)
            .write_to(&mut buf, image::ImageFormat::Png)
            .unwrap();
        buf.into_inner()
    }

    fn write_cbz(path: &Path, entries: &[(&str, &[u8])]) {
        let file = File::create(path).unwrap();
        let mut zip = ZipWriter::new(file);
        for (name, data) in entries {
            zip.start_file(*name, SimpleFileOptions::default()).unwrap();
            zip.write_all(data).unwrap();
        }
        zip.finish().unwrap();
    }

    #[test]
    fn pages_are_naturally_sorted_and_filtered() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.cbz");
        let png = tiny_png(4, 4);
        write_cbz(
            &path,
            &[
                ("page10.png", png.as_slice()),
                ("page2.png", png.as_slice()),
                ("page1.png", png.as_slice()),
                ("__MACOSX/page1.png", b"junk".as_slice()),
                (".hidden.png", b"junk".as_slice()),
                ("Thumbs.db", b"junk".as_slice()),
                ("notes.txt", b"junk".as_slice()),
            ],
        );

        let doc = CbzDocument::open(&path).unwrap();
        assert_eq!(doc.page_count(), 3);
        assert_eq!(doc.page_names(), &["page1.png", "page2.png", "page10.png"]);
    }

    #[test]
    fn decode_page_returns_image_with_expected_dimensions() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.cbz");
        write_cbz(&path, &[("only.png", tiny_png(7, 9).as_slice())]);

        let mut doc = CbzDocument::open(&path).unwrap();
        let img = doc.decode_page(0).unwrap();
        assert_eq!((img.width(), img.height()), (7, 9));
    }

    #[test]
    fn out_of_bounds_page_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.cbz");
        write_cbz(&path, &[("only.png", tiny_png(2, 2).as_slice())]);

        let mut doc = CbzDocument::open(&path).unwrap();
        assert!(matches!(
            doc.decode_page(1),
            Err(Error::PageOutOfBounds { index: 1, count: 1 })
        ));
    }

    #[test]
    fn empty_archive_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty.cbz");
        write_cbz(&path, &[("readme.txt", b"no pages here".as_slice())]);

        assert!(matches!(CbzDocument::open(&path), Err(Error::EmptyArchive)));
    }

    #[test]
    fn comic_info_is_parsed_when_present() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("meta.cbz");
        let xml = r#"<?xml version="1.0"?>
            <ComicInfo>
              <Title>Chapter 1</Title>
              <Series>My Manga</Series>
              <Number>1</Number>
            </ComicInfo>"#;
        write_cbz(
            &path,
            &[
                ("ComicInfo.xml", xml.as_bytes()),
                ("p1.png", tiny_png(2, 2).as_slice()),
            ],
        );

        let doc = CbzDocument::open(&path).unwrap();
        let info = doc.comic_info().unwrap();
        assert_eq!(info.series.as_deref(), Some("My Manga"));
        assert_eq!(info.title.as_deref(), Some("Chapter 1"));
        assert_eq!(doc.title(), "My Manga — Chapter 1");
    }

    #[test]
    fn title_falls_back_to_file_stem() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("One Piece v1.cbz");
        write_cbz(&path, &[("p1.png", tiny_png(2, 2).as_slice())]);

        let doc = CbzDocument::open(&path).unwrap();
        assert_eq!(doc.title(), "One Piece v1");
    }
}
