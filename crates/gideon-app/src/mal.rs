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

use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use url::Url;

use gideon_core::SeriesMeta;
use gideon_sources::Fetcher;

/// One popular manga title from MyAnimeList. Serialisable so the last
/// successfully fetched list can be cached on disk and served through a
/// MyAnimeList outage.
///
/// Everything past the title is optional: MyAnimeList leaves most of these
/// null for a good part of its catalogue (an ongoing series has no chapter
/// count, an unranked one no rank), and a cache written by an older gideon
/// has none of them at all — hence `#[serde(default)]` on the metadata, so a
/// stale cache still deserialises instead of blanking the tab.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PopularManga {
    /// Display title (the English title when MyAnimeList has one, else the
    /// romanised default).
    pub title: String,
    /// Cover image URL, when MyAnimeList has one.
    pub cover_url: Option<String>,
    /// Mean member score, 1.0–10.0 (`null` until a series has enough votes).
    #[serde(default)]
    pub score: Option<f32>,
    /// Publication status, verbatim from MyAnimeList ("Publishing",
    /// "Finished", "On Hiatus", …).
    #[serde(default)]
    pub status: Option<String>,
    /// Genre names ("Action", "Drama", …); empty when MyAnimeList lists none.
    #[serde(default)]
    pub genres: Vec<String>,
    /// Position in MyAnimeList's overall ranking (`null` for unranked and
    /// still-airing entries).
    #[serde(default)]
    pub rank: Option<u32>,
    /// Total chapter count (`null` while a series is still running).
    #[serde(default)]
    pub chapters: Option<u32>,
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
    let body = search_manga(fetcher, query)?;
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

/// Longest query Jikan is asked for. A library folder name can be a whole
/// sentence ("… Vol. 3 Official Colored"); MyAnimeList rejects very long
/// queries outright, so the query is clipped rather than wasted (the web
/// dashboard's MAL sync clips at the same 64).
const MAX_QUERY_CHARS: usize = 64;

/// One Jikan `/manga?q=` search. The single place the search endpoint is
/// spelled out, so the title-variant lookup and the metadata lookup can't
/// drift apart (and a metadata lookup can reuse a body it already fetched
/// instead of paying for a second round-trip on the device).
fn search_manga(fetcher: &dyn Fetcher, query: &str) -> Result<Vec<u8>> {
    let clipped: String = query.trim().chars().take(MAX_QUERY_CHARS).collect();
    let mut url = Url::parse(JIKAN_SEARCH_MANGA).expect("valid static URL");
    url.query_pairs_mut()
        .append_pair("q", &clipped)
        .append_pair("limit", "5");
    fetcher.get(&url).context("searching MyAnimeList")
}

/// Look up MyAnimeList metadata (score, status, genres, rank, chapter count)
/// for a single series by title.
///
/// `Ok(None)` is the normal "no confident match" answer and is not a problem
/// worth reporting: a *wrong* match is far worse than no metadata (it would
/// put another series' score and genres on the user's book), so a hit only
/// counts when a MyAnimeList title matches exactly once normalised — the same
/// rule `syncKoboToMal` uses in the web dashboard before it writes to a real
/// MAL list.
///
/// Romanised-vs-English naming is the one case where an exact rule needs
/// help, so a miss retries with the names MyAnimeList itself knows the series
/// by ([`parse_title_variants`], capped at [`MAX_TITLE_VARIANTS`]) — the same
/// retry the source search uses. That costs extra round-trips, so callers
/// must only ever run this once per series, never per chapter.
pub fn metadata_for_title(fetcher: &dyn Fetcher, title: &str) -> Result<Option<SeriesMeta>> {
    let query = series_query(title);
    let body = search_manga(fetcher, &query)?;
    if let Some(meta) = parse_metadata_match(&body, &query)? {
        return Ok(Some(meta));
    }
    // The first page of results was fetched with the user's spelling; the
    // variants are what that page says the series is *also* called, so a
    // retry can find an entry the first query ranked off the page.
    for variant in parse_title_variants(&body, &query)? {
        let body = search_manga(fetcher, &variant)?;
        if let Some(meta) = parse_metadata_match(&body, &variant)? {
            return Ok(Some(meta));
        }
    }
    Ok(None)
}

