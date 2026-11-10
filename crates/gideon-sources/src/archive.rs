//! The Internet Archive as a book catalogue, beside the OPDS ones.
//!
//! archive.org has no working OPDS feed (its BookServer resets the
//! connection), so the [`archivist`] crate talks to its JSON search and
//! metadata endpoints instead. This module turns those answers into the
//! same [`Feed`] / [`Entry`] / [`Opened`] the OPDS screens already show, so
//! the Books screens need no idea which kind of catalogue they are in:
//!
//! * the root, `https://archive.org/`, is search and nothing else: a feed
//!   with no rows and a search link — no network needed to show it;
//! * an `advancedsearch.php` URL (a search) is a page of books,
//!   one entry per item, paged with `next` like an OPDS feed;
//! * opening a book fetches `/metadata/<identifier>` and hands back its
//!   best EPUB, which then downloads through [`crate::opds::download_epub`]
//!   (whose "starts with PK" check refuses archive.org's throttling page).
//!
//! [`crate::catalog`] decides per URL whether this module or the OPDS
//! parser handles a request.

use url::Url;

use crate::fetch::Fetcher;
use crate::opds::{Entry, EpubLink, Feed, Opened, SearchLink};
use crate::{Error, Result};

/// The catalogue's root, as stored in a `BookCatalog`.
pub const ROOT: &str = "https://archive.org/";

/// Books per page — the same as archivist's default and a Gutenberg page.
const ROWS: u32 = 25;

/// Whether `url` belongs to the Internet Archive.
pub fn is_archive(url: &Url) -> bool {
    matches!(url.host_str(), Some("archive.org" | "www.archive.org"))
}

/// The search link the root feed carries. It is never filled in as a
/// template — [`crate::catalog::search`] recognises it and builds the real
/// query through archivist — but it is a valid OPDS-style template so
/// nothing that only knows OPDS chokes on it.
pub fn search_link() -> SearchLink {
    SearchLink::Template(format!("{ROOT}advancedsearch.php?q={{searchTerms}}"))
}

/// Whether `link` is this catalogue's search.
pub fn is_archive_search(link: &SearchLink) -> bool {
    match link {
        SearchLink::Template(t) => Url::parse(&t.replace("{searchTerms}", "x"))
            .map(|u| is_archive(&u))
            .unwrap_or(false),
        SearchLink::Description(u) => is_archive(u),
    }
}

/// Fetch the feed at an archive.org `url`: the root, or a page of books.
pub fn fetch_feed(fetcher: &dyn Fetcher, url: &Url) -> Result<Feed> {
    if url.path() != "/advancedsearch.php" {
        return Ok(root());
    }
    let body = fetcher.get(url)?;
    let mut page = archivist::parse_search(&body).map_err(|message| Error::ParseFeed {
        url: url.to_string(),
        message,
    })?;
    page.page = query_u32(url, "page").unwrap_or(1).max(1);
    page.rows = query_u32(url, "rows").unwrap_or(ROWS).max(1);
    let client = client(fetcher);
    let next = if page.has_more() {
        Some(with_page(url, page.page + 1))
    } else {
        None
    };
    let entries = page
        .hits
        .iter()
        // Every query asks for EPUBs, but archive.org's format filter is per
        // item and counts derivatives, so check the hit itself.
        .filter(|h| h.has_epub())
        .filter_map(|h| {
            Some(Entry {
                title: h.title.clone(),
                detail: h.creator.clone(),
                epubs: Vec::new(),
                feed: Some(client.item_url(&h.identifier).ok()?),
                year: h.year.clone(),
                language: h.language.as_deref().map(language_name),
            })
        })
        .collect();
    Ok(Feed {
        title: "Results".to_string(),
        entries,
        next,
        search: Some(search_link()),
        epubs: Vec::new(),
        total: Some(page.total),
    })
}

/// Where a search for `query` goes: titles and authors, most downloaded
/// first, books with an EPUB that need no borrowing.
pub fn search_url(query: &str) -> Result<Url> {
    let opts = archivist::SearchOptions {
        rows: ROWS,
        ..Default::default()
    };
    archivist::Client::with_fetcher(archivist::FakeFetcher::new())
        .search_url(query, &opts)
        .map_err(from_archivist)
}

