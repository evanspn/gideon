//! The books shelf: what Nickel reads, listed by gideon.
//!
//! A book — an EPUB or a PDF — lands in the Books folder from an OPDS
//! download or a phone send, and the Kobo's own reader opens it. gideon
//! cannot render one, but it can SHOW them: the Library's Books tab is this
//! scan, so a book you just sent is visible where you went looking for it,
//! not only after a reboot into the stock home screen.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::natsort::natural_cmp;
use crate::upload::{epub_meta_file, nice_name};

/// One book on the shelf.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Book {
    pub path: PathBuf,
    /// Path relative to the books folder, `/`-separated.
    pub relative_path: String,
    /// The book's own title (EPUB metadata), else a readable form of the
    /// file name.
    pub title: String,
    /// Author from the metadata, else the folder the book sits in (the
    /// `Books/<Author>/<Title>` layout gideon writes), else none.
    pub author: Option<String>,
    /// "epub" or "pdf".
    pub format: &'static str,
    pub size: u64,
}

impl Book {
    /// "Title — Author" or just the title, for a one-line row.
    pub fn label(&self) -> String {
        match &self.author {
            Some(a) => format!("{} — {}", self.title, a),
            None => self.title.clone(),
        }
    }
}

/// Every EPUB and PDF under `dir` (recursively), sorted by title then
/// author in natural order. A missing folder is an empty shelf, not an
/// error: nothing has been sent yet.
pub fn scan(dir: &Path) -> std::io::Result<Vec<Book>> {
    let mut books = Vec::new();
    if !dir.exists() {
        return Ok(books);
    }
    walk(dir, dir, &mut books)?;
    books.sort_by(|a, b| {
        natural_cmp(&a.title, &b.title).then_with(|| {
            natural_cmp(
                a.author.as_deref().unwrap_or(""),
                b.author.as_deref().unwrap_or(""),
            )
        })
    });
    Ok(books)
}

fn walk(root: &Path, dir: &Path, out: &mut Vec<Book>) -> std::io::Result<()> {
    let mut entries: Vec<_> = std::fs::read_dir(dir)?.flatten().collect();
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        // Nickel's own bookkeeping (`.kobo-images`, `.adobe-digital-editions`)
        // and gideon's are not books.
        if name.starts_with('.') {
            continue;
        }
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_dir() {
            walk(root, &path, out)?;
            continue;
        }
        if !kind.is_file() {
            continue;
        }
        let format = match path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase())
            .as_deref()
        {
            Some("epub") => "epub",
            Some("pdf") => "pdf",
            _ => continue,
        };
        out.push(describe(root, &path, format));
    }
    Ok(())
}

fn describe(root: &Path, path: &Path, format: &'static str) -> Book {
    let relative_path = path
        .strip_prefix(root)
        .unwrap_or(path)
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/");
    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let meta = if format == "epub" {
        epub_meta_file(path)
    } else {
        Default::default()
    };
    let title = meta
        .title
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| nice_name(&file_name));
    // The folder a book sits in under Books/ is its author in the layout
    // gideon writes; a PDF has no metadata of its own to say otherwise.
    let folder_author = path
        .parent()
        .filter(|p| *p != root)
        .and_then(|p| p.strip_prefix(root).ok())
        .and_then(|p| p.components().next())
        .map(|c| c.as_os_str().to_string_lossy().into_owned());
    let author = meta
        .author
        .as_deref()
        .map(str::trim)
        .filter(|a| !a.is_empty())
        .map(str::to_string)
        .or(folder_author)
        .or_else(|| file_name_author(&file_name));
    let size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    Book {
        path: path.to_path_buf(),
        relative_path,
        title,
        author,
        format,
        size,
    }
}

