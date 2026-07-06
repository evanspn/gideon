//! Library scanning and reading-progress persistence.
//!
//! A library is just a directory tree containing `.cbz` files. Reading
//! progress is stored out-of-band in a single JSON file so the archives
//! themselves are never modified.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::natsort::natural_cmp;
use crate::Result;

/// Serializes all `progress.json` writes in this process, so the reader thread
/// and the background sync thread can't interleave a read-modify-write and lose
/// each other's updates. Held only for the brief load-merge-rename, never across
/// network I/O.
static SAVE_LOCK: Mutex<()> = Mutex::new(());

/// A per-write-unique temp path next to `path`, so two concurrent atomic saves
/// never share (and corrupt) one temp file before the rename.
fn unique_temp_path(path: &Path) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".tmp.{pid}.{n}"));
    path.with_file_name(name)
}

/// A manga archive discovered in the library directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LibraryEntry {
    pub path: PathBuf,
    /// Path relative to the library root, used as the progress key.
    pub relative_path: String,
}

/// Reading progress for a single document.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadingProgress {
    /// Zero-based index of the last page the user was on.
    pub current_page: usize,
    /// Page count at the time progress was recorded.
    pub total_pages: usize,
    /// Unix timestamp (seconds) of the last read.
    pub last_read_at: u64,
}

impl ReadingProgress {
    pub fn is_finished(&self) -> bool {
        self.total_pages > 0 && self.current_page + 1 >= self.total_pages
    }

    pub fn percent(&self) -> f32 {
        if self.total_pages == 0 {
            return 0.0;
        }
        (self.current_page + 1) as f32 / self.total_pages as f32 * 100.0
    }
}

/// A scanned library rooted at a directory.
pub struct Library {
    root: PathBuf,
}

impl Library {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Recursively find every `.cbz` under the root, in natural order.
    pub fn scan(&self) -> Result<Vec<LibraryEntry>> {
        let mut paths = Vec::new();
        scan_dir(&self.root, &mut paths)?;
        let mut entries: Vec<LibraryEntry> = paths
            .into_iter()
            .map(|path| {
                let relative_path = path
                    .strip_prefix(&self.root)
                    .unwrap_or(&path)
                    .to_string_lossy()
                    .replace('\\', "/");
                LibraryEntry {
                    path,
                    relative_path,
                }
            })
            .collect();
        entries.sort_by(|a, b| natural_cmp(&a.relative_path, &b.relative_path));
        Ok(entries)
    }
}

fn scan_dir(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('.') {
            continue;
        }
        if path.is_dir() {
            // Profile libraries live in "@name" subdirectories; a scan of
            // the root (the default profile) must not see other profiles'
            // books. The @ prefix keeps them apart from series dirs.
            if name.starts_with('@') {
                continue;
            }
            scan_dir(&path, out)?;
        } else if name.to_ascii_lowercase().ends_with(".cbz") {
            out.push(path);
        }
    }
    Ok(())
}

/// JSON-backed store mapping library-relative paths to reading progress.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct ProgressStore {
    #[serde(default)]
    progress: HashMap<String, ReadingProgress>,
    /// The last chapter opened per series (series directory → chapter key).
    /// Recorded the instant a chapter opens, so "resume" lands on exactly
    /// where the reader last was — independent of the wall clock and robust to
    /// the app being killed before progress is flushed.
    #[serde(default)]
    last_opened: HashMap<String, String>,
}

impl ProgressStore {
    /// Load the store from `path`, returning an empty store if the file
    /// doesn't exist yet.
    pub fn load(path: &Path) -> Result<Self> {
        match fs::read_to_string(path) {
            Ok(contents) => Ok(serde_json::from_str(&contents)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e.into()),
        }
    }

    /// Persist the store to `path` atomically and **authoritatively** — the
    /// on-disk file becomes exactly this store. Use for writes that must be able
    /// to *remove* entries (mark-unread); a merge would resurrect them. Prefer
    /// [`Self::merge_save`] for progress-*advancing* writes that may race a
    /// second writer (the background sync thread), so a concurrent update isn't
    /// clobbered.
    pub fn save(&self, path: &Path) -> Result<()> {
        let _guard = SAVE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        self.write_atomic(path)
    }

    /// Fold this store into whatever is currently on disk and save, under the
    /// same process lock as [`Self::save`]. Merge rule mirrors the sync layer's
    /// **furthest-page-wins**: per chapter the higher `current_page` and later
    /// `last_read_at` win, this store's `total_pages` and `last_opened` are
    /// taken as the latest report, and chapters only on disk are preserved. So a
    /// write that landed between this store being loaded and saved (e.g. a
    /// background pull, or a reader session that advanced a page) is never lost.
    /// Only ever *raises* a page — never removes — so it must not be used for
    /// mark-unread.
    pub fn merge_save(&self, path: &Path) -> Result<()> {
        let _guard = SAVE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let mut disk = Self::load(path).unwrap_or_default();
        for (key, p) in &self.progress {
            disk.progress
                .entry(key.clone())
                .and_modify(|d| {
                    d.current_page = d.current_page.max(p.current_page);
                    d.total_pages = p.total_pages;
                    d.last_read_at = d.last_read_at.max(p.last_read_at);
                })
                .or_insert(*p);
        }
        for (series, chapter) in &self.last_opened {
            disk.last_opened.insert(series.clone(), chapter.clone());
        }
        disk.write_atomic(path)
    }

