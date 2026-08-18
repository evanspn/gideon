//! Which source/manga a downloaded series came from, persisted as
//! `.gideon/series.json` in the library root. Long-pressing a book on the
//! library shelf uses this to reopen the source's chapter list, so more
//! chapters of the same series can be downloaded from the card.
//!
//! Lenient like all gideon persistence: a missing or malformed file means
//! an empty index, never a crash; sideloaded series simply aren't linked.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::Result;

/// Where a series came from, and which of its chapters are on disk.
///
/// Not `Eq`: `SeriesMeta::score` is a float (MAL means are quoted to two
/// decimals), so only `PartialEq` is meaningful.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SeriesRef {
    pub source_id: String,
    pub source_name: String,
    pub manga_id: String,
    pub manga_title: String,
    /// Cover art URL, so a missing cover can be fetched later.
    #[serde(default)]
    pub cover_url: Option<String>,
    /// Downloaded chapters: chapter id → CBZ file name in the series dir.
    #[serde(default)]
    pub downloaded: BTreeMap<String, String>,
    /// MyAnimeList metadata, if it has ever been fetched for this series.
    ///
    /// `#[serde(default)]` is the load-bearing part: every `.gideon/
    /// series.json` already on a device predates this field, and those files
    /// must keep loading with every other field intact. `skip_serializing_if`
    /// is the other half — a series with no metadata is written exactly as it
    /// was before, so upgrading gideon doesn't rewrite the whole index with
    /// `"meta": null` noise.
    ///
    /// Parsed leniently (`lenient_meta`): metadata is a nicety fetched from a
    /// third-party API, so a half-written or wrong-shaped `meta` object
    /// degrades to `None`/absent fields rather than failing the load and
    /// costing the user their download history.
    #[serde(default, deserialize_with = "lenient_meta")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub meta: Option<SeriesMeta>,
}

/// MyAnimeList metadata about a series, cached on the device.
///
/// Every field is optional because MAL itself leaves them out: an
/// unpublished series has no `score` or `rank`, an ongoing one has no final
/// chapter count, and obscure entries can have no genres at all. Treating
/// "absent" as normal (rather than as an error, or as a sentinel like 0)
/// keeps the UI honest — it can show a dash instead of inventing a rating.
///
/// Fields are parsed leniently for the same reason the rest of gideon's
/// persistence is: this file lives on a device that can lose power mid-write,
/// and a single wrong-typed value must never cost the user the series index.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SeriesMeta {
    /// MAL mean score, e.g. 8.74. Parsed leniently — a non-finite or
    /// non-numeric value becomes `None` (JSON cannot represent NaN/Infinity,
    /// so storing one would silently round-trip to `null` anyway).
    #[serde(default, deserialize_with = "lenient_score")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub score: Option<f32>,

    /// Publication status as MAL words it, e.g. "Publishing", "Finished".
    /// Kept as a free-form string rather than an enum: MAL has added status
    /// values before, and an unknown one should display as-is instead of
    /// being flattened into "unknown". Parsed leniently — anything that
    /// isn't a non-empty string becomes `None`.
    #[serde(default, deserialize_with = "lenient_status")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,

    /// Genre names, in MAL's order. Parsed leniently — non-string and blank
    /// entries are dropped, and a wrong-typed list becomes empty.
    #[serde(default, deserialize_with = "lenient_genres")]
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub genres: Vec<String>,

    /// MAL popularity/score rank (1 = highest). Parsed leniently — anything
    /// that isn't a whole number fitting in `u32` becomes `None`.
    #[serde(default, deserialize_with = "lenient_count")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rank: Option<u32>,

    /// Total chapters the series will have, when known. Ongoing series
    /// report 0 on MAL for "not finished yet"; that is stored as `None` so
    /// the UI never renders "chapter 5 of 0". Parsed leniently.
    #[serde(default, deserialize_with = "lenient_count")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_chapters: Option<u32>,

    /// When this metadata was last fetched, as a unix timestamp.
    ///
    /// Stored beside the manga so a re-read costs nothing and a refresh is a
    /// decision rather than a habit: a score and a genre list do not move,
    /// and the only fields that go stale in practice are the chapter count
    /// (new chapters) and the score (a series ending badly). So the device
    /// re-checks on the order of a fortnight, not on every library open.
    /// `None` means metadata cached before this field existed — treated as
    /// due, so it refreshes once and then falls into the normal cadence.
    #[serde(default, deserialize_with = "lenient_timestamp")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fetched_at: Option<u64>,
}

