//! OPDS 1.x catalogues: how gideon finds and fetches *books* (EPUBs), as
//! opposed to manga, which come through Aidoku sources.
//!
//! OPDS is an Atom feed with two kinds of entry: *navigation* entries that
//! link to another feed ("Popular", "By author…"), and *acquisition* entries
//! that link to a downloadable file. Catalogues disagree on where the file
//! link lives — Calibre puts the EPUB link straight on the entry in a list,
//! while Project Gutenberg lists stub entries that each link to a per-book
//! feed holding the real links — so [`Entry`] carries both and [`open_entry`]
//! sorts out which it is when tapped.
//!
//! Parsing is separate from fetching so it can be unit-tested against
//! captured feeds with no network (`tests/fixtures/opds/`).

use std::path::{Path, PathBuf};

use quick_xml::events::{BytesStart, Event};
use quick_xml::Reader;
use url::Url;

use crate::fetch::Fetcher;
use crate::storage::sanitize;
use crate::{Error, Result};

/// One parsed feed.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Feed {
    pub title: String,
    pub entries: Vec<Entry>,
    /// The next page of this feed, when it is paginated (`rel="next"`).
    pub next: Option<Url>,
    /// How to search this catalogue, when it says.
    pub search: Option<SearchLink>,
    /// EPUB links attached to the feed itself rather than an entry — rare,
    /// but a catalogue that serves a single book this way exists.
    pub epubs: Vec<EpubLink>,
    /// How many entries the whole result has across every page, when the
    /// catalogue says (the Internet Archive's search does).
    pub total: Option<u64>,
}

/// One entry: a book, a sub-catalogue, or (Gutenberg) a stub for a book.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Entry {
    pub title: String,
    /// The line under the title. For a book that is its author(s); for a
    /// section it is the catalogue's blurb ("Our most popular books."). Both
    /// come from the same places in the feed — `<author><name>` or a short
    /// plain-text `<content>` — and a stub cannot be told from a section
    /// until it is opened, so one field serves both and the label shows
    /// whichever it is. A downloaded book's folder is named from it, which
    /// is right for every stub a catalogue was seen to serve.
    pub detail: Option<String>,
    /// Direct EPUB download links, best first (see [`pick_epub`]).
    pub epubs: Vec<EpubLink>,
    /// Another feed this entry opens — a sub-catalogue or a per-book feed.
    pub feed: Option<Url>,
    /// Publication year, when the catalogue says (the Internet Archive does;
    /// OPDS feeds leave it out).
    pub year: Option<String>,
    /// Language, when the catalogue says.
    pub language: Option<String>,
}

impl Entry {
    /// The row label: `Title — detail` when there is a detail line.
    pub fn label(&self) -> String {
        match &self.detail {
            Some(a) if !a.is_empty() => format!("{} — {}", self.title, a),
            _ => self.title.clone(),
        }
    }
}

/// An `application/epub+zip` acquisition link.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EpubLink {
    pub url: Url,
    /// The link's own title ("EPUB (no images)"), when given — used to
    /// prefer the edition with pictures.
    pub title: Option<String>,
    /// The file's size in bytes, when known before downloading.
    pub size: Option<u64>,
}

/// Where a catalogue's search lives.
#[derive(Debug, Clone, PartialEq)]
pub enum SearchLink {
    /// An Atom URL template with `{searchTerms}` in it (Calibre style).
    Template(String),
    /// An OpenSearch description document to fetch the template from
    /// (Gutenberg style).
    Description(Url),
}

/// What tapping an entry turned out to be.
#[derive(Debug, Clone, PartialEq)]
pub enum Opened {
    /// A book, with the EPUB to download.
    Book(EpubLink),
    /// A sub-catalogue to list.
    Feed(Feed),
}

/// Fetch and parse the feed at `url`.
pub fn fetch_feed(fetcher: &dyn Fetcher, url: &Url) -> Result<Feed> {
    let body = fetcher.get(url)?;
    parse_feed(&body, url)
}

/// Fetch the one page after `feed`, if there is one.
pub fn fetch_next(fetcher: &dyn Fetcher, feed: &Feed) -> Result<Option<Feed>> {
    match &feed.next {
        Some(next) => fetch_feed(fetcher, next).map(Some),
        None => Ok(None),
    }
}

