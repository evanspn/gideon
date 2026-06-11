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

    /// Reader fit mode: "contain" (whole page visible) or "fit-width"
    /// (page fills the screen width and scrolls vertically). Parsed
    /// leniently — unknown values behave like "contain".
    #[serde(deserialize_with = "lenient_reader_fit")]
    pub reader_fit: String,

    /// Reader rotation in degrees: 0, 90, 180 or 270. Parsed leniently —
    /// anything else behaves like 0.
    #[serde(deserialize_with = "lenient_reader_rotation")]
    pub reader_rotation: u32,

    /// Frontlight brightness percent (0–100), restored at startup and
    /// updated from the reader's right-edge slide. Parsed leniently.
    #[serde(deserialize_with = "lenient_percent")]
    pub frontlight_brightness: u32,

    /// Frontlight warmth ("night light") percent (0–100), restored at
    /// startup and updated from the reader's left-edge slide.
    #[serde(deserialize_with = "lenient_percent")]
    pub frontlight_warmth: u32,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            source_lists: Vec::new(),
            languages: Vec::new(),
            storage_size_limit: StorageSize(DEFAULT_STORAGE_LIMIT_BYTES),
            predownload_unread_chapters: 2,
            auto_check_updates: true,
            reader_fit: "contain".to_string(),
            reader_rotation: 0,
            frontlight_brightness: 20,
            frontlight_warmth: 0,
        }
    }
}

/// Lenient percent parsing: numbers are clamped to 0–100; anything else
/// (wrong type, missing) falls back to 0.
fn lenient_percent<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<u32, D::Error> {
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(value.as_u64().map_or(0, |v| v.min(100) as u32))
}

/// Lenient `reader_fit` parsing: any JSON value is accepted; only strings
/// pass through (normalized to lowercase), everything else means "contain".
fn lenient_reader_fit<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<String, D::Error> {
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(match value.as_str() {
        Some(s) => s.trim().to_ascii_lowercase(),
        None => "contain".to_string(),
    })
}

/// Lenient `reader_rotation` parsing: only 0/90/180/270 are kept; any other
/// value (wrong number, wrong type) falls back to 0 instead of erroring.
fn lenient_reader_rotation<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<u32, D::Error> {
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(match value.as_u64() {
        Some(degrees @ (90 | 180 | 270)) => degrees as u32,
        _ => 0,
    })
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
        assert_eq!(s.reader_fit, "contain");
        assert_eq!(s.reader_rotation, 0);
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
            reader_fit: "fit-width".into(),
            reader_rotation: 90,
            frontlight_brightness: 65,
            frontlight_warmth: 40,
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
    fn reader_fit_parses_leniently() {
        let load = |json: &str| {
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(Settings::path(dir.path()), json).unwrap();
            Settings::load(dir.path()).unwrap()
        };
        // Valid values pass through (normalized).
        assert_eq!(
            load(r#"{"reader_fit": "fit-width"}"#).reader_fit,
            "fit-width"
        );
        assert_eq!(
            load(r#"{"reader_fit": " FIT-WIDTH "}"#).reader_fit,
            "fit-width"
        );
        assert_eq!(load(r#"{"reader_fit": "contain"}"#).reader_fit, "contain");
        // Unknown strings are kept (the consumer treats them as contain),
        // wrong types fall back to contain instead of erroring.
        assert_eq!(load(r#"{"reader_fit": "sideways"}"#).reader_fit, "sideways");
        assert_eq!(load(r#"{"reader_fit": 42}"#).reader_fit, "contain");
        assert_eq!(load(r#"{"reader_fit": null}"#).reader_fit, "contain");
    }

    #[test]
    fn reader_rotation_parses_leniently() {
        let load = |json: &str| {
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(Settings::path(dir.path()), json).unwrap();
            Settings::load(dir.path()).unwrap()
        };
        assert_eq!(load(r#"{"reader_rotation": 90}"#).reader_rotation, 90);
        assert_eq!(load(r#"{"reader_rotation": 180}"#).reader_rotation, 180);
        assert_eq!(load(r#"{"reader_rotation": 270}"#).reader_rotation, 270);
        assert_eq!(load(r#"{"reader_rotation": 0}"#).reader_rotation, 0);
        // Invalid angles and wrong types never error — they mean 0.
        assert_eq!(load(r#"{"reader_rotation": 45}"#).reader_rotation, 0);
        assert_eq!(load(r#"{"reader_rotation": -90}"#).reader_rotation, 0);
        assert_eq!(load(r#"{"reader_rotation": "90"}"#).reader_rotation, 0);
        assert_eq!(load(r#"{"reader_rotation": null}"#).reader_rotation, 0);
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
