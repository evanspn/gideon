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
                        progress
                            .get(&c.progress_key())
                            .map_or(0, |p| p.last_read_at),
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
    matches!(components.next(), Some(std::path::Component::Normal(_)))
        && components.next().is_none()
}

/// Current unix time in seconds, matching `ReadingProgress::last_read_at`.
fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gideon_core::series::SeriesRef;
    use serde_json::json;

    const HOUR: u64 = 3600;
    /// A fixed "now" well past the epoch, so `now - N hours` never saturates.
    const NOW: u64 = 1_700_000_000;

    /// A library on disk plus the index and progress rows that describe it.
    ///
    /// Progress is built through serde (the same path `progress.json` takes)
    /// rather than through `ProgressStore::update`, because `update` stamps the
    /// wall clock and these tests need chapters read at a chosen time.
    struct Fixture {
        dir: tempfile::TempDir,
        index: SeriesIndex,
        /// key → (current_page, total_pages, last_read_at)
        rows: Vec<(String, usize, usize, u64)>,
        last_opened: Vec<(String, String)>,
    }

    impl Fixture {
        fn new() -> Self {
            Self {
                dir: tempfile::tempdir().unwrap(),
                index: SeriesIndex::default(),
                rows: Vec::new(),
                last_opened: Vec::new(),
            }
        }

        fn library(&self) -> &Path {
            self.dir.path()
        }

        /// Create `chapters` (file name, size) on disk under `series_dir` and
        /// record them all in the index.
        fn series(&mut self, series_dir: &str, chapters: &[(&str, usize)]) {
            self.index.record(
                series_dir,
                SeriesRef {
                    source_id: "s".into(),
                    source_name: "S".into(),
                    manga_id: series_dir.into(),
                    manga_title: series_dir.into(),
                    ..SeriesRef::default()
                },
            );
            std::fs::create_dir_all(self.library().join(series_dir)).unwrap();
            for (i, (file_name, size)) in chapters.iter().enumerate() {
                std::fs::write(
                    self.library().join(series_dir).join(file_name),
                    vec![7u8; *size],
                )
                .unwrap();
                self.index
                    .record_download(series_dir, &format!("c{i}"), file_name);
            }
        }

        /// Record progress for `series_dir/file_name`, finished or not, read
        /// `hours_ago` hours before NOW.
        fn read(&mut self, series_dir: &str, file_name: &str, finished: bool, hours_ago: u64) {
            let total = 10;
            let current = if finished { total - 1 } else { 3 };
            self.row(
                &format!("{series_dir}/{file_name}"),
                current,
                total,
                NOW - hours_ago * HOUR,
            );
        }

        /// A raw progress row, for cases (unknown timestamp, odd keys) the
        /// convenience helper can't express.
        fn row(&mut self, key: &str, current: usize, total: usize, last_read_at: u64) {
            self.rows.retain(|(k, ..)| k != key);
            self.rows
                .push((key.to_string(), current, total, last_read_at));
        }

        fn open(&mut self, series_dir: &str, file_name: &str) {
            self.last_opened
                .push((series_dir.to_string(), format!("{series_dir}/{file_name}")));
        }

        fn progress(&self) -> ProgressStore {
            let progress: serde_json::Map<String, serde_json::Value> = self
                .rows
                .iter()
                .map(|(key, current, total, last_read_at)| {
                    (
                        key.clone(),
                        json!({
                            "current_page": current,
                            "total_pages": total,
                            "last_read_at": last_read_at,
                        }),
                    )
                })
                .collect();
            let last_opened: serde_json::Map<String, serde_json::Value> = self
                .last_opened
                .iter()
                .map(|(series, key)| (series.clone(), json!(key)))
                .collect();
            serde_json::from_value(json!({
                "progress": progress,
                "last_opened": last_opened,
            }))
            .unwrap()
        }

        fn exists(&self, series_dir: &str, file_name: &str) -> bool {
            self.library().join(series_dir).join(file_name).exists()
        }

        fn plan(&self, hours: u32) -> CleanupSummary {
            plan_finished_cleanup_at(self.library(), &self.progress(), &self.index, hours, NOW)
        }

        fn run(&mut self, hours: u32) -> CleanupSummary {
            let library = self.dir.path().to_path_buf();
            let progress = self.progress();
            run_finished_cleanup_at(&library, &progress, &mut self.index, hours, NOW).unwrap()
        }

        fn downloaded(&self, series_dir: &str) -> Vec<String> {
            self.index
                .get(series_dir)
                .map(|s| s.downloaded.values().cloned().collect())
                .unwrap_or_default()
        }
    }

    #[test]
    fn removes_a_finished_and_stale_chapter() {
        let mut f = Fixture::new();
        f.series("Manga", &[("c1.cbz", 100), ("c2.cbz", 200)]);
        f.read("Manga", "c1.cbz", true, 72);
        f.read("Manga", "c2.cbz", false, 1);

        let summary = f.run(48);
        assert_eq!(summary.files(), 1);
        assert_eq!(summary.bytes(), 100);
        assert_eq!(summary.chapters[0].file_name, "c1.cbz");
        assert_eq!(summary.chapters[0].chapter_id, "c0");
        assert!(!f.exists("Manga", "c1.cbz"));
        assert!(f.exists("Manga", "c2.cbz"));
    }

    #[test]
    fn keeps_a_finished_but_recent_chapter() {
        let mut f = Fixture::new();
        f.series("Manga", &[("c1.cbz", 100), ("c2.cbz", 100)]);
        // Finished 47 hours ago, cutoff is 48: still inside the grace period.
        f.read("Manga", "c1.cbz", true, 47);
        f.read("Manga", "c2.cbz", true, 47);

        assert!(f.run(48).is_empty());
        assert!(f.exists("Manga", "c1.cbz"));
        assert!(f.exists("Manga", "c2.cbz"));
    }

    #[test]
    fn keeps_unfinished_and_never_opened_chapters() {
        let mut f = Fixture::new();
        f.series(
            "Manga",
            &[("c1.cbz", 100), ("c2.cbz", 100), ("c3.cbz", 100)],
        );
        // Mid-read, and old enough to be stale — but unfinished.
        f.read("Manga", "c1.cbz", false, 500);
        // c2 has no progress row at all: never opened.
        // c3 is finished and stale, and is present so the "last chapter" rule
        // isn't what saves c1/c2.
        f.read("Manga", "c3.cbz", true, 500);

        let summary = f.run(48);
        assert_eq!(summary.files(), 1);
        assert_eq!(summary.chapters[0].file_name, "c3.cbz");
        assert!(f.exists("Manga", "c1.cbz"), "unfinished chapter kept");
        assert!(f.exists("Manga", "c2.cbz"), "never-opened chapter kept");
    }

    #[test]
    fn keeps_a_chapter_with_an_unknown_read_time() {
        // A progress row synced from another device without a timestamp:
        // last_read_at 0 means unknown, and unknown must not read as 1970.
        let mut f = Fixture::new();
        f.series("Manga", &[("c1.cbz", 100), ("c2.cbz", 100)]);
        f.row("Manga/c1.cbz", 9, 10, 0);
        f.read("Manga", "c2.cbz", true, 500);

        let summary = f.run(48);
        assert_eq!(summary.files(), 1);
        assert!(f.exists("Manga", "c1.cbz"), "unknown timestamp is kept");
    }

    #[test]
    fn keeps_the_chapter_the_user_is_currently_on() {
        let mut f = Fixture::new();
        f.series(
            "Manga",
            &[("c1.cbz", 100), ("c2.cbz", 100), ("c3.cbz", 100)],
        );
        // All three finished long ago, but the reader is sitting on c2 — the
        // user finished it and hasn't moved on yet.
        f.read("Manga", "c1.cbz", true, 500);
        f.read("Manga", "c2.cbz", true, 500);
        f.read("Manga", "c3.cbz", true, 500);
        f.open("Manga", "c2.cbz");

        let summary = f.run(48);
        assert!(f.exists("Manga", "c2.cbz"), "the current chapter is kept");
        assert_eq!(summary.files(), 2);
        assert!(!f.exists("Manga", "c1.cbz"));
        assert!(!f.exists("Manga", "c3.cbz"));
    }

    #[test]
    fn never_empties_a_series() {
        let mut f = Fixture::new();
        f.series("Manga", &[("c1.cbz", 100), ("c2.cbz", 100)]);
        // Both finished and stale, and no last_opened record at all.
        f.read("Manga", "c1.cbz", true, 500);
        f.read("Manga", "c2.cbz", true, 200); // read more recently

        let summary = f.run(48);
        assert_eq!(summary.files(), 1, "one chapter always survives");
        assert!(!f.exists("Manga", "c1.cbz"));
        assert!(
            f.exists("Manga", "c2.cbz"),
            "the most recently read chapter is the survivor"
        );

        // And a second pass over the now-single-chapter series removes nothing.
        assert!(f.run(48).is_empty());
        assert!(f.exists("Manga", "c2.cbz"));
    }

    #[test]
    fn a_single_chapter_series_is_never_touched() {
        let mut f = Fixture::new();
        f.series("Solo", &[("only.cbz", 100)]);
        f.read("Solo", "only.cbz", true, 10_000);

        assert!(f.run(48).is_empty());
        assert!(f.exists("Solo", "only.cbz"));
        assert_eq!(f.downloaded("Solo"), vec!["only.cbz".to_string()]);
    }

    #[test]
    fn the_last_chapter_rule_is_per_series() {
        let mut f = Fixture::new();
        f.series("A", &[("a1.cbz", 100), ("a2.cbz", 100)]);
        f.series("B", &[("b1.cbz", 100)]);
        for (dir, file) in [("A", "a1.cbz"), ("A", "a2.cbz"), ("B", "b1.cbz")] {
            f.read(dir, file, true, 500);
        }

        let summary = f.run(48);
        assert_eq!(summary.files(), 1, "one from A, none from B");
        assert_eq!(summary.chapters[0].series_dir, "A");
        assert!(f.exists("B", "b1.cbz"));
    }

    #[test]
    fn reading_progress_survives_deletion() {
        let mut f = Fixture::new();
        f.series("Manga", &[("c1.cbz", 100), ("c2.cbz", 100)]);
        f.read("Manga", "c1.cbz", true, 500);
        f.read("Manga", "c2.cbz", false, 1);

        f.run(48);
        assert!(!f.exists("Manga", "c1.cbz"));

        // The store the caller handed in is untouched (it is a shared
        // reference — this pass has no way to modify it), and a store saved
        // after the run still remembers the chapter was finished.
        let progress = f.progress();
        assert!(
            progress.get("Manga/c1.cbz").unwrap().is_finished(),
            "the finished record outlives the file"
        );
        let path = f.library().join("progress.json");
        progress.save(&path).unwrap();
        let reloaded = ProgressStore::load(&path).unwrap();
        assert!(reloaded.get("Manga/c1.cbz").unwrap().is_finished());
    }

    #[test]
    fn the_index_is_updated_and_persisted() {
        let mut f = Fixture::new();
        f.series("Manga", &[("c1.cbz", 100), ("c2.cbz", 100)]);
        f.read("Manga", "c1.cbz", true, 500);
        f.read("Manga", "c2.cbz", false, 1);

        f.run(48);
        assert_eq!(f.downloaded("Manga"), vec!["c2.cbz".to_string()]);

        let reloaded = SeriesIndex::load(f.library());
        let downloaded: Vec<String> = reloaded
            .get("Manga")
            .expect("the series itself stays in the index")
            .downloaded
            .values()
            .cloned()
            .collect();
        assert_eq!(
            downloaded,
            vec!["c2.cbz".to_string()],
            "the saved index no longer claims the deleted file"
        );
    }

    #[test]
    fn zero_hours_disables_cleanup_entirely() {
        let mut f = Fixture::new();
        f.series(
            "Manga",
            &[("c1.cbz", 100), ("c2.cbz", 100), ("c3.cbz", 100)],
        );
        for file in ["c1.cbz", "c2.cbz", "c3.cbz"] {
            f.read("Manga", file, true, 100_000);
        }

        assert!(f.plan(0).is_empty(), "dry run reports nothing");
        assert!(f.run(0).is_empty());
        for file in ["c1.cbz", "c2.cbz", "c3.cbz"] {
            assert!(f.exists("Manga", file));
        }
        assert_eq!(f.downloaded("Manga").len(), 3);
        assert!(
            !f.library().join(".gideon").join("series.json").exists(),
            "a disabled pass doesn't even rewrite the index"
        );
    }

    #[test]
    fn dry_run_touches_nothing_but_reports_the_same_set() {
        let mut f = Fixture::new();
        f.series(
            "Manga",
            &[("c1.cbz", 100), ("c2.cbz", 250), ("c3.cbz", 100)],
        );
        f.read("Manga", "c1.cbz", true, 500);
        f.read("Manga", "c2.cbz", true, 500);
        f.read("Manga", "c3.cbz", false, 1);

        let planned = f.plan(48);
        assert_eq!(planned.files(), 2);
        assert_eq!(planned.bytes(), 350);
        // Nothing moved.
        for file in ["c1.cbz", "c2.cbz", "c3.cbz"] {
            assert!(
                f.exists("Manga", file),
                "{file} still on disk after dry run"
            );
        }
        assert_eq!(f.downloaded("Manga").len(), 3);
        assert!(!f.library().join(".gideon").join("series.json").exists());

        // Planning again gives the same answer, and the real run matches it.
        assert_eq!(f.plan(48), planned);
        assert_eq!(f.run(48), planned);
    }

    #[test]
    fn repeated_runs_are_idempotent() {
        let mut f = Fixture::new();
        f.series(
            "Manga",
            &[("c1.cbz", 100), ("c2.cbz", 100), ("c3.cbz", 100)],
        );
        f.read("Manga", "c1.cbz", true, 500);
        f.read("Manga", "c2.cbz", true, 500);
        f.read("Manga", "c3.cbz", false, 1);

        let first = f.run(48);
        assert_eq!(first.files(), 2);
        let second = f.run(48);
        assert!(second.is_empty(), "nothing left to do: {second:?}");
        assert_eq!(f.run(48), CleanupSummary::default());
        assert!(f.exists("Manga", "c3.cbz"));
        assert_eq!(f.downloaded("Manga"), vec!["c3.cbz".to_string()]);
    }

    #[test]
    fn an_empty_library_is_a_no_op() {
        let mut f = Fixture::new();
        assert!(f.plan(48).is_empty());
        assert!(f.run(48).is_empty());
    }

    #[test]
    fn a_series_with_no_files_on_disk_is_left_alone() {
        // Every row stale, every file already gone (a card swap, a manual
        // delete). Nothing to do, and nothing to panic about.
        let mut f = Fixture::new();
        f.series("Manga", &[("c1.cbz", 100), ("c2.cbz", 100)]);
        std::fs::remove_dir_all(f.library().join("Manga")).unwrap();
        f.read("Manga", "c1.cbz", true, 500);
        f.read("Manga", "c2.cbz", true, 500);

        assert!(f.run(48).is_empty());
        assert_eq!(f.downloaded("Manga").len(), 2, "the index is left as-is");
    }

    #[test]
    fn index_entries_whose_files_are_gone_are_ignored_not_counted() {
        // A stale index row must not act as the surviving chapter that
        // licenses deleting the only real file.
        let mut f = Fixture::new();
        f.series("Manga", &[("c1.cbz", 100)]);
        f.index.record_download("Manga", "ghost", "ghost.cbz");
        f.read("Manga", "c1.cbz", true, 500);
        f.read("Manga", "ghost.cbz", true, 500);

        assert!(f.run(48).is_empty(), "the only real file is the last one");
        assert!(f.exists("Manga", "c1.cbz"));
    }

    #[test]
    fn traversal_style_index_entries_are_refused() {
        // A mangled index must never make this pass delete outside a series
        // directory.
        let mut f = Fixture::new();
        f.series("Manga", &[("c1.cbz", 100), ("c2.cbz", 100)]);
        f.read("Manga", "c1.cbz", true, 500);
        f.read("Manga", "c2.cbz", true, 500);

        let outsider = f.library().join("precious.cbz");
        std::fs::write(&outsider, b"do not touch").unwrap();
        f.index.record_download("Manga", "evil", "../precious.cbz");
        f.row("Manga/../precious.cbz", 9, 10, NOW - 500 * HOUR);

        f.run(48);
        assert!(outsider.exists(), "path traversal entry was refused");
    }

    #[test]
    fn plain_component_check() {
        assert!(is_plain_component("c1.cbz"));
        assert!(is_plain_component("Manga One"));
        assert!(!is_plain_component("../precious.cbz"));
        assert!(!is_plain_component("a/b"));
        assert!(!is_plain_component("."));
        assert!(!is_plain_component(""));
        assert!(!is_plain_component("/etc/passwd"));
    }
}