/// The author a wild file name carries: the second " -- " segment of
/// "Title -- Author -- Publisher, Year -- …". Only a fallback for a loose
/// file with no metadata and no author folder; `None` when the name has no
/// such segment or it reads like a year or publisher rather than a person.
fn file_name_author(file_name: &str) -> Option<String> {
    let stem = Path::new(file_name).file_stem()?.to_str()?;
    let second = stem.split(" -- ").nth(1)?.trim();
    let looks_like_name =
        !second.is_empty() && second.len() <= 80 && !second.chars().all(|c| c.is_ascii_digit());
    looks_like_name.then(|| second.to_string())
}

/// Where the reader left off in a book: a chapter and a paragraph in it.
/// Paragraph, not page — pages depend on the text size, and changing it
/// must not lose the place.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct BookPlace {
    pub chapter: usize,
    pub block: usize,
    /// The wrapped line within the paragraph at the top of the page: a long
    /// paragraph spans pages, and this says which of them.
    #[serde(default)]
    pub line: usize,
    /// Chapters in the book when the place was saved, for "n% read".
    #[serde(default)]
    pub chapters: usize,
    #[serde(default)]
    pub updated_at: u64,
}

/// Every book's place, keyed by its path relative to the Books folder.
/// Lives in a dotfile beside the books, which Nickel ignores.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BookPlaces {
    #[serde(default)]
    pub places: BTreeMap<String, BookPlace>,
}

impl BookPlaces {
    pub const FILE: &'static str = ".gideon-places.json";

    pub fn load(books_dir: &Path) -> Self {
        std::fs::read(books_dir.join(Self::FILE))
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, books_dir: &Path) -> std::io::Result<()> {
        std::fs::create_dir_all(books_dir)?;
        let path = books_dir.join(Self::FILE);
        let tmp = books_dir.join(".gideon-places.json.part");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self).unwrap_or_default())?;
        std::fs::rename(tmp, path)
    }

    pub fn get(&self, relative_path: &str) -> Option<BookPlace> {
        self.places.get(relative_path).copied()
    }

    pub fn set(&mut self, relative_path: &str, place: BookPlace) {
        self.places.insert(relative_path.to_string(), place);
    }
}