/// The query a library title is searched with: mirrors the web dashboard's
/// `seriesQuery`. Folder names pick up qualifiers MyAnimeList doesn't carry
/// ("Berserk (Deluxe Edition)", "Manga - Berserk"); dropping them is what
/// lets an *exact* match rule still hit.
fn series_query(title: &str) -> String {
    let mut out = String::with_capacity(title.len());
    let mut depth = 0usize;
    for c in title.chars() {
        match c {
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            c if depth == 0 => out.push(c),
            _ => {}
        }
    }
    let out = out.trim();
    let out = out
        .strip_prefix("Manga ")
        .or_else(|| out.strip_prefix("manga "))
        .or_else(|| out.strip_prefix("Manga-"))
        .or_else(|| out.strip_prefix("manga-"))
        .unwrap_or(out)
        .trim();
    if out.is_empty() {
        title.trim().to_string()
    } else {
        out.to_string()
    }
}

/// Normalised form used for match comparison: case and punctuation carry no
/// meaning across catalogues ("Hunter x Hunter" / "HUNTER×HUNTER"), so they
/// are flattened away — but nothing else is, so the comparison stays exact.
/// Unlike the web dashboard's JS version this keeps non-Latin scripts, which
/// there collapse to the empty string and would make every Japanese title
/// "equal" to every other.
fn norm_title(title: &str) -> String {
    let mut out = String::with_capacity(title.len());
    let mut pending_space = false;
    for c in title.chars() {
        if c.is_alphanumeric() {
            if pending_space && !out.is_empty() {
                out.push(' ');
            }
            pending_space = false;
            out.extend(c.to_lowercase());
        } else {
            pending_space = true;
        }
    }
    out
}

/// Pick the entry of a Jikan `/manga?q=` response whose title *is* `query`
/// (normalised) and return its metadata, or `None` when nothing matches that
/// strictly.
///
/// Novels are dropped the way the web sync drops them: MyAnimeList lists a
/// manga and its light-novel original under near-identical titles, and the
/// novel's chapter count and score describe a different work.
pub fn parse_metadata_match(body: &[u8], query: &str) -> Result<Option<SeriesMeta>> {
    #[derive(Deserialize)]
    struct Response {
        data: Vec<Entry>,
    }
    #[derive(Deserialize)]
    struct Entry {
        title: Option<String>,
        title_english: Option<String>,
        title_japanese: Option<String>,
        #[serde(default, deserialize_with = "lenient")]
        r#type: Option<String>,
        // Leniently parsed for the same reason the ranking is: MyAnimeList
        // leaves most of this null across its catalogue, and an odd value
        // must cost that field only.
        #[serde(default, deserialize_with = "lenient")]
        score: Option<f32>,
        #[serde(default, deserialize_with = "lenient")]
        status: Option<String>,
        #[serde(default, deserialize_with = "lenient")]
        genres: Option<Vec<Genre>>,
        #[serde(default, deserialize_with = "lenient")]
        rank: Option<u32>,
        #[serde(default, deserialize_with = "lenient")]
        chapters: Option<u32>,
    }
    #[derive(Deserialize)]
    struct Genre {
        name: Option<String>,
    }

    let response: Response =
        serde_json::from_slice(body).context("parsing MyAnimeList search response")?;
    let needle = norm_title(query);
    if needle.is_empty() {
        return Ok(None); // nothing to match on — never guess
    }

    for entry in response.data {
        if entry.r#type.as_deref().is_some_and(|t| {
            t.eq_ignore_ascii_case("novel") || t.eq_ignore_ascii_case("Light Novel")
        }) {
            continue;
        }
        let matched = [
            entry.title.as_deref(),
            entry.title_english.as_deref(),
            entry.title_japanese.as_deref(),
        ]
        .into_iter()
        .flatten()
        .any(|t| norm_title(t) == needle);
        if !matched {
            continue;
        }
        let meta = SeriesMeta {
            score: entry.score.filter(|s| s.is_finite()),
            status: entry
                .status
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty()),
            genres: entry
                .genres
                .unwrap_or_default()
                .into_iter()
                .filter_map(|g| g.name)
                .map(|n| n.trim().to_string())
                .filter(|n| !n.is_empty())
                .collect(),
            rank: entry.rank.filter(|r| *r > 0),
            // MyAnimeList reports 0 chapters for a series that hasn't
            // finished; that's "unknown", not "zero chapters".
            total_chapters: entry.chapters.filter(|c| *c > 0),
        };
        // An entry that matched but knows nothing is still no metadata.
        return Ok((!meta.is_empty()).then_some(meta));
    }
    Ok(None)
}

