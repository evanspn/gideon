//! MyAnimeList "Top manga" via the public [Jikan](https://jikan.moe) API
//! (no auth, no API key).
//!
//! Powers the Home "Popular manga" tab: a ranked list of popular manga pulled
//! straight from MyAnimeList. A title tapped there feeds gideon's existing
//! global search, so the user finds and downloads it from their installed
//! sources — MyAnimeList only supplies the *catalogue*, never the pages.
//!
//! The JSON parsing is split from the HTTP fetch so it's unit-testable with a
//! `FakeFetcher` and canned bodies — no network in tests.

use anyhow::{Context, Result};
use serde::Deserialize;
use url::Url;

use gideon_sources::Fetcher;

/// One popular manga title from MyAnimeList.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PopularManga {
    /// Display title (the English title when MyAnimeList has one, else the
    /// romanised default).
    pub title: String,
    /// Cover image URL, when MyAnimeList has one.
    pub cover_url: Option<String>,
}

/// Jikan's top-manga endpoint. `type=manga` keeps it to manga proper (no
/// light novels or one-shots); `filter=bypopularity` ranks by member count
/// rather than score, which is what "popular" means here.
const JIKAN_TOP_MANGA: &str = "https://api.jikan.moe/v4/top/manga?type=manga&filter=bypopularity";

/// Fetch the popular-manga ranking from MyAnimeList (one page, ~25 titles, in
/// rank order).
pub fn fetch_popular(fetcher: &dyn Fetcher) -> Result<Vec<PopularManga>> {
    let url = Url::parse(JIKAN_TOP_MANGA).expect("valid static URL");
    let body = fetcher
        .get(&url)
        .context("fetching MyAnimeList popular manga")?;
    parse_popular(&body)
}

/// Parse a Jikan `/top/manga` response body into popular titles, in rank
/// order. Entries missing a usable title are skipped rather than failing the
/// whole list, so one odd record can't blank the tab.
pub fn parse_popular(body: &[u8]) -> Result<Vec<PopularManga>> {
    #[derive(Deserialize)]
    struct Response {
        data: Vec<Entry>,
    }
    #[derive(Deserialize)]
    struct Entry {
        title: Option<String>,
        title_english: Option<String>,
        images: Option<Images>,
    }
    #[derive(Deserialize)]
    struct Images {
        jpg: Option<Image>,
    }
    #[derive(Deserialize)]
    struct Image {
        image_url: Option<String>,
    }

    let response: Response =
        serde_json::from_slice(body).context("parsing MyAnimeList top-manga response")?;
    Ok(response
        .data
        .into_iter()
        .filter_map(|e| {
            // Prefer the English title (what the user is likely to search for
            // on an English source); fall back to the default title.
            let title = e
                .title_english
                .filter(|t| !t.trim().is_empty())
                .or(e.title)
                .map(|t| t.trim().to_string())
                .filter(|t| !t.is_empty())?;
            Some(PopularManga {
                title,
                cover_url: e.images.and_then(|i| i.jpg).and_then(|j| j.image_url),
            })
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use gideon_sources::fetch::FakeFetcher;

    const SAMPLE: &str = r#"{
        "data": [
            {
                "title": "Berserk",
                "title_english": "Berserk",
                "images": { "jpg": { "image_url": "https://cdn.myanimelist.net/berserk.jpg" } }
            },
            {
                "title": "Vagabond",
                "title_english": null,
                "images": { "jpg": { "image_url": null } }
            },
            {
                "title": "Shingeki no Kyojin",
                "title_english": "Attack on Titan",
                "images": null
            },
            {
                "title": "   ",
                "title_english": null
            }
        ]
    }"#;

    #[test]
    fn parses_titles_in_order_preferring_english() {
        let out = parse_popular(SAMPLE.as_bytes()).unwrap();
        // The blank-title record is dropped; the rest stay in rank order.
        assert_eq!(out.len(), 3);
        assert_eq!(out[0].title, "Berserk");
        assert_eq!(
            out[0].cover_url.as_deref(),
            Some("https://cdn.myanimelist.net/berserk.jpg")
        );
        // No English title → falls back to the default title.
        assert_eq!(out[1].title, "Vagabond");
        assert_eq!(out[1].cover_url, None);
        // English title wins over the romanised default.
        assert_eq!(out[2].title, "Attack on Titan");
    }

    #[test]
    fn fetch_uses_the_jikan_endpoint() {
        let fetcher = FakeFetcher::new().with(JIKAN_TOP_MANGA, SAMPLE);
        let out = fetch_popular(&fetcher).unwrap();
        assert_eq!(out[0].title, "Berserk");
    }

    #[test]
    fn malformed_json_is_an_error_not_a_panic() {
        assert!(parse_popular(b"not json").is_err());
    }
}
