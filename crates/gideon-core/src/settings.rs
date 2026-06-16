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

    /// Profiles: each profile sees its own library subdirectory. The
    /// "default" profile uses the library root (existing books stay
    /// visible); any other profile lives in `<library>/@<name>`. Parsed
    /// leniently — non-string entries are dropped, and an empty or
    /// wrong-typed list falls back to just "default".
    #[serde(deserialize_with = "lenient_profiles")]
    pub profiles: Vec<String>,

    /// The profile whose library is currently shown. Parsed leniently —
    /// anything but a non-empty string means "default".
    #[serde(deserialize_with = "lenient_profile_name")]
    pub active_profile: String,

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

    /// Whether the reading orientation is locked: rotation changes (the
    /// reader's rotate gesture / controls sheet) persist across sessions
    /// when locked, and stay session-only otherwise ("auto"). Parsed
    /// leniently — anything but a JSON bool means the default (locked).
    #[serde(deserialize_with = "lenient_bool_locked")]
    pub reader_rotation_locked: bool,

    /// Kaleido color post-process: "vivid" (the strongest saturation boost,
    /// the default), "standard" (no boost — clears rainbow banding on
    /// gradients) or "off". Parsed leniently — unknown values behave like
    /// "vivid".
    #[serde(deserialize_with = "lenient_color_post_process")]
    pub color_post_process: String,

    /// Page turns between full (flashing) e-ink refreshes. Higher flashes
    /// less often (smoother reading) but lets ghosting build up longer.
    /// Parsed leniently — out-of-range or wrong-typed values fall back to the
    /// default (8); clamped to 4–24.
    #[serde(deserialize_with = "lenient_full_refresh_interval")]
    pub reader_full_refresh_interval: u32,

    /// Whether gideon may bring Wi-Fi up on its own (before a network action
    /// and on wake). Off = never auto-connect; the user connects manually from
    /// the Wi-Fi controls. Parsed leniently — non-bool means the default
    /// (true). (`GIDEON_WIFI_AUTOENABLE=0` is a separate hard override.)
    #[serde(deserialize_with = "lenient_bool_true")]
    pub wifi_auto_connect: bool,

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
            profiles: vec!["default".to_string()],
            active_profile: "default".to_string(),
            storage_size_limit: StorageSize(DEFAULT_STORAGE_LIMIT_BYTES),
            predownload_unread_chapters: 2,
            auto_check_updates: true,
            reader_fit: "contain".to_string(),
            reader_rotation: 0,
            reader_rotation_locked: true,
            color_post_process: "vivid".to_string(),
            reader_full_refresh_interval: 8,
            wifi_auto_connect: true,
            frontlight_brightness: 20,
            frontlight_warmth: 0,
        }
    }
}

/// Lenient bool defaulting to `true`: a JSON bool passes through; anything
/// else (wrong type, missing) means `true`.
fn lenient_bool_true<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<bool, D::Error> {
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(value.as_bool().unwrap_or(true))
}

/// Lenient `reader_rotation_locked` parsing: only a JSON bool passes
/// through; anything else (wrong type, missing) means locked.
fn lenient_bool_locked<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<bool, D::Error> {
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(value.as_bool().unwrap_or(true))
}

/// Lenient percent parsing: numbers are clamped to 0–100; anything else
/// (wrong type, missing) falls back to 0.
fn lenient_percent<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<u32, D::Error> {
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(value.as_u64().map_or(0, |v| v.min(100) as u32))
}

