//! The Internet Archive's books, for an e-reader.
//!
//! archive.org's BookServer (its OPDS feed) is gone, but two plain JSON
//! endpoints are not: the advanced search and the per-item metadata. This
//! crate turns them into "search for a book, pick the EPUB, download it":
//!
//! * [`Client::search`] — `advancedsearch.php`, restricted to texts that
//!   carry an EPUB and are free to download (no lending-library items).
//! * [`Client::item`] — `/metadata/<identifier>`, for the file list; an
//!   [`Item`] knows its best EPUB and PDF.
//! * [`Client::download`] — `/download/<identifier>/<file>`, written
//!   through a temp file so a dropped connection never leaves half a book.
//!
//! Everything goes through the [`Fetcher`] trait, so the parsing is tested
//! against captured responses with no network.

use std::path::{Path, PathBuf};

use url::Url;

pub mod fetch;

pub use fetch::{FakeFetcher, Fetcher, UreqFetcher};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("the network is unreachable")]
    Offline,
    #[error("{url}: {message}")]
    Http { url: String, message: String },
    #[error("couldn't read archive.org's answer for {url}: {message}")]
    Parse { url: String, message: String },
    #[error("\"{identifier}\" has no {what} to download")]
    NoFile {
        identifier: String,
        what: &'static str,
    },
    #[error("the server sent something that is not {expected}")]
    NotTheFile { expected: &'static str },
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Url(#[from] url::ParseError),
}

pub type Result<T> = std::result::Result<T, Error>;

pub const DEFAULT_BASE: &str = "https://archive.org";

/// What to order a search by.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Sort {
    /// Most downloaded first: archive.org's relevance ranking is weak, and
    /// the popular copy of a public-domain book is usually the good one.
    #[default]
    Downloads,
    /// Newest additions first.
    Added,
    /// Alphabetical.
    Title,
}

impl Sort {
    fn param(self) -> &'static str {
        match self {
            Sort::Downloads => "downloads desc",
            Sort::Added => "publicdate desc",
            Sort::Title => "titleSorter asc",
        }
    }
}

/// How to search.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchOptions {
    pub rows: u32,
    /// 1-based.
    pub page: u32,
    pub sort: Sort,
    /// Match the words anywhere in the record (full text) rather than in
    /// the title or author only. Wider, and much noisier.
    pub anywhere: bool,
    /// Accept items that only have a PDF, not an EPUB.
    pub allow_pdf_only: bool,
}

impl Default for SearchOptions {
    fn default() -> Self {
        Self {
            rows: 25,
            page: 1,
            sort: Sort::default(),
            anywhere: false,
            allow_pdf_only: false,
        }
    }
}

/// One search hit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hit {
    pub identifier: String,
    pub title: String,
    /// Authors, joined with ", ".
    pub creator: Option<String>,
    pub year: Option<String>,
    pub language: Option<String>,
    pub downloads: u64,
    /// archive.org's format names ("EPUB", "Text PDF", …).
    pub formats: Vec<String>,
}

impl Hit {
    pub fn has_epub(&self) -> bool {
        self.formats.iter().any(|f| f.eq_ignore_ascii_case("epub"))
    }

    pub fn has_pdf(&self) -> bool {
        self.formats
            .iter()
            .any(|f| f.to_ascii_lowercase().contains("pdf"))
    }

    /// "Title — Author (year)" for a list row.
    pub fn label(&self) -> String {
        let mut s = self.title.clone();
        if let Some(c) = &self.creator {
            s.push_str(" — ");
            s.push_str(c);
        }
        if let Some(y) = &self.year {
            s.push_str(&format!(" ({y})"));
        }
        s
    }
}

/// A page of hits.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SearchPage {
    pub hits: Vec<Hit>,
    pub total: u64,
    pub page: u32,
    pub rows: u32,
}

impl SearchPage {
    pub fn has_more(&self) -> bool {
        (self.page as u64) * (self.rows as u64) < self.total
    }
}

/// One file in an item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct File {
    pub name: String,
    pub format: String,
    pub size: u64,
}

impl File {
    pub fn is_epub(&self) -> bool {
        self.format.eq_ignore_ascii_case("epub")
            || self.name.to_ascii_lowercase().ends_with(".epub")
    }

    pub fn is_pdf(&self) -> bool {
        // "Text PDF" is the real one; "Image Container PDF" and
        // "Additional Text PDF" are scans and extras.
        self.format.eq_ignore_ascii_case("text pdf")
            || (self.name.to_ascii_lowercase().ends_with(".pdf")
                && !self.format.to_ascii_lowercase().contains("container"))
    }
}

/// One archive.org item: a book and its files.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub identifier: String,
    pub title: String,
    pub creator: Option<String>,
    pub date: Option<String>,
    pub language: Option<String>,
    pub description: Option<String>,
    pub license_url: Option<String>,
    pub files: Vec<File>,
}

