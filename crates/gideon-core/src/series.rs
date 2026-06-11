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

/// Where a series came from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SeriesRef {
    pub source_id: String,
    pub source_name: String,
    pub manga_id: String,
    pub manga_title: String,
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

    pub fn record(&mut self, series_dir: &str, origin: SeriesRef) {
        self.series.insert(series_dir.to_string(), origin);
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
    fn malformed_file_is_an_empty_index() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".gideon")).unwrap();
        std::fs::write(dir.path().join(".gideon/series.json"), "{nope").unwrap();
        assert!(SeriesIndex::load(dir.path()).get("x").is_none());
    }
}