impl SeriesMeta {
    /// Whether nothing at all is known. Callers use this to avoid attaching
    /// an all-empty `meta` to a series just because a lookup was attempted.
    pub fn is_empty(&self) -> bool {
        // `fetched_at` deliberately does not count: a timestamp with nothing
        // attached to it is not knowledge about the series, and treating it
        // as such would attach an empty `meta` to everything ever looked up.
        self.score.is_none()
            && self.status.is_none()
            && self.genres.is_empty()
            && self.rank.is_none()
            && self.total_chapters.is_none()
    }

    /// Whether this metadata is old enough to be worth re-checking, `now`
    /// and the age both in seconds. Metadata with no stamp is due.
    pub fn is_stale(&self, now: u64, max_age: u64) -> bool {
        match self.fetched_at {
            // A stamp in the future (a clock that was wrong when it was
            // written) reads as "just fetched" rather than as an age of
            // billions of seconds, so a bad clock cannot cause a refresh
            // storm.
            Some(at) => now.saturating_sub(at) >= max_age,
            None => true,
        }
    }
}

/// Lenient `meta` parsing: any JSON value is accepted, and only an object
/// that actually parses becomes `Some`. A `null`, a leftover string, or a
/// truncated object means "no metadata yet", never a failed load.
fn lenient_meta<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Option<SeriesMeta>, D::Error> {
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(serde_json::from_value::<SeriesMeta>(value)
        .ok()
        .filter(|meta| !meta.is_empty()))
}

/// Lenient `fetched_at` parsing: a whole non-negative number passes through,
/// anything else means "no stamp", which reads as due for a refresh.
fn lenient_timestamp<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Option<u64>, D::Error> {
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(value.as_u64())
}

/// Lenient `score` parsing: a finite number (or a numeric string, which is
/// how some MAL mirrors quote it) passes through; anything else is `None`.
fn lenient_score<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Option<f32>, D::Error> {
    let value = serde_json::Value::deserialize(deserializer)?;
    let number = value
        .as_f64()
        .or_else(|| value.as_str().and_then(|s| s.trim().parse::<f64>().ok()));
    Ok(number.map(|n| n as f32).filter(|n| n.is_finite()))
}

/// Lenient `status` parsing: a non-empty string passes through (trimmed);
/// anything else means `None`.
fn lenient_status<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Option<String>, D::Error> {
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(value
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string))
}

/// Lenient `genres` parsing: non-string and blank entries are dropped, and a
/// wrong-typed value (or `null`) yields an empty list.
fn lenient_genres<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Vec<String>, D::Error> {
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(value
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default())
}

/// Lenient count parsing for `rank` / `total_chapters`: a whole number that
/// fits in `u32` passes through, and 0 is treated as "unknown" (MAL reports
/// 0 for an ongoing series' chapter count and for unranked entries).
/// Anything else — a float, a negative, a string, `null` — is `None`.
fn lenient_count<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Option<u32>, D::Error> {
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(value
        .as_u64()
        .filter(|n| *n > 0 && *n <= u32::MAX as u64)
        .map(|n| n as u32))
}

/// Map from series directory name (under the library root) to its origin.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct SeriesIndex {
    #[serde(default)]
    series: BTreeMap<String, SeriesRef>,
}

impl SeriesIndex {
    /// Load the index, treating a missing or unreadable file as empty.
    pub fn load(library: &Path) -> Self {
        std::fs::read_to_string(Self::path(library))
            .ok()
            .and_then(|raw| serde_json::from_str(&raw).ok())
            .unwrap_or_default()
    }

    /// The origin of a series directory, if it was downloaded via a source.
    pub fn get(&self, series_dir: &str) -> Option<&SeriesRef> {
        self.series.get(series_dir)
    }

    /// Record (or refresh) a series' origin, keeping any download history
    /// already known for it — and likewise any MyAnimeList metadata, which
    /// the download path doesn't carry: re-recording an origin after every
    /// chapter download must not silently throw the cached metadata away.
    /// An origin that *does* carry metadata wins, so a refresh can update it.
    pub fn record(&mut self, series_dir: &str, origin: SeriesRef) {
        match self.series.get_mut(series_dir) {
            Some(existing) => {
                let downloaded = std::mem::take(&mut existing.downloaded);
                let meta = origin.meta.clone().or_else(|| existing.meta.take());
                *existing = SeriesRef {
                    downloaded,
                    meta,
                    ..origin
                };
            }
            None => {
                self.series.insert(series_dir.to_string(), origin);
            }
        }
    }

    /// Record that a chapter of `series_dir` is on disk.
    pub fn record_download(&mut self, series_dir: &str, chapter_id: &str, file_name: &str) {
        if let Some(series) = self.series.get_mut(series_dir) {
            series
                .downloaded
                .insert(chapter_id.to_string(), file_name.to_string());
        }
    }