impl Item {
    /// The EPUB to download: archive.org generates at most one per item,
    /// but a user-uploaded item can carry several; the largest is the one
    /// with its pictures.
    pub fn best_epub(&self) -> Option<&File> {
        self.files
            .iter()
            .filter(|f| f.is_epub())
            .max_by_key(|f| f.size)
    }

    pub fn best_pdf(&self) -> Option<&File> {
        self.files
            .iter()
            .filter(|f| f.is_pdf())
            .max_by_key(|f| f.size)
    }

    pub fn label(&self) -> String {
        match &self.creator {
            Some(c) => format!("{} — {}", self.title, c),
            None => self.title.clone(),
        }
    }
}

/// The archive.org client.
pub struct Client<F: Fetcher> {
    fetcher: F,
    base: Url,
}

impl Client<UreqFetcher> {
    pub fn new() -> Self {
        Self::with_fetcher(UreqFetcher::new())
    }
}

impl Default for Client<UreqFetcher> {
    fn default() -> Self {
        Self::new()
    }
}

impl<F: Fetcher> Client<F> {
    pub fn with_fetcher(fetcher: F) -> Self {
        Self {
            fetcher,
            base: Url::parse(DEFAULT_BASE).expect("constant URL"),
        }
    }

    /// Point at another host (a mirror, or a test server).
    pub fn with_base(mut self, base: Url) -> Self {
        self.base = base;
        self
    }

    /// The URL a search goes to.
    pub fn search_url(&self, query: &str, opts: &SearchOptions) -> Result<Url> {
        let terms = clean_query(query);
        let scope = if opts.anywhere {
            format!("({terms})")
        } else {
            format!("(title:({terms}) OR creator:({terms}))")
        };
        let want = if opts.allow_pdf_only {
            "(format:epub OR format:pdf)"
        } else {
            "format:epub"
        };
        // Texts, downloadable: the lending library ("inlibrary") and
        // anything access-restricted need a borrow, which a reader cannot
        // do, so they are not offered.
        let q = format!(
            "{scope} AND mediatype:texts AND {want} AND -collection:inlibrary AND -access-restricted-item:true"
        );
        let mut url = self.base.join("/advancedsearch.php")?;
        {
            let mut qs = url.query_pairs_mut();
            qs.append_pair("q", &q);
            for field in [
                "identifier",
                "title",
                "creator",
                "year",
                "downloads",
                "language",
                "format",
            ] {
                qs.append_pair("fl[]", field);
            }
            qs.append_pair("sort[]", opts.sort.param());
            qs.append_pair("rows", &opts.rows.max(1).to_string());
            qs.append_pair("page", &opts.page.max(1).to_string());
            qs.append_pair("output", "json");
        }
        Ok(url)
    }

    /// Search for books. Hits without the wanted format (archive.org's
    /// format filter is per item and includes derivatives) are dropped.
    pub fn search(&self, query: &str, opts: &SearchOptions) -> Result<SearchPage> {
        let url = self.search_url(query, opts)?;
        let body = self.fetcher.get(&url)?;
        let mut page = parse_search(&body).map_err(|message| Error::Parse {
            url: url.to_string(),
            message,
        })?;
        page.page = opts.page.max(1);
        page.rows = opts.rows.max(1);
        page.hits
            .retain(|h| h.has_epub() || (opts.allow_pdf_only && h.has_pdf()));
        Ok(page)
    }

    /// The most downloaded free books, page by page: a "popular" shelf.
    pub fn popular(&self, page: u32, rows: u32) -> Result<SearchPage> {
        let opts = SearchOptions {
            rows,
            page,
            sort: Sort::Downloads,
            anywhere: true,
            allow_pdf_only: false,
        };
        // An empty scope matches everything; the filters do the work.
        let url = {
            let mut url = self.base.join("/advancedsearch.php")?;
            let mut qs = url.query_pairs_mut();
            qs.append_pair(
                "q",
                "mediatype:texts AND format:epub AND -collection:inlibrary AND -access-restricted-item:true AND language:(eng OR English)",
            );
            for field in [
                "identifier",
                "title",
                "creator",
                "year",
                "downloads",
                "language",
                "format",
            ] {
                qs.append_pair("fl[]", field);
            }
            qs.append_pair("sort[]", opts.sort.param());
            qs.append_pair("rows", &rows.max(1).to_string());
            qs.append_pair("page", &page.max(1).to_string());
            qs.append_pair("output", "json");
            drop(qs);
            url
        };
        let body = self.fetcher.get(&url)?;
        let mut out = parse_search(&body).map_err(|message| Error::Parse {
            url: url.to_string(),
            message,
        })?;
        out.page = page.max(1);
        out.rows = rows.max(1);
        out.hits.retain(Hit::has_epub);
        Ok(out)
    }