/// Lenient profile-list parsing: only non-empty string entries are kept
/// (trimmed); an empty or wrong-typed list falls back to just "default".
fn lenient_profiles<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Vec<String>, D::Error> {
    let value = serde_json::Value::deserialize(deserializer)?;
    let mut profiles: Vec<String> = value
        .as_array()
        .map(|entries| {
            entries
                .iter()
                .filter_map(|e| e.as_str())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default();
    if profiles.is_empty() {
        profiles.push("default".to_string());
    }
    Ok(profiles)
}

/// Lenient `active_profile` parsing: a non-empty string passes through
/// (trimmed); anything else (wrong type, missing) means "default".
fn lenient_profile_name<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<String, D::Error> {
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(match value.as_str().map(str::trim) {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => "default".to_string(),
    })
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

/// Lenient `color_post_process` parsing: known tokens pass through
/// lowercased; anything else (wrong type, missing) means "vivid".
fn lenient_color_post_process<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<String, D::Error> {
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(
        match value.as_str().map(|s| s.trim().to_ascii_lowercase()) {
            Some(s) if s == "standard" || s == "off" => s,
            _ => "vivid".to_string(),
        },
    )
}

/// Lenient `reader_full_refresh_interval` parsing: a number clamped to
/// 4–24; anything else (wrong type, missing, out of range) falls back to 8.
fn lenient_full_refresh_interval<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<u32, D::Error> {
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(match value.as_u64() {
        Some(n) if (4..=24).contains(&n) => n as u32,
        _ => 8,
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
        assert_eq!(s.profiles, vec!["default"]);
        assert_eq!(s.active_profile, "default");
        assert_eq!(s.reader_fit, "contain");
        assert_eq!(s.reader_rotation, 0);
        assert!(s.reader_rotation_locked);
        assert_eq!(s.color_post_process, "vivid");
        assert_eq!(s.reader_full_refresh_interval, 8);
        assert!(s.wifi_auto_connect);
    }

    #[test]
    fn wifi_auto_connect_parses_leniently() {
        let load = |json: &str| {
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(Settings::path(dir.path()), json).unwrap();
            Settings::load(dir.path()).unwrap().wifi_auto_connect
        };
        assert!(!load(r#"{"wifi_auto_connect": false}"#));
        assert!(load(r#"{"wifi_auto_connect": true}"#));
        // Wrong-typed / missing default to true.
        assert!(load(r#"{"wifi_auto_connect": "no"}"#));
        assert!(load(r#"{}"#));
    }

    #[test]
    fn full_refresh_interval_parses_leniently() {
        let load = |json: &str| {
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(Settings::path(dir.path()), json).unwrap();
            Settings::load(dir.path())
                .unwrap()
                .reader_full_refresh_interval
        };
        assert_eq!(load(r#"{"reader_full_refresh_interval": 12}"#), 12);
        assert_eq!(load(r#"{"reader_full_refresh_interval": 4}"#), 4);
        assert_eq!(load(r#"{"reader_full_refresh_interval": 24}"#), 24);
        // Out of range, wrong type and missing all fall back to 8.
        assert_eq!(load(r#"{"reader_full_refresh_interval": 1}"#), 8);
        assert_eq!(load(r#"{"reader_full_refresh_interval": 99}"#), 8);
        assert_eq!(load(r#"{"reader_full_refresh_interval": "x"}"#), 8);
        assert_eq!(load(r#"{}"#), 8);
    }

    #[test]
    fn color_post_process_parses_leniently() {
        let load = |json: &str| {
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(Settings::path(dir.path()), json).unwrap();
            Settings::load(dir.path()).unwrap().color_post_process
        };
        assert_eq!(load(r#"{"color_post_process": "standard"}"#), "standard");
        assert_eq!(load(r#"{"color_post_process": "OFF"}"#), "off");
        assert_eq!(load(r#"{"color_post_process": "vivid"}"#), "vivid");
        // Unknown / wrong-typed / missing all fall back to vivid.
        assert_eq!(load(r#"{"color_post_process": "nope"}"#), "vivid");
        assert_eq!(load(r#"{"color_post_process": 5}"#), "vivid");
        assert_eq!(load(r#"{}"#), "vivid");
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
            profiles: vec!["default".into(), "alex".into()],
            active_profile: "alex".into(),
            storage_size_limit: StorageSize(500 * 1024 * 1024),
            predownload_unread_chapters: 5,
            auto_check_updates: false,
            reader_fit: "fit-width".into(),
            reader_rotation: 90,
            reader_rotation_locked: false,
            color_post_process: "standard".into(),
            reader_full_refresh_interval: 12,
            wifi_auto_connect: false,
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
    fn profiles_parse_leniently() {
        let load = |json: &str| {
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(Settings::path(dir.path()), json).unwrap();
            Settings::load(dir.path()).unwrap()
        };
        // Valid lists pass through; non-string entries are dropped.
        assert_eq!(
            load(r#"{"profiles": ["default", "alex"]}"#).profiles,
            vec!["default", "alex"]
        );
        assert_eq!(
            load(r#"{"profiles": ["default", 42, null, " bo "]}"#).profiles,
            vec!["default", "bo"]
        );
        // Empty lists and wrong types fall back to just "default".
        assert_eq!(load(r#"{"profiles": []}"#).profiles, vec!["default"]);
        assert_eq!(load(r#"{"profiles": "alex"}"#).profiles, vec!["default"]);
        // Active profile: non-empty strings pass through, the rest means
        // "default".
        assert_eq!(load(r#"{"active_profile": "alex"}"#).active_profile, "alex");
        assert_eq!(load(r#"{"active_profile": ""}"#).active_profile, "default");
        assert_eq!(load(r#"{"active_profile": 7}"#).active_profile, "default");
        assert_eq!(
            load(r#"{"active_profile": null}"#).active_profile,
            "default"
        );
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
    fn rotation_lock_parses_leniently() {
        let load = |json: &str| {
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(Settings::path(dir.path()), json).unwrap();
            Settings::load(dir.path()).unwrap()
        };
        assert!(!load(r#"{"reader_rotation_locked": false}"#).reader_rotation_locked);
        assert!(load(r#"{"reader_rotation_locked": true}"#).reader_rotation_locked);
        // Wrong types and missing values never error — they mean locked.
        assert!(load(r#"{"reader_rotation_locked": "no"}"#).reader_rotation_locked);
        assert!(load(r#"{"reader_rotation_locked": 0}"#).reader_rotation_locked);
        assert!(load(r#"{"reader_rotation_locked": null}"#).reader_rotation_locked);
        assert!(load(r#"{}"#).reader_rotation_locked);
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
