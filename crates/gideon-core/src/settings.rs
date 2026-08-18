//! User settings, persisted as `settings.json` in the data directory.
//!
//! Mirrors the shape of bobo's settings where it makes sense (source lists,
//! languages, storage size limit) and follows the lessons learned there:
//! parsing is lenient — unknown fields are ignored, missing fields get
//! defaults, and a malformed file produces a clear error instead of a crash.
//!
//! # Two scopes
//!
//! [`Settings`] is the *device* file (`$HOME/.config/gideon/settings.json`).
//! [`ProfileSettings`] is the *reader* file, one per profile, living in that
//! profile's own library directory. A Kobo shared by two people has one
//! frontlight, one radio and one disk, but two sets of eyes: the split follows
//! that line exactly, and [`Settings::with_profile`] merges them back into the
//! single `Settings` value the rest of gideon already consumes, so no call site
//! has to know the difference.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::Result;

/// Default storage limit for downloaded chapters: 2 GB, same as bobo.
pub const DEFAULT_STORAGE_LIMIT_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// How long a finished chapter is kept before automatic cleanup removes its
/// CBZ, in hours. Two days: long enough that "I'll flip back to check that
/// panel" still works the next evening, short enough that a binge doesn't
/// silt up the device.
pub const DEFAULT_FINISHED_CLEANUP_HOURS: u32 = 48;

/// The values the settings screen cycles `finished_cleanup_hours` through:
/// never / 1 day / 2 days / 3 days / 1 week. Deliberately coarse — this
/// setting decides when user data is deleted, and a spinner that can land on
/// "3 hours" invites a mis-set that eats a chapter someone was still using.
pub const FINISHED_CLEANUP_STEPS: [u32; 5] = [0, 24, 48, 72, 168];