/// Resolve a feed's search link to a URL template, fetching the OpenSearch
/// description when that is what the feed pointed at. `Ok(None)` means the
/// catalogue offers no Atom search.
pub fn fetch_search_template(fetcher: &dyn Fetcher, link: &SearchLink) -> Result<Option<String>> {
    match link {
        SearchLink::Template(t) => Ok(Some(t.clone())),
        SearchLink::Description(url) => {
            let body = fetcher.get(url)?;
            Ok(parse_opensearch_template(&body))
        }
    }
}

/// Run a search: fetch the template if needed, fill in the query, fetch the
/// results. `Ok(None)` when the catalogue cannot be searched.
pub fn search(fetcher: &dyn Fetcher, link: &SearchLink, query: &str) -> Result<Option<Feed>> {
    let Some(template) = fetch_search_template(fetcher, link)? else {
        return Ok(None);
    };
    let url = search_url(&template, query)?;
    fetch_feed(fetcher, &url).map(Some)
}

/// Open an entry: a direct EPUB is a book; otherwise its feed is fetched.
/// That feed is the book itself when the EPUB links in it hang off entries
/// titled like the one tapped (Gutenberg's per-book feed lists the book's
/// editions, each titled "Frankenstein…"), and a sub-catalogue to list when
/// not (Calibre's "Newest" has an EPUB on every entry, but they are other
/// books' titles — and a sub-catalogue can share its row's name, so the
/// feed's own title decides nothing).
pub fn open_entry(fetcher: &dyn Fetcher, entry: &Entry) -> Result<Opened> {
    if let Some(epub) = pick_epub(&entry.epubs) {
        return Ok(Opened::Book(epub.clone()));
    }
    let Some(url) = &entry.feed else {
        return Err(Error::Fetch {
            url: String::new(),
            message: format!(
                "\"{}\" has neither a download nor a page to open",
                entry.title
            ),
        });
    };
    let feed = fetch_feed(fetcher, url)?;
    let this_book: Vec<EpubLink> = feed
        .epubs
        .iter()
        .chain(
            feed.entries
                .iter()
                .filter(|e| same_title(&e.title, &entry.title))
                .flat_map(|e| e.epubs.iter()),
        )
        .cloned()
        .collect();
    if let Some(epub) = pick_epub(&this_book) {
        return Ok(Opened::Book(epub.clone()));
    }
    Ok(Opened::Feed(feed))
}

/// Whether two entry titles name the same book: equal once trimmed and
/// lower-cased, or one extending the other ("Frankenstein" / "Frankenstein;
/// or, The modern Prometheus"), as editions in one catalogue tend to be.
fn same_title(a: &str, b: &str) -> bool {
    let a = a.trim().to_lowercase();
    let b = b.trim().to_lowercase();
    !a.is_empty() && !b.is_empty() && (a.starts_with(&b) || b.starts_with(&a))
}

/// The EPUB to download out of a book's links: Gutenberg offers the same
/// book with and without its illustrations, and nobody wants the one
/// without. Anything not marked "no images" wins; otherwise the first.
pub fn pick_epub(links: &[EpubLink]) -> Option<&EpubLink> {
    let no_images = |l: &EpubLink| {
        let t = l.title.as_deref().unwrap_or("").to_lowercase();
        t.contains("no images") || t.contains("noimages") || l.url.path().contains("noimages")
    };
    links
        .iter()
        .find(|l| !no_images(l))
        .or_else(|| links.first())
}

/// Fill an OpenSearch template: `{searchTerms}` becomes the encoded query
/// and any optional parameter (`{startPage?}`) is dropped.
pub fn search_url(template: &str, query: &str) -> Result<Url> {
    let encoded: String = url::form_urlencoded::byte_serialize(query.trim().as_bytes()).collect();
    let mut out = String::with_capacity(template.len() + encoded.len());
    let mut rest = template;
    while let Some(start) = rest.find('{') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        let Some(end) = after.find('}') else {
            out.push_str(&rest[start..]);
            rest = "";
            break;
        };
        let name = &after[..end];
        if name == "searchTerms" {
            out.push_str(&encoded);
        }
        // Any other parameter — required or optional — has no value we can
        // supply, so it is left empty; servers treat that as "default".
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Ok(Url::parse(&out)?)
}

