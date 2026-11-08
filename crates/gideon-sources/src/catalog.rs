//! One front door for the Books screens: every book catalogue request comes
//! through here, and goes to the Internet Archive adapter
//! ([`crate::archive`]) for archive.org URLs and to the OPDS parser
//! ([`crate::opds`]) for everything else. Downloads need no dispatch —
//! an EPUB is a file at a URL either way ([`crate::opds::download_epub`]).

use url::Url;

use crate::fetch::Fetcher;
use crate::opds::{Entry, Feed, Opened, SearchLink};
use crate::{archive, opds, Result};

/// Fetch the feed at `url`.
pub fn fetch_feed(fetcher: &dyn Fetcher, url: &Url) -> Result<Feed> {
    if archive::is_archive(url) {
        archive::fetch_feed(fetcher, url)
    } else {
        opds::fetch_feed(fetcher, url)
    }
}

/// The page after `feed`, if it has one.
pub fn fetch_next(fetcher: &dyn Fetcher, feed: &Feed) -> Result<Option<Feed>> {
    match &feed.next {
        Some(next) => fetch_feed(fetcher, next).map(Some),
        None => Ok(None),
    }
}

/// Search a catalogue. `Ok(None)` when it cannot be searched.
pub fn search(fetcher: &dyn Fetcher, link: &SearchLink, query: &str) -> Result<Option<Feed>> {
    if archive::is_archive_search(link) {
        archive::search(fetcher, query).map(Some)
    } else {
        opds::search(fetcher, link, query)
    }
}

/// Open an entry: the book behind it, or the sub-catalogue it lists.
pub fn open_entry(fetcher: &dyn Fetcher, entry: &Entry) -> Result<Opened> {
    match &entry.feed {
        Some(url) if entry.epubs.is_empty() && archive::is_archive(url) => {
            archive::open_entry(fetcher, entry)
        }
        _ => opds::open_entry(fetcher, entry),
    }
}