    pub fn item_url(&self, identifier: &str) -> Result<Url> {
        Ok(self
            .base
            .join(&format!("/metadata/{}", encode_segment(identifier)))?)
    }

    /// One item with its files.
    pub fn item(&self, identifier: &str) -> Result<Item> {
        let url = self.item_url(identifier)?;
        let body = self.fetcher.get(&url)?;
        parse_item(&body, identifier).map_err(|message| Error::Parse {
            url: url.to_string(),
            message,
        })
    }

    /// Where a file of an item downloads from.
    pub fn download_url(&self, identifier: &str, file_name: &str) -> Result<Url> {
        let path = format!(
            "/download/{}/{}",
            encode_segment(identifier),
            encode_segment(file_name)
        );
        Ok(self.base.join(&path)?)
    }

    /// Download one file of an item into `dir`, named after the book
    /// (`Author/Title.ext`, made filename-safe). Checks the bytes look like
    /// the format claimed; written through a temp file. Returns the path and
    /// whether it was already there (nothing fetched).
    pub fn download(&self, item: &Item, file: &File, dir: &Path) -> Result<(PathBuf, bool)> {
        let ext = if file.is_epub() { "epub" } else { "pdf" };
        let dest = book_path(dir, item.creator.as_deref(), &item.title, ext);
        if dest.exists() {
            return Ok((dest, true));
        }
        let url = self.download_url(&item.identifier, &file.name)?;
        let bytes = self.fetcher.get(&url)?;
        let ok = match ext {
            "epub" => bytes.starts_with(b"PK"),
            _ => bytes.starts_with(b"%PDF"),
        };
        if !ok {
            return Err(Error::NotTheFile {
                expected: if ext == "epub" { "an EPUB" } else { "a PDF" },
            });
        }
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = dest.with_extension(format!("{ext}.part"));
        std::fs::write(&tmp, &bytes)?;
        std::fs::rename(&tmp, &dest)?;
        Ok((dest, false))
    }
}

/// Where a book lands: `dir/Author/Title.ext`, the layout gideon's shelf
/// reads. Nickel and gideon both take the real title from inside the file,
/// so the path only has to be safe and stable.
pub fn book_path(dir: &Path, author: Option<&str>, title: &str, ext: &str) -> PathBuf {
    let author = author
        .map(str::trim)
        .filter(|a| !a.is_empty())
        .unwrap_or("Unknown author");
    dir.join(safe_name(author))
        .join(format!("{}.{ext}", safe_name(title)))
}

/// A string that is safe as a single path component on FAT (the Kobo's
/// user partition) and on every desktop: the reserved characters become
/// spaces, runs of whitespace collapse, and the length is capped.
pub fn safe_name(s: &str) -> String {
    let cleaned: String = s
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => ' ',
            c if c.is_control() => ' ',
            c => c,
        })
        .collect();
    let collapsed = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    let trimmed = collapsed.trim_matches(|c: char| c == '.' || c == ' ');
    let capped: String = trimmed.chars().take(120).collect();
    if capped.is_empty() {
        "untitled".to_string()
    } else {
        capped
    }
}

/// Lucene's reserved characters would turn a title into a syntax error;
/// archive.org wants plain words.
fn clean_query(q: &str) -> String {
    let cleaned: String = q
        .chars()
        .map(|c| match c {
            '+' | '-' | '&' | '|' | '!' | '(' | ')' | '{' | '}' | '[' | ']' | '^' | '"' | '~'
            | '*' | '?' | ':' | '\\' | '/' => ' ',
            c => c,
        })
        .collect();
    let words: Vec<&str> = cleaned.split_whitespace().collect();
    if words.is_empty() {
        "*".to_string()
    } else {
        words.join(" ")
    }
}

fn encode_segment(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes())
        .collect::<String>()
        .replace('+', "%20")
}

// --- parsing -----------------------------------------------------------------

fn str_or_list(v: Option<&serde_json::Value>) -> Option<String> {
    match v {
        Some(serde_json::Value::String(s)) => Some(s.trim().to_string()).filter(|s| !s.is_empty()),
        Some(serde_json::Value::Array(items)) => {
            let parts: Vec<String> = items
                .iter()
                .filter_map(|i| i.as_str())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            (!parts.is_empty()).then(|| parts.join(", "))
        }
        Some(serde_json::Value::Number(n)) => Some(n.to_string()),
        _ => None,
    }
}

fn list(v: Option<&serde_json::Value>) -> Vec<String> {
    match v {
        Some(serde_json::Value::String(s)) => vec![s.clone()],
        Some(serde_json::Value::Array(items)) => items
            .iter()
            .filter_map(|i| i.as_str().map(str::to_string))
            .collect(),
        _ => Vec::new(),
    }
}