/// Delete a book and, when that leaves its author folder empty, the folder
/// — so the shelf never shows a nameless author with nothing under them.
pub fn delete(root: &Path, book: &Book) -> std::io::Result<()> {
    std::fs::remove_file(&book.path)?;
    let mut places = BookPlaces::load(root);
    if places.places.remove(&book.relative_path).is_some() {
        let _ = places.save(root);
    }
    if let Some(parent) = book.path.parent() {
        if parent != root
            && std::fs::read_dir(parent)
                .map(|mut d| d.next().is_none())
                .unwrap_or(false)
        {
            let _ = std::fs::remove_dir(parent);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn epub(path: &Path, title: &str, author: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let file = std::fs::File::create(path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        let stored = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        zip.start_file("mimetype", stored).unwrap();
        zip.write_all(b"application/epub+zip").unwrap();
        let opts = zip::write::SimpleFileOptions::default();
        zip.start_file("META-INF/container.xml", opts).unwrap();
        zip.write_all(
            br#"<?xml version="1.0"?><container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container"><rootfiles><rootfile full-path="OEBPS/content.opf" media-type="application/oebps-package+xml"/></rootfiles></container>"#,
        )
        .unwrap();
        zip.start_file("OEBPS/content.opf", opts).unwrap();
        write!(
            zip,
            r#"<?xml version="1.0"?><package xmlns="http://www.idpf.org/2007/opf" xmlns:dc="http://purl.org/dc/elements/1.1/" version="3.0"><metadata><dc:title>{title}</dc:title><dc:creator>{author}</dc:creator></metadata></package>"#
        )
        .unwrap();
        zip.finish().unwrap();
    }

    #[test]
    fn missing_folder_is_an_empty_shelf() {
        let dir = tempfile::tempdir().unwrap();
        assert!(scan(&dir.path().join("Books")).unwrap().is_empty());
    }

    #[test]
    fn lists_epubs_and_pdfs_by_metadata_then_file_name() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        epub(
            &root.join("Kurt Vonnegut/Slaughterhouse-Five.epub"),
            "Slaughterhouse-Five",
            "Kurt Vonnegut",
        );
        // A bare PDF: no metadata, so the file name is the title and the
        // folder is the author.
        std::fs::create_dir_all(root.join("Natsu Hyuuga")).unwrap();
        std::fs::write(
            root.join("Natsu Hyuuga/The Apothecary Diaries, Vol. 1.pdf"),
            b"%PDF-1.4",
        )
        .unwrap();
        // Not books: a cover image, Nickel's hidden folder, a stray text file.
        std::fs::write(root.join("Kurt Vonnegut/cover.jpg"), b"x").unwrap();
        std::fs::create_dir_all(root.join(".kobo-images")).unwrap();
        std::fs::write(root.join(".kobo-images/a.epub"), b"x").unwrap();
        std::fs::write(root.join("notes.txt"), b"x").unwrap();

        let books = scan(root).unwrap();
        let labels: Vec<_> = books.iter().map(Book::label).collect();
        assert_eq!(
            labels,
            [
                "Slaughterhouse-Five — Kurt Vonnegut",
                "The Apothecary Diaries, Vol. 1 — Natsu Hyuuga",
            ]
        );
        assert_eq!(books[0].format, "epub");
        assert_eq!(
            books[0].relative_path,
            "Kurt Vonnegut/Slaughterhouse-Five.epub"
        );
        assert_eq!(books[1].format, "pdf");
        assert_eq!(books[1].size, 8);
    }

    #[test]
    fn loose_book_without_metadata_takes_a_readable_file_name() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path()
                .join("Slaughterhouse-Five -- Kurt Vonnegut -- 1969 -- Anna's Archive.pdf"),
            b"%PDF",
        )
        .unwrap();
        let books = scan(dir.path()).unwrap();
        assert_eq!(books[0].title, "Slaughterhouse-Five");
        assert_eq!(books[0].author.as_deref(), Some("Kurt Vonnegut"));

        std::fs::write(dir.path().join("Plain Title.pdf"), b"%PDF").unwrap();
        std::fs::write(dir.path().join("Dated -- 1969.pdf"), b"%PDF").unwrap();
        let books = scan(dir.path()).unwrap();
        let by_title = |t: &str| books.iter().find(|b| b.title == t).unwrap().author.clone();
        assert_eq!(by_title("Plain Title"), None);
        assert_eq!(by_title("Dated"), None, "a year is not an author");
    }

    #[test]
    fn places_round_trip_and_follow_a_deleted_book_out() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        epub(&root.join("A/one.epub"), "One", "A");
        let mut places = BookPlaces::load(root);
        places.set(
            "A/one.epub",
            BookPlace {
                chapter: 3,
                block: 12,
                line: 0,
                chapters: 20,
                updated_at: 1,
            },
        );
        places.save(root).unwrap();
        assert_eq!(BookPlaces::load(root).get("A/one.epub").unwrap().block, 12);
        // The dotfile is not a book.
        assert_eq!(scan(root).unwrap().len(), 1);
        let one = scan(root).unwrap().remove(0);
        delete(root, &one).unwrap();
        assert!(BookPlaces::load(root).get("A/one.epub").is_none());
    }

    #[test]
    fn delete_removes_the_book_and_its_emptied_author_folder() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        epub(&root.join("A/one.epub"), "One", "A");
        epub(&root.join("B/two.epub"), "Two", "B");
        epub(&root.join("B/three.epub"), "Three", "B");
        let books = scan(root).unwrap();
        let one = books.iter().find(|b| b.title == "One").unwrap();
        delete(root, one).unwrap();
        assert!(!root.join("A").exists(), "emptied author folder goes too");
        let three = books.iter().find(|b| b.title == "Three").unwrap();
        delete(root, three).unwrap();
        assert!(root.join("B/two.epub").exists());
        assert!(root.join("B").exists(), "a folder with books left stays");
    }
}
