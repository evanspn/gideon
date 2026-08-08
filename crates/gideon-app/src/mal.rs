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

/// Jikan's manga-search endpoint, used to look up a title's known name
/// variants (English, romanised, Japanese, synonyms).
const JIKAN_SEARCH_MANGA: &str = "https://api.jikan.moe/v4/manga";

/// How many name variants a lookup returns at most. Each variant a source
/// gets retried with is another network round-trip on the device, so the
/// list stays short.
const MAX_TITLE_VARIANTS: usize = 6;

/// Look up alternative titles for `query` on MyAnimeList: a manga is often
/// listed under its romanised Japanese title on one source and its English
/// title on another (e.g. "Judge" vs "Jajji"), so a search that misses with
/// the user's spelling can be retried with the names the catalogue knows.
///
/// Returns the variants (deduplicated, without the query itself), best
/// matches first. An empty list just means "nothing to retry with".
pub fn search_title_variants(fetcher: &dyn Fetcher, query: &str) -> Result<Vec<String>> {
    let mut url = Url::parse(JIKAN_SEARCH_MANGA).expect("valid static URL");
    url.query_pairs_mut()
        .append_pair("q", query)
        .append_pair("limit", "5");
    let body = fetcher
        .get(&url)
        .context("fetching MyAnimeList title variants")?;
    parse_title_variants(&body, query)
}

/// Parse a Jikan `/manga?q=` response into alternative titles for `query`.
///
/// Jikan's search is fuzzy, so only entries where one of the titles actually
/// contains the query (case-insensitively) contribute — that keeps a search
/// for "judge" from dragging in every courtroom manga's synonyms.
pub fn parse_title_variants(body: &[u8], query: &str) -> Result<Vec<String>> {
    #[derive(Deserialize)]
    struct Response {
        data: Vec<Entry>,
    }
    #[derive(Deserialize, Default)]
    struct Entry {
        title: Option<String>,
        title_english: Option<String>,
        title_japanese: Option<String>,
        #[serde(default)]
        title_synonyms: Vec<String>,
    }

    let response: Response =
        serde_json::from_slice(body).context("parsing MyAnimeList search response")?;
    let needle = query.trim().to_lowercase();

    let mut variants: Vec<String> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    seen.insert(needle.clone());
    for entry in response.data {
        let titles: Vec<String> = entry
            .title
            .into_iter()
            .chain(entry.title_english)
            .chain(entry.title_japanese)
            .chain(entry.title_synonyms)
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty())
            .collect();
        // Only take variants from an entry the query plausibly meant.
        if !titles.iter().any(|t| t.to_lowercase().contains(&needle)) {
            continue;
        }
        for title in titles {
            if seen.insert(title.to_lowercase()) {
                variants.push(title);
            }
        }
    }
    variants.truncate(MAX_TITLE_VARIANTS);
    Ok(variants)
}

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

    const SEARCH_SAMPLE: &str = r#"{
        "data": [
            {
                "title": "Judge",
                "title_english": "Judge",
                "title_japanese": "ジャッジ",
                "title_synonyms": []
            },
            {
                "title": "Shingeki no Kyojin",
                "title_english": "Attack on Titan",
                "title_japanese": "進撃の巨人",
                "title_synonyms": ["AoT"]
            }
        ]
    }"#;

    #[test]
    fn variants_come_from_matching_entries_only() {
        // "judge" matches the first entry; the unrelated fuzzy hit is
        // ignored, and the query itself is not echoed back as a variant.
        let out = parse_title_variants(SEARCH_SAMPLE.as_bytes(), "judge").unwrap();
        assert_eq!(out, vec!["ジャッジ".to_string()]);
    }

    #[test]
    fn variants_include_synonyms_and_both_scripts() {
        let out = parse_title_variants(SEARCH_SAMPLE.as_bytes(), "Attack on Titan").unwrap();
        assert_eq!(
            out,
            vec![
                "Shingeki no Kyojin".to_string(),
                "進撃の巨人".to_string(),
                "AoT".to_string(),
            ]
        );
    }

    #[test]
    fn no_matching_entry_means_no_variants() {
        let out = parse_title_variants(SEARCH_SAMPLE.as_bytes(), "one piece").unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn variant_fetch_uses_the_jikan_search_endpoint() {
        let fetcher = FakeFetcher::new().with(
            "https://api.jikan.moe/v4/manga?q=judge&limit=5",
            SEARCH_SAMPLE,
        );
        let out = search_title_variants(&fetcher, "judge").unwrap();
        assert_eq!(out, vec!["ジャッジ".to_string()]);
    }
}