/// Where a book lands under the books directory: `Author/Title.epub`, both
/// made filename-safe. Nickel reads the real title and author from inside
/// the EPUB, so the path only has to be stable and unsurprising.
pub fn epub_path(books_dir: &Path, author: Option<&str>, title: &str) -> PathBuf {
    let author = author
        .map(str::trim)
        .filter(|a| !a.is_empty())
        .unwrap_or("Unknown author");
    books_dir
        .join(sanitize(author))
        .join(format!("{}.epub", sanitize(title)))
}

/// Download `epub` to its place under `books_dir`. Returns the path and
/// whether it was already there (in which case nothing is fetched — a book
/// already on the device is the one failure mode a reader can't tell apart
/// from "it worked", so it is reported, not hidden).
///
/// Written through a temp file and renamed, so a download cut off by Wi-Fi
/// dropping never leaves a half-EPUB where Nickel would find and index it.
pub fn download_epub(
    fetcher: &dyn Fetcher,
    epub: &Url,
    books_dir: &Path,
    author: Option<&str>,
    title: &str,
) -> Result<(PathBuf, bool)> {
    let dest = epub_path(books_dir, author, title);
    if dest.exists() {
        return Ok((dest, true));
    }
    let bytes = fetcher.get(epub)?;
    // A ZIP (which an EPUB is) starts with "PK". Anything else is an error
    // page that came back with a 200 — Gutenberg's rate limiter does this —
    // and must not be saved as a book.
    if !bytes.starts_with(b"PK") {
        return Err(Error::Fetch {
            url: epub.to_string(),
            message: "the server sent something that is not an EPUB".to_string(),
        });
    }
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = dest.with_extension("epub.part");
    std::fs::write(&tmp, &bytes)?;
    std::fs::rename(&tmp, &dest)?;
    Ok((dest, false))
}

// --- parsing ----------------------------------------------------------------