/// The whole settings surface, as every existing call site sees it.
///
/// Physically this struct is the union of two files. The fields listed below
/// are the *device-global* half — they stay in `settings.json` under
/// `$HOME/.config/gideon` and are shared by everyone using the Kobo, because
/// each one describes a piece of hardware or of the install rather than a
/// person's taste:
///
/// - `profiles`, `active_profile` — the roster itself, and which reader is at
///   the device right now. It cannot live inside a profile: it is what picks
///   the profile.
/// - `source_lists` — sources are *installed software*, fetched and stored
///   once for the device; two readers browsing the same catalogue is not a
///   conflict.
/// - `languages` — a filter over that same shared catalogue, kept with it.
/// - `storage_size_limit` — one disk, one budget. Per-profile budgets over a
///   single filesystem would let one reader's quota evict another's chapters.
/// - `auto_check_updates` — updates replace the binary for everyone.
/// - `color_post_process` — a panel-calibration knob for the Kaleido filter
///   (unlike `color_profile`, which is a palette *preference*).
/// - `wifi_auto_connect` — one radio, and a policy about touching it.
/// - `idle_suspend_minutes` — one power manager for the whole device.
/// - `frontlight_brightness`, `frontlight_warmth` — one lamp, and it is
///   whatever the last hand to touch the slider left it at.
///
/// The other nine fields are per-reader; see [`ProfileSettings`].
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

    /// Which colour profile the UI draws in: "ink-rust" (the default),
    /// "indigo", "sumi", "botanical", or "mono".
    ///
    /// This is a palette choice, not a capability switch — "mono" is a real
    /// target (a panel with no colour filter), not a degraded fallback, and
    /// every profile's ramps are monotonic in luma so the UI stays legible
    /// either way. Parsed leniently: an unknown value means the default.
    #[serde(default = "default_color_profile")]
    #[serde(deserialize_with = "lenient_color_profile")]
    pub color_profile: String,

    /// How the Library draws itself: "shelf" (the cover grid, the default)
    /// or "list" (a dense row per series carrying its MyAnimeList metadata,
    /// download state and progress).
    ///
    /// Two honest views of the same library rather than a replacement: the
    /// shelf is better for browsing by art, the list for deciding what to
    /// read next. Parsed leniently — an unknown value means the default.
    #[serde(default = "default_library_view")]
    #[serde(deserialize_with = "lenient_library_view")]
    pub library_view: String,

    /// What Today draws above the Continue card: "heatmap" (the activity
    /// grid, months at a glance) or "calendar" (this month, with a bar per
    /// series continuing across the days it was read).
    ///
    /// Two views of the same reading history, answering different questions
    /// — "how much, lately" against "what was I on, and for how many days
    /// running" — so it is a taste, and taste is per profile. Parsed
    /// leniently: an unknown value means the default.
    #[serde(default = "default_stats_view")]
    #[serde(deserialize_with = "lenient_stats_view")]
    pub stats_view: String,

    /// Page turns between full (flashing) e-ink refreshes. Higher flashes
    /// less often (smoother reading) but lets ghosting build up longer.
    /// Parsed leniently — out-of-range or wrong-typed values fall back to the
    /// default (8); clamped to 4–24.
    #[serde(deserialize_with = "lenient_full_refresh_interval")]
    pub reader_full_refresh_interval: u32,

    /// Auto-rotate a horizontal double-page spread (a page wider than it is
    /// tall) by 270° so it fills the screen, while the device orientation
    /// stays locked. Parsed leniently — non-bool means the default (off).
    #[serde(deserialize_with = "lenient_bool_false")]
    pub auto_rotate_spreads: bool,

    /// Whether gideon may bring Wi-Fi up on its own (before a network action
    /// and on wake). Off = never auto-connect; the user connects manually from
    /// the Wi-Fi controls. Parsed leniently — non-bool means the default
    /// (true). (`GIDEON_WIFI_AUTOENABLE=0` is a separate hard override.)
    #[serde(deserialize_with = "lenient_bool_true")]
    pub wifi_auto_connect: bool,

    /// Minutes of inactivity before the device suspends on its own, as if
    /// the sleep cover closed. 0 disables the idle suspend ("never").
    /// Parsed leniently — wrong-typed values fall back to the default (15,
    /// the same timeout Nickel and KOReader default to).
    #[serde(deserialize_with = "lenient_idle_suspend")]
    pub idle_suspend_minutes: u32,

    /// Frontlight brightness percent (0–100), restored at startup and
    /// updated from the reader's right-edge slide. Parsed leniently.
    #[serde(deserialize_with = "lenient_percent")]
    pub frontlight_brightness: u32,

    /// Frontlight warmth ("night light") percent (0–100), restored at
    /// startup and updated from the reader's left-edge slide.
    #[serde(deserialize_with = "lenient_percent")]
    pub frontlight_warmth: u32,

    /// How many hours after finishing a chapter its CBZ may be deleted
    /// automatically. 0 means never — the cleanup pass is skipped entirely.
    ///
    /// This is the only setting whose value deletes user files on its own, so
    /// it parses defensively rather than merely leniently: a wrong-typed,
    /// negative or absurd value falls back to the default (48) instead of
    /// being coerced into something small. A settings file that got mangled
    /// must never turn into "clean up everything read in the last hour".
    #[serde(default = "default_finished_cleanup_hours")]
    #[serde(deserialize_with = "lenient_finished_cleanup_hours")]
    pub finished_cleanup_hours: u32,
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
            color_profile: default_color_profile(),
            library_view: default_library_view(),
            stats_view: default_stats_view(),
            reader_full_refresh_interval: 8,
            auto_rotate_spreads: false,
            wifi_auto_connect: true,
            idle_suspend_minutes: 15,
            frontlight_brightness: 20,
            frontlight_warmth: 0,
            finished_cleanup_hours: default_finished_cleanup_hours(),
        }
    }
}

/// The cleanup delay a fresh install uses.
fn default_finished_cleanup_hours() -> u32 {
    DEFAULT_FINISHED_CLEANUP_HOURS
}

/// Lenient `finished_cleanup_hours` parsing: a whole number of hours passes
/// through (0 = never); anything else — a float, a negative, a string, a
/// value too large for `u32` — means the default.
///
/// Note what this deliberately does *not* do: it never clamps a hostile value
/// down into range. Clamping a bogus 4 000 000 000 to "1 hour" would make a
/// corrupt file far more destructive than an unreadable one, and the whole
/// point of a cleanup delay is that the user gets the grace period they chose.
fn lenient_finished_cleanup_hours<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<u32, D::Error> {
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(value
        .as_u64()
        .and_then(|v| u32::try_from(v).ok())
        .unwrap_or(DEFAULT_FINISHED_CLEANUP_HOURS))
}

/// Lenient `idle_suspend_minutes` parsing: any non-negative JSON number
/// passes through (0 = never); anything else means the default (15).
fn lenient_idle_suspend<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<u32, D::Error> {
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(value
        .as_u64()
        .and_then(|v| u32::try_from(v).ok())
        .unwrap_or(15))
}

