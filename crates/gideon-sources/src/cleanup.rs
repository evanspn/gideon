//! Automatic cleanup of chapters the user has finished reading.
//!
//! This is a *separate, earlier* pass from [`crate::storage`]'s
//! `enforce_limit`. Eviction is a last resort: the disk is full, so the
//! least-recently-used chapter has to go, read or not. This pass is the
//! opposite — it runs when there is plenty of room, and removes only chapters
//! the user demonstrably has no further use for: finished, and finished long
//! enough ago (`settings.finished_cleanup_hours`, default 48) that they aren't
//! going to flip back through them. The point is that a device that gets read
//! on never silts up, without the user ever thinking about storage.
//!
//! Everything here deletes user data, so the design is built around refusals
//! rather than around what it removes. In order:
//!
//! * **Only finished chapters.** No progress record at all means never opened;
//!   a partial record means mid-read. Both are kept.
//! * **Only stale ones.** `last_read_at` must be older than the cutoff. A
//!   `last_read_at` of 0 means "timestamp unknown" (a progress row that
//!   arrived from sync without one), and unknown is treated as *recent*, never
//!   as "epoch, therefore ancient".
//! * **Never the chapter the user is on.** `ProgressStore::last_opened` is
//!   written the instant the reader opens a chapter, so it is the truth about
//!   where the user is even when they finished that chapter minutes ago.
//! * **Never the last chapter of a series.** A series with nothing on disk
//!   disappears from the library shelf; from the user's point of view gideon
//!   deleted their book. A few reclaimed MB is not worth that, so one chapter
//!   per series always survives.
//! * **Never reading progress.** Only the CBZ goes. If the progress row went
//!   with it, the chapter would come back as unread, get re-downloaded by the
//!   pre-download queue, get "finished" again... forever. Deleting the file is
//!   reversible (re-download); losing the record that a series was read is not.
//!
//! [`plan_finished_cleanup`] answers "what would go?" without touching the
//! disk — that is what the settings screen shows as "12 chapters · 148 MB
//! ready to clean up" — and [`run_finished_cleanup`] performs exactly that
//! plan. Both are safe to call repeatedly and safe when nothing qualifies.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use gideon_core::library::ProgressStore;
use gideon_core::series::SeriesIndex;

use crate::Result;

/// One chapter file selected for cleanup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CleanupCandidate {
    /// Series directory name, relative to the library root.
    pub series_dir: String,
    /// Source-side chapter id — the key in `SeriesRef::downloaded`.
    pub chapter_id: String,
    /// CBZ file name inside the series directory.
    pub file_name: String,
    /// Absolute path of the CBZ.
    pub path: PathBuf,
    /// Size on disk when the plan was made.
    pub bytes: u64,
}

impl CleanupCandidate {
    /// The progress key for this chapter: its library-relative path, which is
    /// what [`ProgressStore`] is keyed by.
    pub fn progress_key(&self) -> String {
        format!("{}/{}", self.series_dir, self.file_name)
    }
}

/// What a cleanup pass removed (or, for a dry run, would remove).
///
/// Carrying the candidates rather than just counts lets the UI say *which*
/// series is about to shrink, and lets tests assert the exact set — the same
/// value type is returned by the dry run and the real run precisely so the two
/// can be compared.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CleanupSummary {
    pub chapters: Vec<CleanupCandidate>,
}

impl CleanupSummary {
    /// How many chapter files.
    pub fn files(&self) -> usize {
        self.chapters.len()
    }

    /// How many bytes they occupy.
    pub fn bytes(&self) -> u64 {
        self.chapters.iter().map(|c| c.bytes).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.chapters.is_empty()
    }
}

/// Seconds in an hour, as the setting's unit.
const SECONDS_PER_HOUR: u64 = 3600;

/// Report what a cleanup would remove, without touching the disk.
///
/// This is the dry run the settings screen calls before offering the user the
/// button. It reads file sizes (a stat, not a write) and nothing else.
pub fn plan_finished_cleanup(
    library: &Path,
    progress: &ProgressStore,
    index: &SeriesIndex,
    cleanup_hours: u32,
) -> CleanupSummary {
    plan_finished_cleanup_at(library, progress, index, cleanup_hours, now_unix())
}