/// Parse an OPDS/Atom feed. `base` resolves relative `href`s (Gutenberg's
/// are all relative).
pub fn parse_feed(xml: &[u8], base: &Url) -> Result<Feed> {
    let parse_err = |message: String| Error::ParseFeed {
        url: base.to_string(),
        message,
    };
    let mut reader = Reader::from_reader(xml);
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();

    let mut feed = Feed::default();
    // Element path from the root, by local name — "feed/entry/author/name".
    let mut path: Vec<String> = Vec::new();
    let mut entry: Option<Entry> = None;
    let mut authors: Vec<String> = Vec::new();
    // Text of the entry's <content type="text">: Gutenberg puts the author
    // there on its stub entries. Any other content type is prose.
    let mut content_text: Option<String> = None;
    let mut content_is_text = false;

    loop {
        let event = reader
            .read_event_into(&mut buf)
            .map_err(|e| parse_err(e.to_string()))?;
        match event {
            Event::Start(e) => {
                let name = local(&e);
                if name == "entry" && path.len() == 1 {
                    entry = Some(Entry::default());
                    authors.clear();
                    content_text = None;
                }
                if name == "content" {
                    content_is_text = attr(&e, "type")
                        .map(|t| t == "text" || t.is_empty())
                        .unwrap_or(true);
                }
                if name == "link" {
                    // <link> with children is unusual but legal.
                    handle_link(&e, base, &mut feed, entry.as_mut());
                }
                path.push(name);
            }
            Event::Empty(e) => {
                if local(&e) == "link" {
                    handle_link(&e, base, &mut feed, entry.as_mut());
                }
            }
            Event::Text(t) => {
                let text = t.unescape().map_err(|e| parse_err(e.to_string()))?;
                absorb_text(
                    &path,
                    &text,
                    &mut feed,
                    entry.as_mut(),
                    &mut authors,
                    &mut content_text,
                    content_is_text,
                );
            }
            Event::CData(c) => {
                let text = String::from_utf8_lossy(&c);
                absorb_text(
                    &path,
                    &text,
                    &mut feed,
                    entry.as_mut(),
                    &mut authors,
                    &mut content_text,
                    content_is_text,
                );
            }
            Event::End(_) => {
                let name = path.pop().unwrap_or_default();
                if name == "entry" && path.len() == 1 {
                    if let Some(mut done) = entry.take() {
                        if !authors.is_empty() {
                            done.detail = Some(authors.join(", "));
                        } else if let Some(c) = content_text.take() {
                            // A short plain-text content is the author line
                            // on Gutenberg's book stubs and the one-line
                            // blurb on its sections; a long one is prose.
                            let c = c.trim();
                            if !c.is_empty() && c.chars().count() <= 120 && !c.contains('\n') {
                                done.detail = Some(c.to_string());
                            }
                        }
                        let title_ok = !done.title.trim().is_empty();
                        let has_target = done.feed.is_some() || !done.epubs.is_empty();
                        if title_ok && has_target {
                            feed.entries.push(done);
                        }
                    }
                }
            }
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
    }
    if path.first().map(String::as_str) == Some("feed")
        || feed.title.is_empty() && feed.entries.is_empty() && feed.next.is_none()
    {
        // Fell off the end mid-document, or nothing recognisable at all.
        if feed.entries.is_empty() && feed.title.is_empty() {
            return Err(parse_err("not an OPDS feed".to_string()));
        }
    }
    Ok(feed)
}

#[allow(clippy::too_many_arguments)]
fn absorb_text(
    path: &[String],
    text: &str,
    feed: &mut Feed,
    entry: Option<&mut Entry>,
    authors: &mut Vec<String>,
    content_text: &mut Option<String>,
    content_is_text: bool,
) {
    let at = |want: &[&str]| path.len() == want.len() && path.iter().zip(want).all(|(a, b)| a == b);
    if at(&["feed", "title"]) {
        feed.title = text.trim().to_string();
        return;
    }
    let Some(entry) = entry else { return };
    if at(&["feed", "entry", "title"]) {
        // A title split by an inline element arrives in pieces.
        if !entry.title.is_empty() {
            entry.title.push(' ');
        }
        entry.title.push_str(text.trim());
    } else if at(&["feed", "entry", "author", "name"]) {
        let a = text.trim();
        if !a.is_empty() {
            authors.push(a.to_string());
        }
    } else if at(&["feed", "entry", "content"]) && content_is_text {
        content_text.get_or_insert_with(String::new).push_str(text);
    }
}

/// Classify one `<link>` and file it on the entry (inside one) or the feed.
fn handle_link(e: &BytesStart<'_>, base: &Url, feed: &mut Feed, entry: Option<&mut Entry>) {
    let Some(href) = attr(e, "href") else { return };
    if href.starts_with("data:") {
        return;
    }
    let Ok(url) = base.join(&href) else { return };
    let rel = attr(e, "rel").unwrap_or_default();
    let kind = attr(e, "type").unwrap_or_default();
    let title = attr(e, "title").filter(|t| !t.is_empty());

    let is_atom = kind.starts_with("application/atom+xml");
    let is_epub = kind.starts_with("application/epub+zip");
    let is_acquisition = rel.contains("opds-spec.org/acquisition")
        && !rel.ends_with("/buy")
        && !rel.ends_with("/borrow")
        && !rel.ends_with("/sample")
        && !rel.ends_with("/subscribe");

    match entry {
        Some(entry) => {
            if is_epub && is_acquisition {
                entry.epubs.push(EpubLink {
                    url,
                    title,
                    size: None,
                });
            } else if is_atom
                && entry.feed.is_none()
                && !matches!(
                    rel.as_str(),
                    "self"
                        | "alternate"
                        | "related"
                        | "search"
                        | "next"
                        | "previous"
                        | "start"
                        | "up"
                )
            {
                entry.feed = Some(url);
            }
        }
        None => {
            if rel == "next" && is_atom {
                feed.next = Some(url);
            } else if rel == "search" {
                if kind.starts_with("application/opensearchdescription+xml") {
                    feed.search.get_or_insert(SearchLink::Description(url));
                } else if is_atom || href.contains("{searchTerms}") {
                    // Decoded href, template braces intact.
                    feed.search = Some(SearchLink::Template(
                        url.to_string().replace("%7B", "{").replace("%7D", "}"),
                    ));
                }
            } else if is_epub && is_acquisition {
                feed.epubs.push(EpubLink {
                    url,
                    title,
                    size: None,
                });
            }
        }
    }
}

/// Parse an OpenSearch description for its Atom results template.
pub fn parse_opensearch_template(xml: &[u8]) -> Option<String> {
    let mut reader = Reader::from_reader(xml);
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();
    let mut fallback: Option<String> = None;
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) if local(&e) == "Url" => {
                let kind = attr(&e, "type").unwrap_or_default();
                let Some(template) = attr(&e, "template") else {
                    continue;
                };
                if kind.starts_with("application/atom+xml") {
                    return Some(template);
                }
                // Some catalogues only declare an untyped template.
                if kind.is_empty() && fallback.is_none() {
                    fallback = Some(template);
                }
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
        buf.clear();
    }
    fallback
}