/// Lenient bool defaulting to `false`: a JSON bool passes through; anything
/// else (wrong type, missing) means `false`.
fn lenient_bool_false<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<bool, D::Error> {
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(value.as_bool().unwrap_or(false))
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

/// The colour profile a fresh install draws in.
fn default_color_profile() -> String {
    "ink-rust".to_string()
}

/// The Library view a fresh install uses.
///
/// The dense list, not the cover shelf. The list is the view that answers
/// the question you actually have in front of a library — what is on the
/// device, how much of it is unread, what is next — and hiding it behind a
/// title-bar tap meant nobody would ever find it. The shelf is still one tap
/// away for browsing by art, and the choice is per profile and sticky.
fn default_library_view() -> String {
    "list".to_string()
}

fn default_stats_view() -> String {
    "heatmap".to_string()
}

/// Lenient `stats_view` parsing: "heatmap" and "calendar" pass through,
/// anything else falls back to the default. Both are named explicitly so
/// neither depends on which one the default happens to be — the bug the
/// library view shipped with.
fn lenient_stats_view<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<String, D::Error> {
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(
        match value.as_str().map(|s| s.trim().to_ascii_lowercase()) {
            Some(s) if s == "heatmap" || s == "calendar" => s,
            _ => default_stats_view(),
        },
    )
}

/// Lenient `library_view` parsing: "list" and "shelf" pass through, anything
/// else falls back to the default.
///
/// This used to read "anything that isn't `list` means the shelf", which was
/// right while the shelf WAS the default. Once the dense list became the
/// default the same branch quietly inverted: `"library_view": "shelf"` fell
/// through to the fallback and came back as `"list"`, so choosing the cover
/// shelf survived until the next read of the file — which is every repaint.
/// Both known values are named explicitly now, so neither depends on which
/// one the default happens to be.
fn lenient_library_view<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<String, D::Error> {
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(
        match value.as_str().map(|s| s.trim().to_ascii_lowercase()) {
            Some(s) if s == "list" || s == "shelf" => s,
            _ => default_library_view(),
        },
    )
}

/// Lenient `color_profile` parsing: known profiles pass through lowercased,
/// anything else means the default. A settings file written by a newer
/// gideon naming a profile this build doesn't have must not be fatal.
fn lenient_color_profile<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<String, D::Error> {
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(
        match value.as_str().map(|s| s.trim().to_ascii_lowercase()) {
            Some(s) if s == "indigo" || s == "sumi" || s == "botanical" || s == "mono" => s,
            _ => default_color_profile(),
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

    /// Overlay a profile's personal settings onto the device-global ones.
    ///
    /// Only the fields the profile actually states are replaced; everything
    /// else — the radio, the lamp, the disk, the profile roster — passes
    /// through untouched. This is the single point where the two files become
    /// the one `Settings` value the rest of gideon consumes.
    #[must_use]
    pub fn with_profile(mut self, p: &ProfileSettings) -> Settings {
        if let Some(v) = p.reader_fit.clone() {
            self.reader_fit = v;
        }
        if let Some(v) = p.reader_rotation {
            self.reader_rotation = v;
        }
        if let Some(v) = p.reader_rotation_locked {
            self.reader_rotation_locked = v;
        }
        if let Some(v) = p.auto_rotate_spreads {
            self.auto_rotate_spreads = v;
        }
        if let Some(v) = p.reader_full_refresh_interval {
            self.reader_full_refresh_interval = v;
        }
        if let Some(v) = p.color_profile.clone() {
            self.color_profile = v;
        }
        if let Some(v) = p.library_view.clone() {
            self.library_view = v;
        }
        if let Some(v) = p.stats_view.clone() {
            self.stats_view = v;
        }
        if let Some(v) = p.predownload_unread_chapters {
            self.predownload_unread_chapters = v;
        }
        if let Some(v) = p.finished_cleanup_hours {
            self.finished_cleanup_hours = v;
        }
        self
    }
}

/// The per-profile subset of [`Settings`], stored in the profile's own library
/// directory at `<profile_library_dir>/.gideon/settings.json` — the same
/// hidden directory that already holds `progress.json` and `series.json`, so a
/// reader's preferences travel with their books (including through
/// [`crate::profile::convert_default`], which moves that whole directory).
///
/// Every field here is personal taste or a reading habit, not a property of
/// the device:
///
/// - `reader_fit` — whether a page is shown whole or filled to the width. A
///   matter of eyesight and of how close you hold the thing.
/// - `reader_rotation`, `reader_rotation_locked` — one reader reads in
///   landscape on the sofa, the other portrait in bed; neither wants the
///   other's orientation restored under them.
/// - `auto_rotate_spreads` — the same argument for double-page spreads.
/// - `reader_full_refresh_interval` — a personal trade between ghosting and
///   flashing; some readers are far more bothered by one than the other.
/// - `color_profile` — a palette preference, purely aesthetic. (Contrast
///   `color_post_process`, which calibrates the panel and stays global.)
/// - `library_view` — shelf or list is a browsing habit, and it is *your*
///   library being drawn.
/// - `predownload_unread_chapters` — how far ahead to fetch follows how you
///   read (binge vs. a chapter a night), and the pre-fetch is done against
///   your own series.
/// - `finished_cleanup_hours` — how long *your* finished chapters are kept
///   before deletion. Letting one reader's tidiness delete another's
///   just-finished chapter would be the worst kind of shared-device surprise.
///
/// # Every field is optional, and that is the migration
///
/// Each field is an `Option`: `None` means "this profile has said nothing,
/// use the device value". That choice is what makes the upgrade path safe.
/// Every device already in the field has all nine values in the device-global
/// file and no per-profile file at all; [`ProfileSettings::load`] on such a
/// device yields `ProfileSettings::default()` — all `None` — and
/// [`Settings::with_profile`] then changes nothing whatsoever, so the merged
/// result is byte-identical to what the user had before the upgrade. Their
/// reader fit and rotation cannot be lost, because nothing ever overwrites
/// them until the profile explicitly saves a value.
///
/// The alternative — seeding the per-profile file from the device file on
/// first save — was rejected: it needs a write to happen at exactly the right
/// moment, it makes "not set" and "set to the default" indistinguishable, and
/// any device that failed or skipped that one write would silently fall back
/// to defaults instead of the user's real values. A `None` overlay has no such
/// moment to get wrong: it degrades to today's behaviour by construction.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ProfileSettings {
    /// See [`Settings::reader_fit`]. `None` = use the device value.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(deserialize_with = "opt_reader_fit")]
    pub reader_fit: Option<String>,

    /// See [`Settings::reader_rotation`]. `None` = use the device value.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(deserialize_with = "opt_reader_rotation")]
    pub reader_rotation: Option<u32>,

    /// See [`Settings::reader_rotation_locked`]. `None` = use the device value.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(deserialize_with = "opt_bool")]
    pub reader_rotation_locked: Option<bool>,

    /// See [`Settings::auto_rotate_spreads`]. `None` = use the device value.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(deserialize_with = "opt_bool")]
    pub auto_rotate_spreads: Option<bool>,

    /// See [`Settings::reader_full_refresh_interval`]. `None` = use the device
    /// value; out-of-range (not 4–24) also means `None` rather than a clamp.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(deserialize_with = "opt_full_refresh_interval")]
    pub reader_full_refresh_interval: Option<u32>,

    /// See [`Settings::color_profile`]. `None` = use the device value.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(deserialize_with = "opt_color_profile")]
    pub color_profile: Option<String>,

    /// See [`Settings::library_view`]. `None` = use the device value.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(deserialize_with = "opt_library_view")]
    pub library_view: Option<String>,

    /// See [`Settings::stats_view`]. `None` = use the device value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[serde(deserialize_with = "opt_stats_view")]
    pub stats_view: Option<String>,

    /// See [`Settings::predownload_unread_chapters`]. `None` = use the device
    /// value.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(deserialize_with = "opt_u32")]
    pub predownload_unread_chapters: Option<u32>,

    /// See [`Settings::finished_cleanup_hours`]. `None` = use the device
    /// value.
    ///
    /// Parsed as defensively here as it is there: this is the one setting that
    /// deletes user files, so a mangled value falls back to "say nothing" —
    /// which means the device value, which means the delay the user chose —
    /// and never to something shorter.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(deserialize_with = "opt_u32")]
    pub finished_cleanup_hours: Option<u32>,
}

impl ProfileSettings {
    /// Load a profile's settings from
    /// `<profile_library_dir>/.gideon/settings.json`.
    ///
    /// Never an error. A missing file — the case on every device upgrading
    /// into this layout — gives all-`None` defaults, and so does an
    /// unparseable one; a file that parses but holds junk in some fields gives
    /// `None` for exactly those fields. In all three cases the affected
    /// settings fall back to the device-global value, which is the behaviour
    /// gideon had before profiles were split out at all.
    pub fn load(profile_library_dir: &Path) -> Self {
        match fs::read_to_string(Self::path(profile_library_dir)) {
            Ok(contents) => serde_json::from_str(&contents).unwrap_or_default(),
            Err(_) => Self::default(),
        }
    }

    /// Persist atomically (temp file + rename), creating the `.gideon`
    /// directory if the profile doesn't have one yet.
    pub fn save(&self, profile_library_dir: &Path) -> Result<()> {
        let path = Self::path(profile_library_dir);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, serde_json::to_string_pretty(self)?)?;
        fs::rename(&tmp, path)?;
        Ok(())
    }

    /// Extract the per-profile half of a merged [`Settings`], stating every
    /// field explicitly.
    ///
    /// This is what the UI calls when a reader changes one of their own
    /// settings: the merged value it was editing becomes a fully-populated
    /// `ProfileSettings` to save. Note that it deliberately produces all
    /// `Some` — once a profile has saved, it owns these nine values outright
    /// and no longer drifts with the device file.
    pub fn from_settings(s: &Settings) -> Self {
        Self {
            reader_fit: Some(s.reader_fit.clone()),
            reader_rotation: Some(s.reader_rotation),
            reader_rotation_locked: Some(s.reader_rotation_locked),
            auto_rotate_spreads: Some(s.auto_rotate_spreads),
            reader_full_refresh_interval: Some(s.reader_full_refresh_interval),
            color_profile: Some(s.color_profile.clone()),
            library_view: Some(s.library_view.clone()),
            stats_view: Some(s.stats_view.clone()),
            predownload_unread_chapters: Some(s.predownload_unread_chapters),
            finished_cleanup_hours: Some(s.finished_cleanup_hours),
        }
    }

    pub fn path(profile_library_dir: &Path) -> PathBuf {
        profile_library_dir.join(".gideon").join("settings.json")
    }
}

/// Lenient optional `reader_fit`: a string passes through normalized (unknown
/// values included — the reader treats them as "contain", exactly as with the
/// device field), anything else means "unset".
fn opt_reader_fit<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Option<String>, D::Error> {
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(value.as_str().map(|s| s.trim().to_ascii_lowercase()))
}

/// Lenient optional `reader_rotation`: only 0/90/180/270 count as stated.
fn opt_reader_rotation<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Option<u32>, D::Error> {
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(match value.as_u64() {
        Some(degrees @ (0 | 90 | 180 | 270)) => Some(degrees as u32),
        _ => None,
    })
}

/// Lenient optional bool: only a JSON bool counts as stated.
fn opt_bool<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Option<bool>, D::Error> {
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(value.as_bool())
}

/// Lenient optional non-negative integer: only a whole number that fits in
/// `u32` counts as stated. Never clamps — a hostile value means "unset", so
/// the device value stands.
fn opt_u32<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Option<u32>, D::Error> {
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(value.as_u64().and_then(|v| u32::try_from(v).ok()))
}

/// Lenient optional `reader_full_refresh_interval`: 4–24 counts as stated,
/// anything else means "unset".
fn opt_full_refresh_interval<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Option<u32>, D::Error> {
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(match value.as_u64() {
        Some(n) if (4..=24).contains(&n) => Some(n as u32),
        _ => None,
    })
}

/// Lenient optional `color_profile`: a known palette counts as stated;
/// anything else (including a palette from a newer build) means "unset".
fn opt_color_profile<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Option<String>, D::Error> {
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(
        match value.as_str().map(|s| s.trim().to_ascii_lowercase()) {
            Some(s)
                if s == "ink-rust"
                    || s == "indigo"
                    || s == "sumi"
                    || s == "botanical"
                    || s == "mono" =>
            {
                Some(s)
            }
            _ => None,
        },
    )
}

/// Lenient optional `library_view`: "shelf" or "list" count as stated.
fn opt_library_view<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Option<String>, D::Error> {
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(
        match value.as_str().map(|s| s.trim().to_ascii_lowercase()) {
            Some(s) if s == "shelf" || s == "list" => Some(s),
            _ => None,
        },
    )
}

/// Lenient optional `stats_view`: "heatmap" or "calendar" count as stated,
/// anything else as "not stated" so the device value shows through.
fn opt_stats_view<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Option<String>, D::Error> {
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(
        match value.as_str().map(|s| s.trim().to_ascii_lowercase()) {
            Some(s) if s == "heatmap" || s == "calendar" => Some(s),
            _ => None,
        },
    )
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
    } else {
        let n = cleaned.strip_suffix("MB")?;
        (n, 1024u64 * 1024)
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
        assert!(!s.auto_rotate_spreads);
        assert_eq!(s.idle_suspend_minutes, 15);
        assert_eq!(s.finished_cleanup_hours, 48);
    }

    #[test]
    fn finished_cleanup_hours_parses_defensively() {
        let load = |json: &str| {
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(Settings::path(dir.path()), json).unwrap();
            Settings::load(dir.path()).unwrap().finished_cleanup_hours
        };
        assert_eq!(load(r#"{"finished_cleanup_hours": 24}"#), 24);
        assert_eq!(load(r#"{"finished_cleanup_hours": 168}"#), 168);
        assert_eq!(load(r#"{"finished_cleanup_hours": 0}"#), 0, "0 = never");
        // Junk falls back to the default — never to a *shorter* delay, which
        // would delete more than the user ever asked for.
        assert_eq!(load(r#"{"finished_cleanup_hours": -5}"#), 48);
        assert_eq!(load(r#"{"finished_cleanup_hours": "two days"}"#), 48);
        assert_eq!(load(r#"{"finished_cleanup_hours": 1.5}"#), 48);
        assert_eq!(load(r#"{"finished_cleanup_hours": null}"#), 48);
        assert_eq!(load(r#"{"finished_cleanup_hours": 99999999999999}"#), 48);
        assert_eq!(load(r#"{}"#), 48, "an older settings file gets the default");
    }

    #[test]
    fn finished_cleanup_steps_are_usable_as_a_cycle() {
        // The UI cycles through these; they must contain the default (so the
        // current value is always on the wheel) and start at "never".
        assert_eq!(FINISHED_CLEANUP_STEPS[0], 0);
        assert!(FINISHED_CLEANUP_STEPS.contains(&DEFAULT_FINISHED_CLEANUP_HOURS));
        assert!(
            FINISHED_CLEANUP_STEPS.windows(2).all(|w| w[0] < w[1]),
            "steps must be strictly increasing"
        );
    }

    #[test]
    fn idle_suspend_minutes_parses_leniently() {
        let load = |json: &str| {
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(Settings::path(dir.path()), json).unwrap();
            Settings::load(dir.path()).unwrap().idle_suspend_minutes
        };
        assert_eq!(load(r#"{"idle_suspend_minutes": 5}"#), 5);
        assert_eq!(load(r#"{"idle_suspend_minutes": 0}"#), 0, "0 = never");
        // Wrong-typed / negative / missing default to 15.
        assert_eq!(load(r#"{"idle_suspend_minutes": "soon"}"#), 15);
        assert_eq!(load(r#"{"idle_suspend_minutes": -3}"#), 15);
        assert_eq!(load(r#"{}"#), 15);
    }

    #[test]
    fn auto_rotate_spreads_parses_leniently() {
        let load = |json: &str| {
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(Settings::path(dir.path()), json).unwrap();
            Settings::load(dir.path()).unwrap().auto_rotate_spreads
        };
        assert!(load(r#"{"auto_rotate_spreads": true}"#));
        assert!(!load(r#"{"auto_rotate_spreads": false}"#));
        // Wrong-typed / missing default to false.
        assert!(!load(r#"{"auto_rotate_spreads": "yes"}"#));
        assert!(!load(r#"{}"#));
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
            color_profile: "indigo".into(),
            library_view: "list".into(),
            stats_view: "calendar".into(),
            reader_rotation: 90,
            reader_rotation_locked: false,
            color_post_process: "standard".into(),
            reader_full_refresh_interval: 12,
            auto_rotate_spreads: true,
            wifi_auto_connect: false,
            idle_suspend_minutes: 5,
            frontlight_brightness: 65,
            frontlight_warmth: 40,
            finished_cleanup_hours: 168,
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

    /// A `Settings` with every field moved off its default, standing in for
    /// "what a real device has in settings.json before the upgrade".
    fn populated_settings() -> Settings {
        Settings {
            source_lists: vec!["https://example.com/index.json".into()],
            languages: vec!["en".into()],
            profiles: vec!["default".into(), "alex".into()],
            active_profile: "alex".into(),
            storage_size_limit: StorageSize(500 * 1024 * 1024),
            predownload_unread_chapters: 5,
            auto_check_updates: false,
            reader_fit: "fit-width".into(),
            reader_rotation: 270,
            reader_rotation_locked: false,
            color_post_process: "standard".into(),
            color_profile: "sumi".into(),
            library_view: "list".into(),
            stats_view: "calendar".into(),
            reader_full_refresh_interval: 20,
            auto_rotate_spreads: true,
            wifi_auto_connect: false,
            idle_suspend_minutes: 5,
            frontlight_brightness: 65,
            frontlight_warmth: 40,
            finished_cleanup_hours: 168,
        }
    }

    #[test]
    fn profile_settings_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let p = ProfileSettings::from_settings(&populated_settings());
        p.save(dir.path()).unwrap();
        // It lands next to progress.json / series.json, so it travels with
        // the profile through convert_default.
        assert!(dir.path().join(".gideon/settings.json").is_file());
        assert_eq!(ProfileSettings::load(dir.path()), p);
    }

    #[test]
    fn upgrading_a_device_with_no_per_profile_file_changes_nothing() {
        // The migration guarantee. A device in the field has every value in
        // the device-global file and no per-profile file at all; the merged
        // result must be indistinguishable from today's behaviour.
        let library = tempfile::tempdir().unwrap();
        assert!(!ProfileSettings::path(library.path()).exists());

        let device = populated_settings();
        let merged = device
            .clone()
            .with_profile(&ProfileSettings::load(library.path()));
        assert_eq!(merged, device, "an upgrade must not lose a single setting");

        // Spelled out for the two the user would notice first.
        assert_eq!(merged.reader_fit, "fit-width");
        assert_eq!(merged.reader_rotation, 270);

        // And the same holds for a fresh install.
        let fresh = Settings::default();
        assert_eq!(
            fresh
                .clone()
                .with_profile(&ProfileSettings::load(library.path())),
            fresh
        );
    }

    #[test]
    fn a_corrupt_per_profile_file_falls_back_instead_of_erroring() {
        let device = populated_settings();

        // Unparseable JSON: nothing is stated, so the device values stand.
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".gideon")).unwrap();
        std::fs::write(ProfileSettings::path(dir.path()), "{not json").unwrap();
        assert_eq!(
            ProfileSettings::load(dir.path()),
            ProfileSettings::default()
        );
        assert_eq!(
            device
                .clone()
                .with_profile(&ProfileSettings::load(dir.path())),
            device
        );

        // Parseable but junk per field: those fields fall back one by one,
        // the good ones still apply.
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".gideon")).unwrap();
        std::fs::write(
            ProfileSettings::path(dir.path()),
            r#"{
                "reader_fit": 42,
                "reader_rotation": 45,
                "reader_rotation_locked": "no",
                "auto_rotate_spreads": null,
                "reader_full_refresh_interval": 99,
                "color_profile": "chartreuse",
                "library_view": "gallery",
                "predownload_unread_chapters": -1,
                "finished_cleanup_hours": 99999999999999,
                "future_field": {"nested": true}
            }"#,
        )
        .unwrap();
        let p = ProfileSettings::load(dir.path());
        assert_eq!(
            p,
            ProfileSettings::default(),
            "every junk field means unset"
        );
        assert_eq!(device.clone().with_profile(&p), device);

        // A mangled cleanup delay must never shorten: it stays the device's.
        assert_eq!(device.with_profile(&p).finished_cleanup_hours, 168);
    }

    #[test]
    fn two_profiles_hold_different_values_independently() {
        let alex = tempfile::tempdir().unwrap();
        let bo = tempfile::tempdir().unwrap();
        let device = Settings::default();

        ProfileSettings {
            reader_fit: Some("fit-width".into()),
            reader_rotation: Some(90),
            color_profile: Some("indigo".into()),
            library_view: Some("list".into()),
            ..Default::default()
        }
        .save(alex.path())
        .unwrap();

        ProfileSettings {
            reader_fit: Some("contain".into()),
            reader_rotation: Some(180),
            color_profile: Some("mono".into()),
            ..Default::default()
        }
        .save(bo.path())
        .unwrap();

        let a = device
            .clone()
            .with_profile(&ProfileSettings::load(alex.path()));
        let b = device
            .clone()
            .with_profile(&ProfileSettings::load(bo.path()));

        assert_eq!(a.reader_fit, "fit-width");
        assert_eq!(b.reader_fit, "contain");
        assert_eq!(a.reader_rotation, 90);
        assert_eq!(b.reader_rotation, 180);
        assert_eq!(a.color_profile, "indigo");
        assert_eq!(b.color_profile, "mono");
        // Bo said nothing about the library view, so the device value stands.
        assert_eq!(a.library_view, "list");
        assert_eq!(b.library_view, device.library_view);
    }

    #[test]
    fn the_overlay_never_touches_device_global_fields() {
        let device = populated_settings();
        // A profile that states everything it possibly can.
        let p = ProfileSettings::from_settings(&Settings::default());
        let merged = device.clone().with_profile(&p);

        assert_eq!(merged.source_lists, device.source_lists);
        assert_eq!(merged.languages, device.languages);
        assert_eq!(merged.profiles, device.profiles);
        assert_eq!(merged.active_profile, device.active_profile);
        assert_eq!(merged.storage_size_limit, device.storage_size_limit);
        assert_eq!(merged.auto_check_updates, device.auto_check_updates);
        assert_eq!(merged.color_post_process, device.color_post_process);
        assert_eq!(merged.wifi_auto_connect, device.wifi_auto_connect);
        assert_eq!(merged.idle_suspend_minutes, device.idle_suspend_minutes);
        assert_eq!(merged.frontlight_brightness, device.frontlight_brightness);
        assert_eq!(merged.frontlight_warmth, device.frontlight_warmth);
        // ...while the personal half did change.
        assert_ne!(merged.reader_fit, device.reader_fit);
    }

    #[test]
    fn from_settings_then_with_profile_round_trips() {
        let personal = populated_settings();
        let p = ProfileSettings::from_settings(&personal);
        // Applied over a completely different device file, the nine personal
        // values come back exactly.
        let merged = Settings::default().with_profile(&p);
        assert_eq!(merged.reader_fit, personal.reader_fit);
        assert_eq!(merged.reader_rotation, personal.reader_rotation);
        assert_eq!(
            merged.reader_rotation_locked,
            personal.reader_rotation_locked
        );
        assert_eq!(merged.auto_rotate_spreads, personal.auto_rotate_spreads);
        assert_eq!(
            merged.reader_full_refresh_interval,
            personal.reader_full_refresh_interval
        );
        assert_eq!(merged.color_profile, personal.color_profile);
        assert_eq!(merged.library_view, personal.library_view);
        assert_eq!(
            merged.predownload_unread_chapters,
            personal.predownload_unread_chapters
        );
        assert_eq!(
            merged.finished_cleanup_hours,
            personal.finished_cleanup_hours
        );
        // And it survives a save/load in between.
        let dir = tempfile::tempdir().unwrap();
        p.save(dir.path()).unwrap();
        assert_eq!(
            Settings::default().with_profile(&ProfileSettings::load(dir.path())),
            merged
        );
    }

    #[test]
    fn a_profile_may_state_the_default_value_and_it_sticks() {
        // "Unset" and "set to the default" must be distinguishable: alex
        // explicitly wants rotation 0 even though the device is on 270.
        let dir = tempfile::tempdir().unwrap();
        ProfileSettings {
            reader_rotation: Some(0),
            reader_rotation_locked: Some(true),
            ..Default::default()
        }
        .save(dir.path())
        .unwrap();
        let merged = populated_settings().with_profile(&ProfileSettings::load(dir.path()));
        assert_eq!(merged.reader_rotation, 0);
        assert!(merged.reader_rotation_locked);
    }

    #[test]
    fn unset_fields_are_omitted_from_the_file() {
        let dir = tempfile::tempdir().unwrap();
        ProfileSettings {
            reader_fit: Some("fit-width".into()),
            ..Default::default()
        }
        .save(dir.path())
        .unwrap();
        let raw = std::fs::read_to_string(ProfileSettings::path(dir.path())).unwrap();
        assert!(raw.contains("reader_fit"));
        assert!(
            !raw.contains("color_profile"),
            "unset fields must not be written: {raw}"
        );
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
    #[test]
    fn the_cover_shelf_survives_a_round_trip_through_settings_json() {
        // The lenient parser said "anything that isn't list means the shelf",
        // which inverted the moment the list became the default: "shelf"
        // fell through to the fallback and read back as "list", so choosing
        // the shelf lasted until the next read of the file.
        let dir = tempfile::tempdir().unwrap();
        Settings {
            library_view: "shelf".into(),
            ..Settings::default()
        }
        .save(dir.path())
        .unwrap();
        assert_eq!(Settings::load(dir.path()).unwrap().library_view, "shelf");

        // And a value from neither vocabulary still lands on the default
        // rather than erroring.
        let path = Settings::path(dir.path());
        let raw = std::fs::read_to_string(&path).unwrap();
        std::fs::write(&path, raw.replace("\"shelf\"", "\"grid\"")).unwrap();
        assert_eq!(Settings::load(dir.path()).unwrap().library_view, "list");
    }
}