    /// Forget a downloaded chapter (e.g. after deleting its file).
    pub fn forget_download(&mut self, series_dir: &str, file_name: &str) {
        if let Some(series) = self.series.get_mut(series_dir) {
            series.downloaded.retain(|_, f| f != file_name);
        }
    }

    /// Drop a series entirely (e.g. after deleting its directory).
    pub fn remove(&mut self, series_dir: &str) {
        self.series.remove(series_dir);
    }

    /// Every recorded series directory and its origin, in directory order.
    /// Used by storage accounting to size up what's been downloaded.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &SeriesRef)> {
        self.series.iter().map(|(dir, r)| (dir.as_str(), r))
    }

    /// The series downloaded from this source/manga, if any.
    pub fn find_manga(&self, source_id: &str, manga_id: &str) -> Option<(&str, &SeriesRef)> {
        self.series
            .iter()
            .find(|(_, r)| r.source_id == source_id && r.manga_id == manga_id)
            .map(|(dir, r)| (dir.as_str(), r))
    }

    /// Persist atomically (temp file + rename).
    pub fn save(&self, library: &Path) -> Result<()> {
        let path = Self::path(library);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(self)?)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    fn path(library: &Path) -> PathBuf {
        library.join(".gideon").join("series.json")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn origin() -> SeriesRef {
        SeriesRef {
            source_id: "multi.mangadex".into(),
            source_name: "MangaDex".into(),
            manga_id: "m1".into(),
            manga_title: "Manga One".into(),
            ..SeriesRef::default()
        }
    }

    #[test]
    fn round_trips_through_the_library_dir() {
        let dir = tempfile::tempdir().unwrap();
        let mut index = SeriesIndex::load(dir.path());
        assert!(index.get("Manga One").is_none());

        index.record("Manga One", origin());
        index.save(dir.path()).unwrap();

        let reloaded = SeriesIndex::load(dir.path());
        assert_eq!(reloaded.get("Manga One"), Some(&origin()));
        assert!(reloaded.get("Sideloaded").is_none());
    }

    #[test]
    fn re_recording_keeps_the_download_history() {
        let dir = tempfile::tempdir().unwrap();
        let mut index = SeriesIndex::load(dir.path());
        index.record("Manga One", origin());
        index.record_download("Manga One", "c1", "Chapter 1.cbz");
        // A later download re-records the origin; history must survive.
        index.record("Manga One", origin());
        assert_eq!(
            index.get("Manga One").unwrap().downloaded.get("c1"),
            Some(&"Chapter 1.cbz".to_string())
        );

        index.forget_download("Manga One", "Chapter 1.cbz");
        assert!(index.get("Manga One").unwrap().downloaded.is_empty());

        assert_eq!(
            index.find_manga("multi.mangadex", "m1").map(|(d, _)| d),
            Some("Manga One")
        );
        index.remove("Manga One");
        assert!(index.get("Manga One").is_none());
    }

    fn meta() -> SeriesMeta {
        SeriesMeta {
            score: Some(8.74),
            status: Some("Publishing".into()),
            genres: vec!["Action".into(), "Drama".into()],
            rank: Some(42),
            total_chapters: Some(120),
            fetched_at: Some(1_700_000_000),
        }
    }

    #[test]
    fn metadata_goes_stale_on_a_schedule_not_on_every_read() {
        // A score and a genre list do not move; the chapter count does when
        // new chapters land, and a score can slide after a bad ending. So
        // metadata is re-checked on the order of a fortnight rather than
        // every time the library is opened.
        let fresh = SeriesMeta {
            fetched_at: Some(1_000_000),
            ..meta()
        };
        const FORTNIGHT: u64 = 14 * 24 * 3600;
        assert!(!fresh.is_stale(1_000_000 + FORTNIGHT - 1, FORTNIGHT));
        assert!(fresh.is_stale(1_000_000 + FORTNIGHT, FORTNIGHT));

        // Cached before the stamp existed: due once, then on the cadence.
        let unstamped = SeriesMeta {
            fetched_at: None,
            ..meta()
        };
        assert!(unstamped.is_stale(0, FORTNIGHT));

        // A stamp from a clock that was wrong reads as just-fetched rather
        // than as an age of billions of seconds.
        let future = SeriesMeta {
            fetched_at: Some(9_000_000),
            ..meta()
        };
        assert!(!future.is_stale(1_000_000, FORTNIGHT));

        // A stamp alone is not knowledge about the series.
        assert!(SeriesMeta {
            fetched_at: Some(1_000_000),
            ..SeriesMeta::default()
        }
        .is_empty());
    }

    #[test]
    fn metadata_round_trips_through_the_library_dir() {
        let dir = tempfile::tempdir().unwrap();
        let mut index = SeriesIndex::load(dir.path());
        index.record(
            "Manga One",
            SeriesRef {
                meta: Some(meta()),
                ..origin()
            },
        );
        index.save(dir.path()).unwrap();

        let reloaded = SeriesIndex::load(dir.path());
        assert_eq!(reloaded.get("Manga One").unwrap().meta, Some(meta()));
    }

    #[test]
    fn an_old_file_without_metadata_still_loads() {
        // Byte-for-byte the shape written by gideon builds that predate
        // SeriesMeta. Every other field must survive, and meta is simply
        // absent — this is the compatibility guarantee for devices in the
        // field.
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".gideon")).unwrap();
        std::fs::write(
            dir.path().join(".gideon/series.json"),
            r#"{
              "series": {
                "Manga One": {
                  "source_id": "multi.mangadex",
                  "source_name": "MangaDex",
                  "manga_id": "m1",
                  "manga_title": "Manga One",
                  "cover_url": "https://example.invalid/c.jpg",
                  "downloaded": { "c1": "Chapter 1.cbz" }
                }
              }
            }"#,
        )
        .unwrap();

        let index = SeriesIndex::load(dir.path());
        let series = index.get("Manga One").expect("old entry must still load");
        assert_eq!(series.manga_title, "Manga One");
        assert_eq!(
            series.cover_url.as_deref(),
            Some("https://example.invalid/c.jpg")
        );
        assert_eq!(series.downloaded.get("c1"), Some(&"Chapter 1.cbz".into()));
        assert_eq!(series.meta, None);

        // And re-saving such a series doesn't grow a "meta" key.
        index.save(dir.path()).unwrap();
        let raw = std::fs::read_to_string(dir.path().join(".gideon/series.json")).unwrap();
        assert!(!raw.contains("meta"), "{raw}");
    }

    #[test]
    fn partial_metadata_survives_and_junk_degrades_to_none() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".gideon")).unwrap();
        std::fs::write(
            dir.path().join(".gideon/series.json"),
            r#"{
              "series": {
                "Partial": {
                  "source_id": "s", "source_name": "S",
                  "manga_id": "m", "manga_title": "Partial",
                  "meta": { "score": 7.5, "genres": ["Comedy"] }
                },
                "Junk": {
                  "source_id": "s", "source_name": "S",
                  "manga_id": "m2", "manga_title": "Junk",
                  "meta": {
                    "score": "not a number",
                    "status": "",
                    "genres": "Action",
                    "rank": -3,
                    "total_chapters": 0
                  }
                },
                "NotAnObject": {
                  "source_id": "s", "source_name": "S",
                  "manga_id": "m3", "manga_title": "NotAnObject",
                  "meta": "???"
                }
              }
            }"#,
        )
        .unwrap();

        let index = SeriesIndex::load(dir.path());
        let partial = index.get("Partial").unwrap().meta.clone().unwrap();
        assert_eq!(partial.score, Some(7.5));
        assert_eq!(partial.genres, vec!["Comedy".to_string()]);
        assert_eq!(partial.status, None);
        assert_eq!(partial.rank, None);
        assert_eq!(partial.total_chapters, None);

        // Every field junk (0/negative counts included) collapses to an
        // empty struct, which is stored as "no metadata" rather than an
        // empty object.
        assert_eq!(index.get("Junk").unwrap().meta, None);
        assert_eq!(index.get("NotAnObject").unwrap().meta, None);
    }

    #[test]
    fn re_recording_keeps_metadata_unless_the_new_origin_has_some() {
        let dir = tempfile::tempdir().unwrap();
        let mut index = SeriesIndex::load(dir.path());
        index.record(
            "Manga One",
            SeriesRef {
                meta: Some(meta()),
                ..origin()
            },
        );

        // The download path re-records without metadata: keep the cache.
        index.record("Manga One", origin());
        assert_eq!(index.get("Manga One").unwrap().meta, Some(meta()));

        // A refresh that carries metadata replaces it.
        let refreshed = SeriesMeta {
            score: Some(9.0),
            ..SeriesMeta::default()
        };
        index.record(
            "Manga One",
            SeriesRef {
                meta: Some(refreshed.clone()),
                ..origin()
            },
        );
        assert_eq!(index.get("Manga One").unwrap().meta, Some(refreshed));
    }

    #[test]
    fn malformed_file_is_an_empty_index() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".gideon")).unwrap();
        std::fs::write(dir.path().join(".gideon/series.json"), "{nope").unwrap();
        assert!(SeriesIndex::load(dir.path()).get("x").is_none());
    }
}
