//! Chapter downloading: turn a set of fetched page images into a `.cbz`
//! archive that gideon-core can open, mirroring how bobo stores offline
//! chapters.

use std::io::{Cursor, Write};
use std::path::Path;

use url::Url;
use zip::write::SimpleFileOptions;
use zip::ZipWriter;

use crate::fetch::Fetcher;
use crate::Result;

/// Write pages (already-encoded image bytes) into a CBZ at `out_path`.
///
/// Page names are zero-padded so the resulting archive sorts correctly in
/// any reader, not just gideon.
pub fn pages_to_cbz(out_path: &Path, pages: &[(String, Vec<u8>)]) -> Result<()> {
    if let Some(parent) = out_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let file = std::fs::File::create(out_path)?;
    let mut zip = ZipWriter::new(file);
    // Pages are already compressed images; store them instead of deflating.
    let options = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);

    for (name, bytes) in pages {
        zip.start_file(name.as_str(), options)?;
        zip.write_all(bytes)?;
    }
    zip.finish()?;
    Ok(())
}

/// Download every page URL of a chapter and pack them into a CBZ.
///
/// The page order of `page_urls` is preserved; extensions are taken from the
/// URL path (defaulting to `jpg`).
pub fn download_chapter_to_cbz(
    fetcher: &dyn Fetcher,
    page_urls: &[Url],
    out_path: &Path,
) -> Result<()> {
    let width = page_urls.len().to_string().len().max(3);
    let mut pages = Vec::with_capacity(page_urls.len());

    for (i, url) in page_urls.iter().enumerate() {
        let bytes = fetcher.get(url)?;
        let ext = url
            .path()
            .rsplit('.')
            .next()
            .filter(|e| e.len() <= 4 && e.chars().all(|c| c.is_ascii_alphanumeric()))
            .unwrap_or("jpg")
            .to_ascii_lowercase();
        pages.push((format!("{:0width$}.{ext}", i + 1, width = width), bytes));
    }

    pages_to_cbz(out_path, &pages)
}

/// Read raw bytes back out of a CBZ (helper shared by tests and callers
/// verifying downloads).
pub fn read_cbz_entries(path: &Path) -> Result<Vec<(String, Vec<u8>)>> {
    use std::io::Read;

    let bytes = std::fs::read(path)?;
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes))?;
    let mut entries = Vec::new();
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i)?;
        let mut buf = Vec::new();
        entry.read_to_end(&mut buf)?;
        entries.push((entry.name().to_string(), buf));
    }
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fetch::FakeFetcher;

    #[test]
    fn pages_round_trip_through_cbz() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("chapter.cbz");
        let pages = vec![
            ("001.jpg".to_string(), vec![1u8, 2, 3]),
            ("002.png".to_string(), vec![4u8, 5]),
        ];

        pages_to_cbz(&out, &pages).unwrap();

        let entries = read_cbz_entries(&out).unwrap();
        assert_eq!(entries, pages);
    }

    #[test]
    fn download_chapter_names_pages_in_order() {
        let fetcher = FakeFetcher::new()
            .with("https://cdn.example.com/c1/a.jpg", vec![1u8])
            .with("https://cdn.example.com/c1/b.png", vec![2u8])
            .with("https://cdn.example.com/c1/c", vec![3u8]);

        let urls = vec![
            Url::parse("https://cdn.example.com/c1/a.jpg").unwrap(),
            Url::parse("https://cdn.example.com/c1/b.png").unwrap(),
            Url::parse("https://cdn.example.com/c1/c").unwrap(),
        ];

        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("downloads/manga/ch1.cbz");
        download_chapter_to_cbz(&fetcher, &urls, &out).unwrap();

        let entries = read_cbz_entries(&out).unwrap();
        let names: Vec<&str> = entries.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, vec!["001.jpg", "002.png", "003.jpg"]);
        assert_eq!(entries[2].1, vec![3u8]);
    }

    #[test]
    fn missing_page_fails_the_download() {
        let fetcher = FakeFetcher::new();
        let urls = vec![Url::parse("https://cdn.example.com/missing.jpg").unwrap()];
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("ch.cbz");
        assert!(download_chapter_to_cbz(&fetcher, &urls, &out).is_err());
    }
}