/// Search every free catalogue for `query` and hand back one list.
///
/// A reader should not have to know which catalogue has a book. archivist
/// asks Project Gutenberg, Open Library and the Internet Archive, merges
/// the answers (one row per book, the best copy first) and this turns them
/// into entries: a Gutenberg book carries its EPUB link outright; an
/// Archive or Open Library one points at its `/metadata/` page, with the
/// other candidate scans kept as alternates for [`open_entry`] to try.
///
/// One catalogue being down is not an error — the others still answer.
/// Nobody answering is: the device is offline, and the screen says so.
pub fn search(fetcher: &dyn Fetcher, query: &str) -> Result<Feed> {
    let client = client(fetcher);
    let mut report = client.find(query, ROWS as usize);
    if report.all_failed() {
        return Err(from_archivist(report.failed.remove(0).1));
    }
    let entries: Vec<Entry> = report
        .results
        .into_iter()
        .filter_map(|f| {
            let (epubs, feed) = match f.epub {
                Some(url) => (
                    vec![EpubLink {
                        url,
                        title: None,
                        size: None,
                    }],
                    None,
                ),
                None => {
                    let (first, rest) = f.archive_ids.split_first()?;
                    let mut url = client.item_url(first).ok()?;
                    if !rest.is_empty() {
                        url.query_pairs_mut().append_pair("alt", &rest.join(","));
                    }
                    (Vec::new(), Some(url))
                }
            };
            Some(Entry {
                title: f.title,
                detail: f.author,
                epubs,
                feed,
                year: f.year,
                language: f.language.as_deref().map(language_name),
            })
        })
        .collect();
    Ok(Feed {
        title: "Results".to_string(),
        total: Some(entries.len() as u64),
        entries,
        next: None,
        search: Some(search_link()),
        epubs: Vec::new(),
    })
}

/// Open an entry from one of this catalogue's feeds: a book (its
/// `/metadata/` page) gives back its best EPUB; anything else is a feed.
pub fn open_entry(fetcher: &dyn Fetcher, entry: &Entry) -> Result<Opened> {
    let Some(url) = &entry.feed else {
        return Err(Error::Fetch {
            url: String::new(),
            message: format!("\"{}\" has nothing to open", entry.title),
        });
    };
    let Some(identifier) = metadata_identifier(url) else {
        return fetch_feed(fetcher, url).map(Opened::Feed);
    };
    let client = client(fetcher);
    // The item named, then any alternates a unified search attached (other
    // scans of the same book): the first with an EPUB wins.
    let alternates: Vec<String> = url
        .query_pairs()
        .find(|(k, _)| k == "alt")
        .map(|(_, v)| v.split(',').map(str::to_string).collect())
        .unwrap_or_default();
    let mut found = None;
    let mut last_err = None;
    for id in std::iter::once(identifier).chain(alternates) {
        match client.item(&id) {
            Ok(item) => match item.best_epub().cloned() {
                Some(file) => {
                    found = Some((item, file));
                    break;
                }
                None => {
                    last_err = Some(Error::Fetch {
                        url: url.to_string(),
                        message: format!("\"{}\" has no EPUB to download", item.title),
                    })
                }
            },
            Err(e) => last_err = Some(from_archivist(e)),
        }
    }
    let Some((item, file)) = found else {
        return Err(last_err.expect("at least one item was tried"));
    };
    Ok(Opened::Book(EpubLink {
        url: client
            .download_url(&item.identifier, &file.name)
            .map_err(from_archivist)?,
        title: None,
        size: Some(file.size).filter(|s| *s > 0),
    }))
}

/// The root feed: search, and nothing to browse.
fn root() -> Feed {
    Feed {
        title: "Search all free books".to_string(),
        entries: Vec::new(),
        next: None,
        search: Some(search_link()),
        epubs: Vec::new(),
        total: None,
    }
}

/// archive.org's language field is a code ("eng") as often as a name
/// ("English"); the sheet shows a name.
fn language_name(raw: &str) -> String {
    match raw.trim().to_ascii_lowercase().as_str() {
        "eng" | "en" | "english" => "English",
        "fre" | "fra" | "fr" | "french" => "French",
        "ger" | "deu" | "de" | "german" => "German",
        "spa" | "es" | "spanish" => "Spanish",
        "ita" | "it" | "italian" => "Italian",
        "por" | "pt" | "portuguese" => "Portuguese",
        "lat" | "la" | "latin" => "Latin",
        "dut" | "nld" | "nl" | "dutch" => "Dutch",
        "rus" | "ru" | "russian" => "Russian",
        "jpn" | "ja" | "japanese" => "Japanese",
        "chi" | "zho" | "zh" | "chinese" => "Chinese",
        _ => return raw.trim().to_string(),
    }
    .to_string()
}