/// Run `fetch` and cache a non-empty result at `cache`; on a failed (or
/// empty) fetch, fall back to the previously cached list. MyAnimeList goes
/// down often enough (its API answers 504 for every request while it does)
/// that the Popular tab serves yesterday's ranking through an outage rather
/// than an error — only a first-ever failure, with nothing cached yet,
/// surfaces to the caller.
pub fn popular_with_cache(
    cache: &Path,
    fetch: impl FnOnce() -> Result<Vec<PopularManga>>,
) -> Result<Vec<PopularManga>> {
    match fetch() {
        Ok(popular) if !popular.is_empty() => {
            // Best-effort: failing to write the cache must not fail the fetch.
            if let Ok(json) = serde_json::to_vec(&popular) {
                let _ = std::fs::write(cache, json);
            }
            Ok(popular)
        }
        fresh => match std::fs::read(cache)
            .ok()
            .and_then(|json| serde_json::from_slice::<Vec<PopularManga>>(&json).ok())
            .filter(|cached| !cached.is_empty())
        {
            Some(cached) => Ok(cached),
            None => fresh,
        },
    }
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

/// Deserialise an optional field without ever failing: anything that isn't a
/// `T` — `null`, a missing key, or a value of the wrong shape — reads as
/// `None`. MyAnimeList's optional metadata is inconsistent enough that a
/// strict field would let one odd record fail the whole ranking.
fn lenient<'de, D, T>(deserializer: D) -> std::result::Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::de::DeserializeOwned,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(serde_json::from_value(value).ok())
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
        // The metadata below is absent or null for a good share of the
        // catalogue, so every field is read leniently: an unexpected shape
        // costs that one field, never the whole entry.
        #[serde(default, deserialize_with = "lenient")]
        score: Option<f32>,
        #[serde(default, deserialize_with = "lenient")]
        status: Option<String>,
        #[serde(default, deserialize_with = "lenient")]
        genres: Option<Vec<Genre>>,
        #[serde(default, deserialize_with = "lenient")]
        rank: Option<u32>,
        #[serde(default, deserialize_with = "lenient")]
        chapters: Option<u32>,
    }
    #[derive(Deserialize)]
    struct Genre {
        name: Option<String>,
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
                score: e.score,
                status: e
                    .status
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty()),
                genres: e
                    .genres
                    .unwrap_or_default()
                    .into_iter()
                    .filter_map(|g| g.name)
                    .map(|n| n.trim().to_string())
                    .filter(|n| !n.is_empty())
                    .collect(),
                rank: e.rank,
                chapters: e.chapters,
            })
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use gideon_sources::fetch::FakeFetcher;

    /// Shaped like a real `/top/manga` page: a complete entry, one with the
    /// metadata explicitly null (an ongoing series, as Jikan reports it), one
    /// with the metadata keys absent altogether, and a junk record.
    const SAMPLE: &str = r#"{
        "data": [
            {
                "mal_id": 2,
                "title": "Berserk",
                "title_english": "Berserk",
                "images": { "jpg": { "image_url": "https://cdn.myanimelist.net/berserk.jpg" } },
                "score": 9.47,
                "status": "Publishing",
                "rank": 1,
                "chapters": 374,
                "genres": [
                    { "mal_id": 1, "type": "manga", "name": "Action", "url": "https://myanimelist.net/manga/genre/1/Action" },
                    { "mal_id": 8, "type": "manga", "name": "Drama", "url": "https://myanimelist.net/manga/genre/8/Drama" }
                ]
            },
            {
                "mal_id": 656,
                "title": "Vagabond",
                "title_english": null,
                "images": { "jpg": { "image_url": null } },
                "score": null,
                "status": null,
                "rank": null,
                "chapters": null,
                "genres": []
            },
            {
                "mal_id": 23390,
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
    fn complete_entry_keeps_all_metadata() {
        let out = parse_popular(SAMPLE.as_bytes()).unwrap();
        let berserk = &out[0];
        // Title and cover are unaffected by the extra fields.
        assert_eq!(berserk.title, "Berserk");
        assert_eq!(
            berserk.cover_url.as_deref(),
            Some("https://cdn.myanimelist.net/berserk.jpg")
        );
        assert_eq!(berserk.score, Some(9.47));
        assert_eq!(berserk.status.as_deref(), Some("Publishing"));
        assert_eq!(berserk.genres, vec!["Action".to_string(), "Drama".into()]);
        assert_eq!(berserk.rank, Some(1));
        assert_eq!(berserk.chapters, Some(374));
    }

    #[test]
    fn null_metadata_is_none_not_a_dropped_entry() {
        let out = parse_popular(SAMPLE.as_bytes()).unwrap();
        let vagabond = &out[1];
        // Unchanged: the entry survives and still falls back to `title`.
        assert_eq!(vagabond.title, "Vagabond");
        assert_eq!(vagabond.cover_url, None);
        assert_eq!(vagabond.score, None);
        assert_eq!(vagabond.status, None);
        assert!(vagabond.genres.is_empty());
        assert_eq!(vagabond.rank, None);
        assert_eq!(vagabond.chapters, None);
    }

    #[test]
    fn absent_metadata_keys_are_none_not_a_dropped_entry() {
        let out = parse_popular(SAMPLE.as_bytes()).unwrap();
        let aot = &out[2];
        // Unchanged: English title still wins, missing images still mean None.
        assert_eq!(aot.title, "Attack on Titan");
        assert_eq!(aot.cover_url, None);
        assert_eq!(aot.score, None);
        assert_eq!(aot.status, None);
        assert!(aot.genres.is_empty());
        assert_eq!(aot.rank, None);
        assert_eq!(aot.chapters, None);
    }

    #[test]
    fn metadata_of_an_unexpected_shape_costs_only_that_field() {
        // Nothing here is the type Jikan documents; the entry must still
        // parse with its title and cover intact.
        const ODD: &str = r#"{
            "data": [
                {
                    "title": "One Piece",
                    "images": { "jpg": { "image_url": "https://cdn.myanimelist.net/op.jpg" } },
                    "score": "9.22",
                    "status": 7,
                    "genres": { "name": "Action" },
                    "rank": -3,
                    "chapters": 1.5
                }
            ]
        }"#;
        let out = parse_popular(ODD.as_bytes()).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].title, "One Piece");
        assert_eq!(
            out[0].cover_url.as_deref(),
            Some("https://cdn.myanimelist.net/op.jpg")
        );
        assert_eq!(out[0].score, None);
        assert_eq!(out[0].status, None);
        assert!(out[0].genres.is_empty());
        assert_eq!(out[0].rank, None);
        assert_eq!(out[0].chapters, None);
    }

    #[test]
    fn a_cache_written_before_the_metadata_existed_still_loads() {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("popular.json");
        std::fs::write(&cache, r#"[{"title":"Berserk","cover_url":null}]"#).unwrap();

        let out = popular_with_cache(&cache, || anyhow::bail!("MAL is down")).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].title, "Berserk");
        assert_eq!(out[0].score, None);
        assert!(out[0].genres.is_empty());
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

    #[test]
    fn popular_cache_serves_the_last_good_list_through_an_outage() {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("popular.json");
        let berserk = vec![PopularManga {
            title: "Berserk".into(),
            cover_url: None,
            score: Some(9.47),
            status: Some("Publishing".into()),
            genres: vec!["Action".into()],
            rank: Some(1),
            chapters: Some(374),
        }];

        // First fetch succeeds and populates the cache.
        let out = popular_with_cache(&cache, || Ok(berserk.clone())).unwrap();
        assert_eq!(out, berserk);

        // MyAnimeList goes down: the cached list is served, not the error.
        let out = popular_with_cache(&cache, || anyhow::bail!("MAL is down")).unwrap();
        assert_eq!(out, berserk);

        // An empty response falls back to the cache too.
        let out = popular_with_cache(&cache, || Ok(Vec::new())).unwrap();
        assert_eq!(out, berserk);
    }

    #[test]
    fn popular_cache_first_ever_failure_still_surfaces() {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("popular.json");
        assert!(popular_with_cache(&cache, || anyhow::bail!("MAL is down")).is_err());
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

    /// A `/manga?q=berserk` page: the exact match, a novel adaptation whose
    /// title normalises the same (must never win), and a fuzzy neighbour.
    const META_SAMPLE: &str = r#"{
        "data": [
            {
                "mal_id": 99,
                "title": "Berserk: The Prototype",
                "type": "manga",
                "score": 7.1,
                "status": "Finished",
                "chapters": 1
            },
            {
                "mal_id": 98,
                "title": "BERSERK!",
                "type": "Light Novel",
                "score": 5.0,
                "status": "Finished",
                "chapters": 3
            },
            {
                "mal_id": 2,
                "title": "Berserk",
                "title_english": "Berserk",
                "title_japanese": "ベルセルク",
                "type": "manga",
                "score": 9.47,
                "status": "Publishing",
                "rank": 1,
                "chapters": 0,
                "genres": [{ "name": "Action" }, { "name": "Drama" }]
            }
        ]
    }"#;

    #[test]
    fn metadata_needs_an_exact_title_match() {
        let meta = parse_metadata_match(META_SAMPLE.as_bytes(), "berserk")
            .unwrap()
            .expect("the exactly-titled entry matches");
        assert_eq!(meta.score, Some(9.47));
        assert_eq!(meta.status.as_deref(), Some("Publishing"));
        assert_eq!(meta.genres, vec!["Action".to_string(), "Drama".into()]);
        assert_eq!(meta.rank, Some(1));
        // 0 chapters means "still running", not zero.
        assert_eq!(meta.total_chapters, None);

        // A near-miss title is not a match: no metadata beats wrong metadata.
        assert_eq!(
            parse_metadata_match(META_SAMPLE.as_bytes(), "berserk prototype").unwrap(),
            None
        );
        assert_eq!(
            parse_metadata_match(META_SAMPLE.as_bytes(), "").unwrap(),
            None
        );
    }

    #[test]
    fn lookup_matches_through_punctuation_and_qualifiers() {
        // Folder-name noise the exact rule must survive (`series_query` drops
        // the parenthetical, `norm_title` the case and punctuation).
        let fetcher = FakeFetcher::new().with(
            "https://api.jikan.moe/v4/manga?q=Berserk&limit=5",
            META_SAMPLE,
        );
        let meta = metadata_for_title(&fetcher, "Berserk (Deluxe Edition)")
            .unwrap()
            .unwrap();
        assert_eq!(meta.rank, Some(1));
    }

    #[test]
    fn lookup_retries_with_the_titles_mal_knows() {
        // The user's spelling finds the right series only as a *synonym*;
        // the retry with MyAnimeList's own title is what lands the metadata.
        const BY_SYNONYM: &str = r#"{
            "data": [
                {
                    "title": "Judge",
                    "title_synonyms": ["Jajji"],
                    "type": "manga",
                    "score": 7.2
                }
            ]
        }"#;
        let fetcher = FakeFetcher::new()
            .with("https://api.jikan.moe/v4/manga?q=Jajji&limit=5", BY_SYNONYM)
            .with("https://api.jikan.moe/v4/manga?q=Judge&limit=5", BY_SYNONYM);
        let meta = metadata_for_title(&fetcher, "Jajji").unwrap().unwrap();
        assert_eq!(meta.score, Some(7.2));
    }

    #[test]
    fn an_unknown_title_is_none_not_an_error() {
        let fetcher = FakeFetcher::new().with(
            "https://api.jikan.moe/v4/manga?q=Nothing+Like+It&limit=5",
            r#"{ "data": [] }"#,
        );
        assert_eq!(
            metadata_for_title(&fetcher, "Nothing Like It").unwrap(),
            None
        );
    }

    #[test]
    fn a_search_failure_is_an_error_the_caller_can_swallow() {
        // Nothing canned: the fetch fails, exactly as it does offline.
        assert!(metadata_for_title(&FakeFetcher::new(), "Berserk").is_err());
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