fn local(e: &BytesStart<'_>) -> String {
    String::from_utf8_lossy(e.local_name().as_ref()).into_owned()
}

fn attr(e: &BytesStart<'_>, name: &str) -> Option<String> {
    e.attributes().flatten().find_map(|a| {
        if a.key.local_name().as_ref() == name.as_bytes() {
            a.unescape_value().ok().map(|v| v.into_owned())
        } else {
            None
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fetch::FakeFetcher;

    const ROOT: &[u8] = include_bytes!("../tests/fixtures/opds/gutenberg-root.xml");
    const SEARCH: &[u8] = include_bytes!("../tests/fixtures/opds/gutenberg-search.xml");
    const POPULAR: &[u8] = include_bytes!("../tests/fixtures/opds/gutenberg-popular.xml");
    const BOOK84: &[u8] = include_bytes!("../tests/fixtures/opds/gutenberg-book84.xml");
    const OSD: &[u8] = include_bytes!("../tests/fixtures/opds/gutenberg-osd.xml");
    const CALIBRE_ROOT: &[u8] = include_bytes!("../tests/fixtures/opds/calibre-root.xml");
    const CALIBRE_NEWEST: &[u8] = include_bytes!("../tests/fixtures/opds/calibre-newest.xml");

    fn gutenberg() -> Url {
        Url::parse("https://www.gutenberg.org/ebooks.opds/").unwrap()
    }

    #[test]
    fn gutenberg_root_is_navigation_with_an_opensearch_description() {
        let feed = parse_feed(ROOT, &gutenberg()).unwrap();
        assert_eq!(feed.title, "Project Gutenberg");
        let titles: Vec<&str> = feed.entries.iter().map(|e| e.title.as_str()).collect();
        assert_eq!(titles, ["Popular", "Latest", "Random"]);
        // Relative hrefs are resolved against the feed's own URL.
        assert_eq!(
            feed.entries[0].feed.as_ref().unwrap().as_str(),
            "https://www.gutenberg.org/ebooks/search.opds/?sort_order=downloads"
        );
        assert!(feed.entries[0].epubs.is_empty());
        assert_eq!(
            feed.search,
            Some(SearchLink::Description(
                Url::parse("https://www.gutenberg.org/catalog/osd-books.xml").unwrap()
            ))
        );
        assert!(feed.next.is_none());
    }

    #[test]
    fn gutenberg_search_results_are_stubs_with_the_author_in_content() {
        let feed = parse_feed(SEARCH, &gutenberg()).unwrap();
        let frankenstein = feed
            .entries
            .iter()
            .find(|e| {
                e.feed
                    .as_ref()
                    .is_some_and(|u| u.path() == "/ebooks/84.opds")
            })
            .expect("Frankenstein stub");
        assert_eq!(
            frankenstein.title,
            "Frankenstein; or, The modern Prometheus"
        );
        assert_eq!(
            frankenstein.detail.as_deref(),
            Some("Mary Wollstonecraft Shelley")
        );
        assert_eq!(
            frankenstein.label(),
            "Frankenstein; or, The modern Prometheus — Mary Wollstonecraft Shelley"
        );
        // The "Authors" / "Subjects" facet rows are navigation, kept as rows.
        assert!(feed.entries.iter().any(|e| e.title == "Authors"));
    }

    #[test]
    fn gutenberg_popular_paginates_with_a_next_link() {
        let feed = parse_feed(POPULAR, &gutenberg()).unwrap();
        assert!(feed.entries.len() >= 20);
        assert_eq!(
            feed.next.as_ref().unwrap().as_str(),
            "https://www.gutenberg.org/ebooks/search.opds/?sort_order=downloads&start_index=26"
        );
    }

    #[test]
    fn gutenberg_book_feed_prefers_the_illustrated_epub() {
        let feed = parse_feed(BOOK84, &gutenberg()).unwrap();
        assert!(feed
            .title
            .starts_with("Frankenstein; or, The modern Prometheus"));
        let epubs: Vec<EpubLink> = feed.entries.iter().flat_map(|e| e.epubs.clone()).collect();
        assert_eq!(epubs.len(), 3, "noimages + epub3 + epub, no kindle files");
        let pick = pick_epub(&epubs).unwrap();
        assert!(!pick.url.path().contains("noimages"), "picked {}", pick.url);
        assert!(epubs
            .iter()
            .any(|e| e.url.path().ends_with("84.epub.noimages")));
    }

    #[test]
    fn opening_a_gutenberg_stub_resolves_to_its_book() {
        let search = parse_feed(SEARCH, &gutenberg()).unwrap();
        let stub = search
            .entries
            .iter()
            .find(|e| {
                e.feed
                    .as_ref()
                    .is_some_and(|u| u.path() == "/ebooks/84.opds")
            })
            .unwrap()
            .clone();
        let fetcher = FakeFetcher::new().with("https://www.gutenberg.org/ebooks/84.opds", BOOK84);
        match open_entry(&fetcher, &stub).unwrap() {
            Opened::Book(epub) => assert!(epub.url.path().ends_with(".images")),
            other => panic!("expected a book, got {other:?}"),
        }
    }

    #[test]
    fn opening_a_navigation_entry_lists_the_sub_catalogue() {
        let root = parse_feed(ROOT, &gutenberg()).unwrap();
        let popular = root.entries[0].clone();
        let fetcher = FakeFetcher::new().with(
            "https://www.gutenberg.org/ebooks/search.opds/?sort_order=downloads",
            POPULAR,
        );
        match open_entry(&fetcher, &popular).unwrap() {
            Opened::Feed(feed) => assert!(feed.entries.len() >= 20),
            other => panic!("expected a feed, got {other:?}"),
        }
    }

    #[test]
    fn opensearch_description_yields_the_atom_template_and_fills_it() {
        let template = parse_opensearch_template(OSD).unwrap();
        assert_eq!(
            template,
            "http://m.gutenberg.org/ebooks/search.opds/?query={searchTerms}"
        );
        let url = search_url(&template, "slaughterhouse five").unwrap();
        assert_eq!(
            url.as_str(),
            "http://m.gutenberg.org/ebooks/search.opds/?query=slaughterhouse+five"
        );
    }

    #[test]
    fn search_runs_through_the_description_document() {
        let root = parse_feed(ROOT, &gutenberg()).unwrap();
        let fetcher = FakeFetcher::new()
            .with("https://www.gutenberg.org/catalog/osd-books.xml", OSD)
            .with(
                "http://m.gutenberg.org/ebooks/search.opds/?query=frankenstein",
                SEARCH,
            );
        let results = search(&fetcher, root.search.as_ref().unwrap(), "frankenstein")
            .unwrap()
            .expect("searchable");
        assert!(results
            .entries
            .iter()
            .any(|e| e.title.starts_with("Frankenstein")));
    }

    #[test]
    fn optional_template_parameters_are_dropped() {
        let url = search_url(
            "https://x.test/s?q={searchTerms}&page={startPage?}&n={count?}",
            "a&b",
        )
        .unwrap();
        assert_eq!(url.as_str(), "https://x.test/s?q=a%26b&page=&n=");
    }

    #[test]
    fn calibre_root_has_a_template_search_and_navigation_rows() {
        let base = Url::parse("http://calibre.local:8080/opds").unwrap();
        let feed = parse_feed(CALIBRE_ROOT, &base).unwrap();
        assert_eq!(feed.title, "calibre library");
        assert_eq!(
            feed.search,
            Some(SearchLink::Template(
                "http://calibre.local:8080/opds/search/{searchTerms}".to_string()
            ))
        );
        let newest = feed.entries.iter().find(|e| e.title == "Newest").unwrap();
        assert_eq!(
            newest.feed.as_ref().unwrap().as_str(),
            "http://calibre.local:8080/opds/navcatalog/4f6e6577657374"
        );
    }

    #[test]
    fn calibre_acquisition_list_stays_a_list_and_its_books_download_directly() {
        let base = Url::parse("http://calibre.local:8080/opds/navcatalog/4f6e6577657374").unwrap();
        let newest = parse_feed(CALIBRE_NEWEST, &base).unwrap();
        assert_eq!(newest.entries.len(), 2);
        let book = &newest.entries[0];
        assert_eq!(book.title, "The Apothecary Diaries: Volume 1");
        assert_eq!(book.detail.as_deref(), Some("Natsu Hyuuga, Touko Shino"));
        assert_eq!(
            book.epubs[0].url.as_str(),
            "http://calibre.local:8080/get/epub/12/calibre"
        );
        // Opening the "Newest" entry from the root lists it, even though
        // every entry in it carries an EPUB: it is titled "Newest", not after
        // the entry that was tapped.
        let root = parse_feed(
            CALIBRE_ROOT,
            &Url::parse("http://calibre.local:8080/opds").unwrap(),
        )
        .unwrap();
        let newest_entry = root
            .entries
            .iter()
            .find(|e| e.title == "Newest")
            .unwrap()
            .clone();
        let fetcher = FakeFetcher::new().with(
            "http://calibre.local:8080/opds/navcatalog/4f6e6577657374",
            CALIBRE_NEWEST,
        );
        assert!(matches!(
            open_entry(&fetcher, &newest_entry).unwrap(),
            Opened::Feed(_)
        ));
        // And opening a book row downloads without another round-trip.
        let fetcher = FakeFetcher::new();
        assert!(matches!(
            open_entry(&fetcher, book).unwrap(),
            Opened::Book(_)
        ));
    }

    #[test]
    fn download_writes_author_title_epub_and_reports_an_existing_copy() {
        let dir = tempfile::tempdir().unwrap();
        let epub = Url::parse("http://calibre.local:8080/get/epub/12/calibre").unwrap();
        let fetcher = FakeFetcher::new().with(epub.as_str(), b"PK\x03\x04fake".to_vec());
        let (path, existed) = download_epub(
            &fetcher,
            &epub,
            dir.path(),
            Some("Natsu Hyuuga"),
            "The Apothecary Diaries: Volume 1",
        )
        .unwrap();
        assert!(!existed);
        assert_eq!(
            path,
            dir.path()
                .join("Natsu Hyuuga")
                .join("The Apothecary Diaries_ Volume 1.epub")
        );
        assert_eq!(std::fs::read(&path).unwrap(), b"PK\x03\x04fake");
        assert!(!path.with_extension("epub.part").exists());

        // Second time: nothing fetched, reported as already there.
        let fetcher = FakeFetcher::new();
        let (again, existed) = download_epub(
            &fetcher,
            &epub,
            dir.path(),
            Some("Natsu Hyuuga"),
            "The Apothecary Diaries: Volume 1",
        )
        .unwrap();
        assert!(existed);
        assert_eq!(again, path);
    }

    #[test]
    fn a_non_epub_response_is_refused_not_saved() {
        let dir = tempfile::tempdir().unwrap();
        let epub = Url::parse("https://www.gutenberg.org/ebooks/84.epub.images").unwrap();
        let fetcher =
            FakeFetcher::new().with(epub.as_str(), b"<html>Too many requests</html>".to_vec());
        let err = download_epub(&fetcher, &epub, dir.path(), None, "Frankenstein").unwrap_err();
        assert!(err.to_string().contains("not an EPUB"), "{err}");
        assert!(
            std::fs::read_dir(dir.path()).unwrap().next().is_none(),
            "nothing written"
        );
    }

    #[test]
    fn unknown_author_gets_a_folder_too() {
        let p = epub_path(Path::new("/b"), None, "Slaughterhouse-Five");
        assert_eq!(p, Path::new("/b/Unknown author/Slaughterhouse-Five.epub"));
    }

    #[test]
    fn garbage_is_a_parse_error_not_a_panic() {
        let err = parse_feed(b"<html><body>nope</body></html>", &gutenberg()).unwrap_err();
        assert!(matches!(err, Error::ParseFeed { .. }));
        let err = parse_feed(b"\xff\xfe not xml", &gutenberg());
        assert!(err.is_err());
    }
}