/// `https://archive.org/metadata/<identifier>` → the identifier.
fn metadata_identifier(url: &Url) -> Option<String> {
    let mut segments = url.path_segments()?;
    if segments.next()? != "metadata" {
        return None;
    }
    let id = segments.next().filter(|s| !s.is_empty())?;
    Some(percent_decode(id).unwrap_or_else(|| id.to_string()))
}

fn percent_decode(s: &str) -> Option<String> {
    let bytes: Vec<u8> = url::form_urlencoded::parse(format!("x={s}").as_bytes())
        .next()
        .map(|(_, v)| v.into_owned().into_bytes())?;
    String::from_utf8(bytes).ok()
}

fn query_u32(url: &Url, key: &str) -> Option<u32> {
    url.query_pairs()
        .find(|(k, _)| k == key)
        .and_then(|(_, v)| v.parse().ok())
}

/// `url` with its `page` parameter set to `page`, everything else kept in order.
fn with_page(url: &Url, page: u32) -> Url {
    let pairs: Vec<(String, String)> = url
        .query_pairs()
        .map(|(k, v)| {
            let v = if k == "page" {
                page.to_string()
            } else {
                v.into_owned()
            };
            (k.into_owned(), v)
        })
        .collect();
    let mut out = url.clone();
    out.query_pairs_mut().clear().extend_pairs(pairs);
    out
}

/// archivist on top of gideon's fetcher, so archive.org requests get the
/// same retries, roots and "Wi-Fi is off" message as everything else.
struct Bridge<'a>(&'a dyn Fetcher);

impl archivist::Fetcher for Bridge<'_> {
    fn get(&self, url: &Url) -> archivist::Result<Vec<u8>> {
        self.0.get(url).map_err(|e| match e {
            Error::Offline => archivist::Error::Offline,
            Error::Fetch { url, message } => archivist::Error::Http { url, message },
            e => archivist::Error::Http {
                url: url.to_string(),
                message: e.to_string(),
            },
        })
    }
}

fn client(fetcher: &dyn Fetcher) -> archivist::Client<Bridge<'_>> {
    archivist::Client::with_fetcher(Bridge(fetcher))
}