/// [`plan_finished_cleanup`] with the clock injected, so tests (and any
/// caller that has already read the time) are not at the mercy of wall time.
pub fn plan_finished_cleanup_at(
    library: &Path,
    progress: &ProgressStore,
    index: &SeriesIndex,
    cleanup_hours: u32,
    now_unix: u64,
) -> CleanupSummary {
    // 0 means "never": the whole pass is skipped, so no amount of odd data
    // elsewhere can produce a deletion.
    if cleanup_hours == 0 {
        return CleanupSummary::default();
    }
    // Saturating: a huge setting means a cutoff at the epoch, i.e. nothing is
    // ever old enough. Wrapping here would mean the opposite.
    let cutoff = now_unix.saturating_sub(cleanup_hours as u64 * SECONDS_PER_HOUR);

    let mut chapters = Vec::new();
    for (series_dir, series) in index.iter() {
        // Only files the index knows about *and* that are really on disk. The
        // on-disk check is what makes repeated runs idempotent, and it also
        // keeps the "last chapter" rule honest: a stale index entry must not
        // be counted as the survivor that licenses deleting a real file.
        let present: Vec<(&str, &str, PathBuf, u64)> = series
            .downloaded
            .iter()
            .filter_map(|(chapter_id, file_name)| {
                let path = chapter_path(library, series_dir, file_name)?;
                let bytes = std::fs::metadata(&path).ok().filter(|m| m.is_file())?.len();
                Some((chapter_id.as_str(), file_name.as_str(), path, bytes))
            })
            .collect();

        // The chapter the reader last opened, as a library-relative key.
        let current = progress.last_opened(series_dir);

        let mut removable: Vec<CleanupCandidate> = present
            .iter()
            .filter(|(_, file_name, _, _)| {
                let key = format!("{series_dir}/{file_name}");
                if Some(key.as_str()) == current {
                    return false; // never the chapter the user is on
                }
                match progress.get(&key) {
                    // Never opened, or opened and not finished.
                    None => false,
                    Some(p) if !p.is_finished() => false,
                    // last_read_at == 0 is "unknown", not "1970": keep.
                    Some(p) => p.last_read_at != 0 && p.last_read_at < cutoff,
                }
            })
            .map(|(chapter_id, file_name, path, bytes)| CleanupCandidate {
                series_dir: series_dir.to_string(),
                chapter_id: (*chapter_id).to_string(),
                file_name: (*file_name).to_string(),
                path: path.clone(),
                bytes: *bytes,
            })
            .collect();

        // Refusal: never empty a series out. If every present chapter
        // qualifies, spare the one read most recently — the closest thing to
        // "where the user left off" when there is no `last_opened` record.
        if !removable.is_empty() && removable.len() == present.len() {
            let spare = removable
                .iter()
                .enumerate()
                .max_by_key(|(i, c)| {
                    (
                        progress.get(&c.progress_key()).map_or(0, |p| p.last_read_at),
                        // Tie-break on the file name so the choice is stable
                        // across runs and platforms rather than arbitrary.
                        std::cmp::Reverse(*i),
                    )
                })
                .map(|(i, _)| i);
            if let Some(i) = spare {
                removable.remove(i);
            }
        }

        chapters.extend(removable);
    }

    // Deterministic order, so the dry run and the real run agree and the UI
    // lists things the same way twice.
    chapters.sort_by(|a, b| {
        a.series_dir
            .cmp(&b.series_dir)
            .then_with(|| a.file_name.cmp(&b.file_name))
    });
    CleanupSummary { chapters }
}

/// Delete the finished, stale chapters [`plan_finished_cleanup`] identifies,
/// drop them from `index`, and persist the index.
///
/// `index` is taken by `&mut` and saved here (only when something actually
/// changed) so the on-disk index can never claim a file that this pass just
/// removed. The [`ProgressStore`] is taken by shared reference on purpose:
/// this function has no way to modify reading progress, which is the strongest
/// available statement that finishing a chapter is remembered after its file
/// is gone.
pub fn run_finished_cleanup(
    library: &Path,
    progress: &ProgressStore,
    index: &mut SeriesIndex,
    cleanup_hours: u32,
) -> Result<CleanupSummary> {
    run_finished_cleanup_at(library, progress, index, cleanup_hours, now_unix())
}

/// [`run_finished_cleanup`] with the clock injected.
pub fn run_finished_cleanup_at(
    library: &Path,
    progress: &ProgressStore,
    index: &mut SeriesIndex,
    cleanup_hours: u32,
    now_unix: u64,
) -> Result<CleanupSummary> {
    let plan = plan_finished_cleanup_at(library, progress, index, cleanup_hours, now_unix);
    if plan.is_empty() {
        // Nothing qualified: don't rewrite the index for nothing.
        return Ok(plan);
    }

    let mut removed = Vec::new();
    for candidate in plan.chapters {
        match std::fs::remove_file(&candidate.path) {
            Ok(()) => {
                index.forget_download(&candidate.series_dir, &candidate.file_name);
                removed.push(candidate);
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // Vanished between plan and delete (a concurrent delete, a
                // card yanked and returned). The goal state is reached; drop
                // the index entry but don't claim the bytes back.
                index.forget_download(&candidate.series_dir, &candidate.file_name);
            }
            Err(_) => {
                // One unreadable file (bad permissions, I/O error) must not
                // abort reclaiming everything else, and must not be reported
                // as freed. The index keeps pointing at it, which is the
                // truth: it is still there.
            }
        }
    }

    let summary = CleanupSummary { chapters: removed };
    index.save(library)?;
    Ok(summary)
}

/// Resolve `library/series_dir/file_name`, refusing anything that isn't a
/// plain file name inside a plain directory name.
///
/// The index is gideon's own file, but it is JSON on a FAT32 card that a user
/// can edit and that a bad write can mangle, and this function is the last
/// thing standing between that file and `remove_file`. A `..` or an absolute
/// path in either component is refused outright rather than normalized.
fn chapter_path(library: &Path, series_dir: &str, file_name: &str) -> Option<PathBuf> {
    if !is_plain_component(series_dir) || !is_plain_component(file_name) {
        return None;
    }
    Some(library.join(series_dir).join(file_name))
}

/// Whether `raw` is exactly one ordinary path component — no separators, no
/// `.`/`..`, no root, no empty string.
fn is_plain_component(raw: &str) -> bool {
    let path = Path::new(raw);
    let mut components = path.components();
    matches!(components.next(), Some(std::path::Component::Normal(_))) && components.next().is_none()
}

/// Current unix time in seconds, matching `ReadingProgress::last_read_at`.
fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