    /// Save, taking **this store's value** for every chapter it knows (so the
    /// reader can move a page up *or* down — a deliberate flip back must stick),
    /// while preserving any chapter a concurrent writer (the background sync
    /// thread) added to disk since this store was loaded. Under the same process
    /// lock as [`Self::save`]. Use for the reader's own progress writes; sync
    /// uses [`Self::merge_save`] (furthest-page-wins) so it can never rewind
    /// another device.
    pub fn overlay_save(&self, path: &Path) -> Result<()> {
        let _guard = SAVE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let mut disk = Self::load(path).unwrap_or_default();
        for (key, p) in &self.progress {
            disk.progress.insert(key.clone(), *p);
        }
        for (series, chapter) in &self.last_opened {
            disk.last_opened.insert(series.clone(), chapter.clone());
        }
        disk.write_atomic(path)
    }

    /// Atomic write (temp file + rename) with a *unique* temp name, so two
    /// concurrent writers can't corrupt a shared temp file. Callers hold
    /// `SAVE_LOCK`; this does not lock (it would deadlock).
    fn write_atomic(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let tmp = unique_temp_path(path);
        fs::write(&tmp, serde_json::to_string_pretty(self)?)?;
        fs::rename(&tmp, path)?;
        Ok(())
    }

    pub fn get(&self, key: &str) -> Option<ReadingProgress> {
        self.progress.get(key).copied()
    }

    /// Iterate every `(chapter_key, progress)` pair — used by sync to push
    /// the device's local progress to the backend.
    pub fn entries(&self) -> impl Iterator<Item = (&str, ReadingProgress)> {
        self.progress.iter().map(|(k, v)| (k.as_str(), *v))
    }

    /// Record that the user is on `current_page` of `total_pages`.
    pub fn update(&mut self, key: &str, current_page: usize, total_pages: usize) {
        let last_read_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        self.progress.insert(
            key.to_owned(),
            ReadingProgress {
                current_page,
                total_pages,
                last_read_at,
            },
        );
    }

    /// Record `chapter_key` as the chapter the user most recently opened in
    /// `series_key` (its top-level directory). Called the moment the reader
    /// opens a chapter.
    pub fn set_last_opened(&mut self, series_key: &str, chapter_key: &str) {
        self.last_opened
            .insert(series_key.to_owned(), chapter_key.to_owned());
    }

    /// The chapter most recently opened in `series_key`, if recorded.
    pub fn last_opened(&self, series_key: &str) -> Option<&str> {
        self.last_opened.get(series_key).map(|s| s.as_str())
    }

    pub fn len(&self) -> usize {
        self.progress.len()
    }

    /// Forget a chapter's progress entirely — used to "mark as unread" when the
    /// user opened/finished something by accident. Returns whether anything was
    /// removed. A later sync may pull the row back from another device, which is
    /// the correct furthest-page-wins behaviour; this clears it locally now.
    pub fn remove(&mut self, key: &str) -> bool {
        self.progress.remove(key).is_some()
    }

