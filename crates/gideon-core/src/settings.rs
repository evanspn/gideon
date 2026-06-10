//! User settings, persisted as `settings.json` in the data directory.
//!
//! Mirrors the shape of bobo's settings where it makes sense (source lists,
//! languages, storage size limit) and follows the lessons learned there:
//! parsing is lenient — unknown fields are ignored, missing fields get
//! defaults, and a malformed file produces a clear error instead of a crash.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::Result;

/// Default storage limit for downloaded chapters: 2 GB, same as bobo.
pub const DEFAULT_STORAGE_LIMIT_BYTES: u64 = 2 * 1024 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Extra Aidoku-compatible source list URLs, on top of the preinstalled
    /// defaults.
    pub source_lists: Vec<String>,

    /// If set, only show sources/chapters for these languages.
    pub languages: Vec<String>,

    /// Storage budget for downloaded chapters, e.g. "2 GB" or "500 MB".
    /// Oldest-read chapters are evicted when the budget is exceeded.
    pub storage_size_limit: StorageSize,

    /// How many unread chapters to pre-download ahead of the one being read.
    /// 0 disables pre-downloading.
    pub predownload_unread_chapters: u32,

    /// Check GitHub releases for gideon updates automatically.
    pub auto_check_updates: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            source_lists: Vec::new(),
            languages: Vec::new(),
            storage_size_limit: StorageSize(DEFAULT_STORAGE_LIMIT_BYTES),
            predownload_unread_chapters: 2,
            auto_check_updates: true,
        }
    }
}

impl Settings {
    /// Load settings from `dir/settings.json`, returning defaults when the
    /// file doesn't exist yet.
    pub fn load(dir: &Path) -> Result<Self> {
        let path = Self::path(dir);
        match fs::read_to_string(&path) {
            Ok(contents) => Ok(serde_json::from_str(&contents)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e.into()),
        }
    }

    /// Persist settings atomically (temp file + rename).
    pub fn save(&self, dir: &Path) -> Result<()> {
        fs::create_dir_all(dir)?;
        let path = Self::path(dir);
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, serde_json::to_string_pretty(self)?)?;
        fs::rename(&tmp, path)?;
        Ok(())
    }

    pub fn path(dir: &Path) -> PathBuf {
        dir.join("settings.json")
    }
}

/// A storage size that round-trips through human-friendly strings
/// ("2 GB", "500 MB", "1.5 GB") but is used as bytes internally.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StorageSize(pub u64);

impl StorageSize {
    pub fn bytes(self) -> u64 {
        self.0
    }
}

impl std::fmt::Display for StorageSize {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        const GB: u64 = 1024 * 1024 * 1024;
        const MB: u64 = 1024 * 1024;
        if self.0 >= GB && self.0.is_multiple_of(GB / 100) {
            let whole = self.0 / GB;
            let frac = (self.0 % GB) * 100 / GB;
            if frac == 0 {
                write!(f, "{whole} GB")
            } else {
                write!(f, "{whole}.{frac:02} GB")
            }
        } else {
            write!(f, "{} MB", self.0 / MB)
        }
    }
}

impl Serialize for StorageSize {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for StorageSize {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        parse_storage_size(&raw).map(StorageSize).ok_or_else(|| {
            serde::de::Error::custom(format!(
                "invalid storage size '{raw}' (expected e.g. \"2 GB\" or \"500 MB\")"
            ))
        })
    }
}

/// Parse "<number> <GB|MB>" (case-insensitive, whitespace-lenient) to bytes.
pub fn parse_storage_size(raw: &str) -> Option<u64> {
    let cleaned = raw.trim().to_ascii_uppercase();
    let (number_part, unit) = if let Some(n) = cleaned.strip_suffix("GB") {
        (n, 1024u64 * 1024 * 1024)
    } else if let Some(n) = cleaned.strip_suffix("MB") {
        (n, 1024u64 * 1024)
    } else {
        return None;
    };

    let value: f64 = number_part.trim().parse().ok()?;
    if !(value > 0.0 && value.is_finite()) {
        return None;
    }
    Some((value * unit as f64) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_bobo_conventions() {
        let s = Settings::default();
        assert_eq!(s.storage_size_limit.bytes(), 2 * 1024 * 1024 * 1024);
        assert_eq!(s.predownload_unread_chapters, 2);
        assert!(s.auto_check_updates);
        assert!(s.source_lists.is_empty());
    }

    #[test]
    fn load_missing_file_gives_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let s = Settings::load(dir.path()).unwrap();
        assert_eq!(s, Settings::default());
    }

    #[test]
    fn settings_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let s = Settings {
            source_lists: vec!["https://example.com/index.json".into()],
            languages: vec!["en".into(), "es".into()],
            storage_size_limit: StorageSize(500 * 1024 * 1024),
            predownload_unread_chapters: 5,
            auto_check_updates: false,
        };
        s.save(dir.path()).unwrap();

        let loaded = Settings::load(dir.path()).unwrap();
        assert_eq!(loaded, s);
    }

    #[test]
    fn parsing_is_lenient_about_unknown_and_missing_fields() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            Settings::path(dir.path()),
            r#"{"languages": ["en"], "future_field": {"nested": true}}"#,
        )
        .unwrap();
        let s = Settings::load(dir.path()).unwrap();
        assert_eq!(s.languages, vec!["en"]);
        // Everything else got defaults.
        assert_eq!(s.storage_size_limit.bytes(), DEFAULT_STORAGE_LIMIT_BYTES);
    }

    #[test]
    fn storage_size_parsing() {
        assert_eq!(parse_storage_size("2 GB"), Some(2 * 1024 * 1024 * 1024));
        assert_eq!(parse_storage_size("500 MB"), Some(500 * 1024 * 1024));
        assert_eq!(
            parse_storage_size("1.5 GB"),
            Some(1024 * 1024 * 1024 * 3 / 2)
        );
        assert_eq!(parse_storage_size("  2gb "), Some(2 * 1024 * 1024 * 1024));
        assert_eq!(parse_storage_size("0 GB"), None);
        assert_eq!(parse_storage_size("-1 GB"), None);
        assert_eq!(parse_storage_size("lots"), None);
        assert_eq!(parse_storage_size("2 TB"), None);
    }

    #[test]
    fn storage_size_display_round_trips() {
        for size in [
            StorageSize(2 * 1024 * 1024 * 1024),
            StorageSize(500 * 1024 * 1024),
        ] {
            let displayed = size.to_string();
            assert_eq!(
                parse_storage_size(&displayed),
                Some(size.bytes()),
                "{displayed}"
            );
        }
    }

    #[test]
    fn malformed_storage_size_is_a_clear_error() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            Settings::path(dir.path()),
            r#"{"storage_size_limit": "much wow"}"#,
        )
        .unwrap();
        let err = Settings::load(dir.path()).unwrap_err();
        assert!(
            err.to_string().contains("much wow"),
            "unhelpful error: {err}"
        );
    }
}