fn from_archivist(e: archivist::Error) -> Error {
    match e {
        archivist::Error::Offline => Error::Offline,
        archivist::Error::Http { url, message } => Error::Fetch { url, message },
        archivist::Error::Parse { url, message } => Error::ParseFeed { url, message },
        archivist::Error::Io(e) => Error::Io(e),
        archivist::Error::Url(e) => Error::InvalidUrl(e),
        e => Error::Fetch {
            url: String::new(),
            message: e.to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fetch::FakeFetcher;

    /// Two pages of "frankenstein" hits: 3 found, 2 rows a page. The third
    /// hit on page 1 has no EPUB (archive.org's format filter counts
    /// derivatives) and must not be offered.
    const PAGE1: &str = r#"{"response":{"numFound":3,"start":0,"docs":[
        {"identifier":"frankenstein00shel","title":"Frankenstein","creator":"Shelley, Mary Wollstonecraft","year":1818,"downloads":900,"format":["EPUB","Text PDF"]},
        {"identifier":"frankenstein-pdf-only","title":"Frankenstein (scan)","creator":["Shelley, Mary","Unknown"],"downloads":10,"format":["Text PDF"]}
    ]}}"#;
    const PAGE2: &str = r#"{"response":{"numFound":3,"start":2,"docs":[
        {"identifier":"lastman00shel","title":"The Last Man","creator":"Shelley, Mary Wollstonecraft","downloads":50,"format":["EPUB"]}
    ]}}"#;
    const ITEM: &str = r#"{"metadata":{"identifier":"frankenstein00shel","title":"Frankenstein","creator":"Shelley, Mary Wollstonecraft"},
        "files":[
          {"name":"frankenstein00shel.pdf","format":"Text PDF","size":"9000"},
          {"name":"frankenstein00shel_small.epub","format":"EPUB","size":"100"},
          {"name":"Frankenstein illustrated.epub","format":"EPUB","size":"5000"}
        ]}"#;

    fn paged_url(page: u32) -> Url {
        let opts = archivist::SearchOptions {
            rows: 2,
            page,
            ..Default::default()
        };
        archivist::Client::with_fetcher(archivist::FakeFetcher::new())
            .search_url("frankenstein", &opts)
            .unwrap()
    }

    fn root_url() -> Url {
        Url::parse(ROOT).unwrap()
    }

    #[test]
    fn the_root_is_search_only_and_needs_no_network() {
        let feed = crate::catalog::fetch_feed(&FakeFetcher::new(), &root_url()).unwrap();
        assert_eq!(feed.title, "Search all free books");
        assert!(feed.entries.is_empty(), "nothing to browse, only search");
        assert!(is_archive_search(feed.search.as_ref().unwrap()));
    }

    #[test]
    fn a_results_page_lists_books_with_epubs_and_pages_on() {
        let fetcher = FakeFetcher::new()
            .with(paged_url(1).as_str(), PAGE1)
            .with(paged_url(2).as_str(), PAGE2);
        let feed = crate::catalog::fetch_feed(&fetcher, &paged_url(1)).unwrap();
        let labels: Vec<String> = feed.entries.iter().map(|e| e.label()).collect();
        assert_eq!(labels, ["Frankenstein — Shelley, Mary Wollstonecraft"]);
        assert_eq!(
            feed.entries[0].feed.as_ref().unwrap().as_str(),
            "https://archive.org/metadata/frankenstein00shel"
        );
        assert!(feed.entries[0].epubs.is_empty());
        assert_eq!(feed.next.as_ref(), Some(&paged_url(2)));

        let page2 = crate::catalog::fetch_next(&fetcher, &feed)
            .unwrap()
            .unwrap();
        assert_eq!(page2.entries[0].title, "The Last Man");
        assert_eq!(page2.next, None, "3 hits fit on two pages of 2");
    }

    #[test]
    fn search_goes_through_archivist_not_the_opds_template() {
        let url = super::search_url("frankenstein").unwrap();
        let fetcher = FakeFetcher::new().with(url.as_str(), PAGE1);
        let feed = crate::catalog::search(&fetcher, &search_link(), "frankenstein")
            .unwrap()
            .unwrap();
        assert_eq!(feed.title, "Results");
        assert_eq!(feed.entries[0].title, "Frankenstein");
    }

    #[test]
    fn opening_a_book_picks_its_largest_epub() {
        let fetcher =
            FakeFetcher::new().with("https://archive.org/metadata/frankenstein00shel", ITEM);
        let entry = Entry {
            title: "Frankenstein".into(),
            detail: Some("Shelley, Mary Wollstonecraft".into()),
            epubs: Vec::new(),
            feed: Some(Url::parse("https://archive.org/metadata/frankenstein00shel").unwrap()),
            ..Default::default()
        };
        match crate::catalog::open_entry(&fetcher, &entry).unwrap() {
            Opened::Book(link) => assert_eq!(
                link.url.as_str(),
                "https://archive.org/download/frankenstein00shel/Frankenstein%20illustrated.epub"
            ),
            other => panic!("expected a book, got {other:?}"),
        }
    }

    #[test]
    fn a_book_without_an_epub_says_so() {
        let fetcher = FakeFetcher::new().with(
            "https://archive.org/metadata/frankenstein-pdf-only",
            r#"{"metadata":{"title":"Frankenstein (scan)"},"files":[{"name":"a.pdf","format":"Text PDF","size":"1"}]}"#,
        );
        let entry = Entry {
            title: "Frankenstein (scan)".into(),
            feed: Some(Url::parse("https://archive.org/metadata/frankenstein-pdf-only").unwrap()),
            ..Default::default()
        };
        let err = crate::catalog::open_entry(&fetcher, &entry).unwrap_err();
        assert!(err.to_string().contains("no EPUB"), "{err}");
    }

    #[test]
    fn offline_stays_offline_through_archivist() {
        struct Down;
        impl Fetcher for Down {
            fn get(&self, _: &Url) -> Result<Vec<u8>> {
                Err(Error::Offline)
            }
        }
        let entry = Entry {
            title: "x".into(),
            feed: Some(Url::parse("https://archive.org/metadata/x").unwrap()),
            ..Default::default()
        };
        assert!(matches!(
            crate::catalog::open_entry(&Down, &entry),
            Err(Error::Offline)
        ));
    }

    #[test]
    fn opds_catalogues_are_left_alone() {
        assert!(!is_archive(
            &Url::parse("https://www.gutenberg.org/ebooks.opds/").unwrap()
        ));
        assert!(!is_archive_search(&SearchLink::Template(
            "https://www.gutenberg.org/ebooks/search.opds/?query={searchTerms}".into()
        )));
        assert!(is_archive(&Url::parse("https://www.archive.org/").unwrap()));
    }

    #[test]
    fn one_search_merges_gutenberg_open_library_and_the_archive() {
        let probe = archivist::Client::with_fetcher(archivist::FakeFetcher::new());
        let q = "the time machine";
        let gutendex = r#"{"results":[{"title":"The Time Machine","authors":[{"name":"Wells, H. G."}],"languages":["en"],"download_count":9,
            "formats":{"application/epub+zip":"https://www.gutenberg.org/ebooks/35.epub3.images"}}]}"#;
        let open_library = r#"{"docs":[
            {"title":"The Time Machine","author_name":["H. G. Wells"],"first_publish_year":1895,"ebook_access":"public","ia":["tm_a","tm_b"]},
            {"title":"The Time Machine and Other Stories","author_name":["H. G. Wells"],"ebook_access":"public","ia":["tm_other","tm_other2"]}]}"#;
        let archive = r#"{"response":{"numFound":1,"start":0,"docs":[
            {"identifier":"tm_scan","title":"The time machine","creator":"Wells, H. G.","downloads":7,"format":["EPUB"]}]}}"#;
        let item = |id: &str, epub: bool| {
            let files = if epub {
                format!(r#"[{{"name":"{id}.epub","format":"EPUB","size":"1000"}}]"#)
            } else {
                r#"[{"name":"x.pdf","format":"Text PDF","size":"5"}]"#.to_string()
            };
            format!(
                r#"{{"metadata":{{"identifier":"{id}","title":"The Time Machine and Other Stories"}},"files":{files}}}"#
            )
        };
        let fetcher = FakeFetcher::new()
            .with(probe.gutendex_url(q).unwrap().as_str(), gutendex)
            .with(probe.open_library_url(q).unwrap().as_str(), open_library)
            .with(search_url(q).unwrap().as_str(), archive)
            .with(
                probe.item_url("tm_other").unwrap().as_str(),
                item("tm_other", false),
            )
            .with(
                probe.item_url("tm_other2").unwrap().as_str(),
                item("tm_other2", true),
            );
        let feed = search(&fetcher, q).unwrap();
        let titles: Vec<&str> = feed.entries.iter().map(|e| e.title.as_str()).collect();
        // One row for the book itself (three catalogues had it), then the
        // collection that only Open Library offered.
        assert_eq!(
            titles,
            ["The Time Machine", "The Time Machine and Other Stories"]
        );
        assert_eq!(feed.total, Some(2));
        assert_eq!(feed.next, None);
        // Gutenberg's copy carries its EPUB outright and opens without a fetch.
        let top = &feed.entries[0];
        assert_eq!(top.detail.as_deref(), Some("H. G. Wells"));
        assert_eq!(top.year.as_deref(), Some("1895"));
        assert_eq!(top.language.as_deref(), Some("English"));
        let Opened::Book(link) = crate::catalog::open_entry(&fetcher, top).unwrap() else {
            panic!("expected a book");
        };
        assert_eq!(
            link.url.as_str(),
            "https://www.gutenberg.org/ebooks/35.epub3.images"
        );
        // The Open Library one resolves through its scans: the first has
        // only a PDF, the alternate has the EPUB.
        let Opened::Book(link) = crate::catalog::open_entry(&fetcher, &feed.entries[1]).unwrap()
        else {
            panic!("expected a book");
        };
        assert!(
            link.url
                .as_str()
                .ends_with("/download/tm_other2/tm_other2.epub"),
            "{}",
            link.url
        );

        // One catalogue alone still answers; none at all is the error.
        let only_archive = FakeFetcher::new().with(search_url(q).unwrap().as_str(), archive);
        assert_eq!(search(&only_archive, q).unwrap().entries.len(), 1);
        assert!(search(&FakeFetcher::new(), q).is_err());
    }
}