    pub fn is_empty(&self) -> bool {
        self.progress.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn touch(path: &Path) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, b"stub").unwrap();
    }

    #[test]
    fn scan_finds_cbz_recursively_in_natural_order() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        touch(&root.join("One Piece/vol10.cbz"));
        touch(&root.join("One Piece/vol2.cbz"));
        touch(&root.join("Berserk/vol1.cbz"));
        touch(&root.join("Berserk/notes.txt"));
        touch(&root.join(".hidden/secret.cbz"));
        touch(&root.join("loose.CBZ"));

        let entries = Library::new(root).scan().unwrap();
        let rel: Vec<&str> = entries.iter().map(|e| e.relative_path.as_str()).collect();
        assert_eq!(
            rel,
            vec![
                "Berserk/vol1.cbz",
                "loose.CBZ",
                "One Piece/vol2.cbz",
                "One Piece/vol10.cbz",
            ]
        );
    }

    #[test]
    fn scan_skips_profile_directories() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        touch(&root.join("Shared/vol1.cbz"));
        touch(&root.join("@alex/Alexs Series/vol1.cbz"));

        // The root (default profile) doesn't see other profiles' books...
        let entries = Library::new(root).scan().unwrap();
        let rel: Vec<&str> = entries.iter().map(|e| e.relative_path.as_str()).collect();
        assert_eq!(rel, vec!["Shared/vol1.cbz"]);

        // ...but a scan rooted at the profile dir sees its own.
        let entries = Library::new(root.join("@alex")).scan().unwrap();
        let rel: Vec<&str> = entries.iter().map(|e| e.relative_path.as_str()).collect();
        assert_eq!(rel, vec!["Alexs Series/vol1.cbz"]);
    }

    #[test]
    fn progress_round_trips_through_disk() {
        let dir = tempfile::tempdir().unwrap();
        let store_path = dir.path().join("state/progress.json");

        let mut store = ProgressStore::load(&store_path).unwrap();
        assert!(store.is_empty());

        store.update("One Piece/vol2.cbz", 5, 200);
        store.save(&store_path).unwrap();

        let reloaded = ProgressStore::load(&store_path).unwrap();
        let p = reloaded.get("One Piece/vol2.cbz").unwrap();
        assert_eq!(p.current_page, 5);
        assert_eq!(p.total_pages, 200);
        assert!(p.last_read_at > 0);
        assert!(!p.is_finished());
        assert!((p.percent() - 3.0).abs() < 0.01);
    }

    #[test]
    fn last_opened_round_trips_and_overrides() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state/progress.json");
        let mut store = ProgressStore::default();
        store.set_last_opened("Series", "Series/vol5.cbz");
        store.set_last_opened("Series", "Series/vol6.cbz"); // overwrites
        store.save(&path).unwrap();

        let reloaded = ProgressStore::load(&path).unwrap();
        assert_eq!(reloaded.last_opened("Series"), Some("Series/vol6.cbz"));
        assert_eq!(reloaded.last_opened("Other"), None);
    }

    #[test]
    fn remove_forgets_a_chapters_progress() {
        let mut store = ProgressStore::default();
        store.update("Series/vol1.cbz", 1, 2);
        assert!(store.remove("Series/vol1.cbz"), "removed an existing key");
        assert!(store.get("Series/vol1.cbz").is_none());
        assert!(
            !store.remove("Series/vol1.cbz"),
            "removing again is a no-op"
        );
    }

    #[test]
    fn finished_detection() {
        let p = ReadingProgress {
            current_page: 19,
            total_pages: 20,
            last_read_at: 0,
        };
        assert!(p.is_finished());
        assert_eq!(p.percent(), 100.0);
    }

    #[test]
    fn merge_save_preserves_a_concurrent_write_to_another_chapter() {
        // Simulates the reader/sync race: something wrote chapter B to disk
        // after this store (holding only A) was loaded. merge_save must fold in
        // rather than clobber, so B survives.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("progress.json");

        let mut on_disk = ProgressStore::default();
        on_disk.update("B/vol1.cbz", 15, 30); // written by the "other" writer
        on_disk.save(&path).unwrap();

        let mut mine = ProgressStore::default();
        mine.update("A/vol1.cbz", 3, 10); // my stale snapshot only knows A
        mine.merge_save(&path).unwrap();

        let merged = ProgressStore::load(&path).unwrap();
        assert!(merged.get("A/vol1.cbz").is_some(), "my chapter is written");
        assert_eq!(
            merged.get("B/vol1.cbz").unwrap().current_page,
            15,
            "the concurrent write to B is preserved, not clobbered"
        );
    }

    #[test]
    fn overlay_save_is_authoritative_but_preserves_other_chapters() {
        // The reader can move its own chapter down (deliberate flip back) AND a
        // chapter the background sync added to disk must survive.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("progress.json");

        let mut on_disk = ProgressStore::default();
        on_disk.update("A/vol1.cbz", 5, 10); // reader's chapter, currently 5
        on_disk.update("B/vol1.cbz", 15, 30); // added by a background sync
        on_disk.save(&path).unwrap();

        // Reader loaded A at 5, paged back to 2, and knows nothing of B.
        let mut reader = ProgressStore::default();
        reader.update("A/vol1.cbz", 2, 10);
        reader.overlay_save(&path).unwrap();

        let saved = ProgressStore::load(&path).unwrap();
        assert_eq!(
            saved.get("A/vol1.cbz").unwrap().current_page,
            2,
            "the reader is authoritative for its own chapter, even moving back"
        );
        assert_eq!(
            saved.get("B/vol1.cbz").unwrap().current_page,
            15,
            "a chapter the sync added is preserved, not clobbered"
        );
    }

    #[test]
    fn merge_save_never_lowers_a_page() {
        // Furthest-page-wins: a stale writer at page 2 can't rewind disk's 9.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("progress.json");

        let mut on_disk = ProgressStore::default();
        on_disk.update("A/vol1.cbz", 9, 10);
        on_disk.save(&path).unwrap();

        let mut stale = ProgressStore::default();
        stale.update("A/vol1.cbz", 2, 10);
        stale.merge_save(&path).unwrap();

        let merged = ProgressStore::load(&path).unwrap();
        assert_eq!(
            merged.get("A/vol1.cbz").unwrap().current_page,
            9,
            "merge_save keeps the furthest page, never rewinds"
        );
    }
}