/// Parse an `advancedsearch.php?output=json` body.
pub fn parse_search(body: &[u8]) -> std::result::Result<SearchPage, String> {
    let v: serde_json::Value = serde_json::from_slice(body).map_err(|e| e.to_string())?;
    let response = v.get("response").ok_or("no \"response\" object")?;
    let total = response
        .get("numFound")
        .and_then(|n| n.as_u64())
        .unwrap_or(0);
    let docs = response
        .get("docs")
        .and_then(|d| d.as_array())
        .ok_or("no \"docs\" list")?;
    let hits = docs
        .iter()
        .filter_map(|d| {
            let identifier = d.get("identifier")?.as_str()?.to_string();
            Some(Hit {
                title: str_or_list(d.get("title")).unwrap_or_else(|| identifier.clone()),
                creator: str_or_list(d.get("creator")),
                year: str_or_list(d.get("year")),
                language: str_or_list(d.get("language")),
                downloads: d.get("downloads").and_then(|n| n.as_u64()).unwrap_or(0),
                formats: list(d.get("format")),
                identifier,
            })
        })
        .collect();
    Ok(SearchPage {
        hits,
        total,
        page: 1,
        rows: 0,
    })
}

/// Parse a `/metadata/<identifier>` body.
pub fn parse_item(body: &[u8], identifier: &str) -> std::result::Result<Item, String> {
    let v: serde_json::Value = serde_json::from_slice(body).map_err(|e| e.to_string())?;
    if let Some(err) = v.get("error").and_then(|e| e.as_str()) {
        return Err(err.to_string());
    }
    let meta = v
        .get("metadata")
        .ok_or("no \"metadata\" object (no such item?)")?;
    let files = v
        .get("files")
        .and_then(|f| f.as_array())
        .map(|files| {
            files
                .iter()
                .filter_map(|f| {
                    let name = f.get("name")?.as_str()?.to_string();
                    let size = match f.get("size") {
                        Some(serde_json::Value::String(s)) => s.parse().unwrap_or(0),
                        Some(serde_json::Value::Number(n)) => n.as_u64().unwrap_or(0),
                        _ => 0,
                    };
                    Some(File {
                        name,
                        format: f
                            .get("format")
                            .and_then(|s| s.as_str())
                            .unwrap_or("")
                            .to_string(),
                        size,
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(Item {
        identifier: str_or_list(meta.get("identifier")).unwrap_or_else(|| identifier.to_string()),
        title: str_or_list(meta.get("title")).unwrap_or_else(|| identifier.to_string()),
        creator: str_or_list(meta.get("creator")),
        date: str_or_list(meta.get("date")).or_else(|| str_or_list(meta.get("year"))),
        language: str_or_list(meta.get("language")),
        description: str_or_list(meta.get("description")),
        license_url: str_or_list(meta.get("licenseurl")),
        files,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queries_are_scoped_and_cleaned() {
        let c = Client::with_fetcher(FakeFetcher::new());
        let url = c
            .search_url(
                "Slaughterhouse-Five: or, the (Children's) Crusade",
                &SearchOptions::default(),
            )
            .unwrap();
        let q = url
            .query_pairs()
            .find(|(k, _)| k == "q")
            .map(|(_, v)| v.into_owned())
            .unwrap();
        assert!(
            q.starts_with("(title:(Slaughterhouse Five or, the Children's Crusade) OR creator:(")
        );
        assert!(q.contains("-collection:inlibrary"));
        assert!(q.contains("format:epub"));
        assert!(url
            .query_pairs()
            .any(|(k, v)| k == "sort[]" && v == "downloads desc"));
        assert_eq!(clean_query("  "), "*");
    }

    #[test]
    fn paths_are_safe_for_a_kobo() {
        assert_eq!(safe_name("Kurt Vonnegut, Jr."), "Kurt Vonnegut, Jr");
        assert_eq!(safe_name("What/If: A Book?"), "What If A Book");
        assert_eq!(safe_name("..."), "untitled");
        let p = book_path(Path::new("/b"), None, "X", "epub");
        assert_eq!(p, Path::new("/b/Unknown author/X.epub"));
    }

    #[test]
    fn download_urls_encode_spaces_as_percent_20() {
        let c = Client::with_fetcher(FakeFetcher::new());
        let url = c
            .download_url(
                "slaughterhouse-five-kurt-vonnegut",
                "Slaughterhouse Five - Kurt Vonnegut.epub",
            )
            .unwrap();
        assert_eq!(
            url.as_str(),
            "https://archive.org/download/slaughterhouse-five-kurt-vonnegut/Slaughterhouse%20Five%20-%20Kurt%20Vonnegut.epub"
        );
    }
}
