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
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
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
    /// already known for it.
    pub fn record(&mut self, series_dir: &str, origin: SeriesRef) {
        match self.series.get_mut(series_dir) {
            Some(existing) => {
                let downloaded = std::mem::take(&mut existing.downloaded);
                *existing = SeriesRef {
                    downloaded,
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

    #[test]
    fn malformed_file_is_an_empty_index() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".gideon")).unwrap();
        std::fs::write(dir.path().join(".gideon/series.json"), "{nope").unwrap();
        assert!(SeriesIndex::load(dir.path()).get("x").is_none());
    }
}
