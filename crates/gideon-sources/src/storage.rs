//! Downloaded-chapter storage with a size budget.
//!
//! Chapters are stored as CBZ files under a downloads directory. When the
//! total size exceeds the configured budget (settings:
//! `storage_size_limit`), the least-recently-accessed chapters are evicted
//! first — mirroring bobo's chapter storage behavior. This is the engine
//! that chapter pre-downloading builds on.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use url::Url;

use crate::download::download_chapter_to_cbz;
use crate::fetch::Fetcher;
use crate::Result;

pub struct ChapterStorage {
    dir: PathBuf,
    limit_bytes: u64,
}

impl ChapterStorage {
    pub fn new(dir: impl Into<PathBuf>, limit_bytes: u64) -> Self {
        Self {
            dir: dir.into(),
            limit_bytes,
        }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Path where a chapter is (or would be) stored. The key is sanitized so
    /// source-provided ids can't escape the storage directory or produce
    /// invalid FAT32 filenames — a lesson from bobo's "sanitize chapter
    /// filenames" fixes.
    pub fn chapter_path(&self, manga_key: &str, chapter_key: &str) -> PathBuf {
        self.dir
            .join(sanitize(manga_key))
            .join(format!("{}.cbz", sanitize(chapter_key)))
    }

    pub fn has_chapter(&self, manga_key: &str, chapter_key: &str) -> bool {
        self.chapter_path(manga_key, chapter_key).exists()
    }

    /// Download a chapter into storage (no-op if already present), then
    /// enforce the size budget.
    pub fn download_chapter(
        &self,
        fetcher: &dyn Fetcher,
        manga_key: &str,
        chapter_key: &str,
        page_urls: &[Url],
    ) -> Result<PathBuf> {
        let path = self.chapter_path(manga_key, chapter_key);
        if !path.exists() {
            download_chapter_to_cbz(fetcher, page_urls, &path)?;
        }
        self.enforce_limit()?;
        Ok(path)
    }

    /// Pre-download up to `count` chapters from `queue` that aren't stored
    /// yet (the settings' `predownload_unread_chapters` drives `count`).
    /// Returns the paths of newly downloaded chapters; stops at the first
    /// failure so one bad chapter doesn't block recording earlier successes.
    pub fn predownload(
        &self,
        fetcher: &dyn Fetcher,
        queue: &[(String, String, Vec<Url>)],
        count: u32,
    ) -> Result<Vec<PathBuf>> {
        let mut downloaded = Vec::new();
        for (manga_key, chapter_key, page_urls) in queue {
            if downloaded.len() as u32 >= count {
                break;
            }
            if self.has_chapter(manga_key, chapter_key) {
                continue;
            }
            let path = self.download_chapter(fetcher, manga_key, chapter_key, page_urls)?;
            downloaded.push(path);
        }
        Ok(downloaded)
    }

    /// Total bytes used by stored chapters.
    pub fn total_size(&self) -> Result<u64> {
        Ok(self.stored_chapters()?.iter().map(|c| c.size).sum())
    }

    /// Evict least-recently-accessed chapters until under the budget.
    /// Returns the evicted paths.
    pub fn enforce_limit(&self) -> Result<Vec<PathBuf>> {
        let mut chapters = self.stored_chapters()?;
        let mut total: u64 = chapters.iter().map(|c| c.size).sum();
        if total <= self.limit_bytes {
            return Ok(Vec::new());
        }

        // Oldest access first.
        chapters.sort_by_key(|c| c.accessed);
        let mut evicted = Vec::new();
        for chapter in chapters {
            if total <= self.limit_bytes {
                break;
            }
            fs::remove_file(&chapter.path)?;
            total = total.saturating_sub(chapter.size);
            evicted.push(chapter.path);
        }
        Ok(evicted)
    }

    fn stored_chapters(&self) -> Result<Vec<StoredChapter>> {
        let mut chapters = Vec::new();
        if !self.dir.exists() {
            return Ok(chapters);
        }
        collect_cbz(&self.dir, &mut chapters)?;
        Ok(chapters)
    }
}

struct StoredChapter {
    path: PathBuf,
    size: u64,
    accessed: SystemTime,
}

fn collect_cbz(dir: &Path, out: &mut Vec<StoredChapter>) -> Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            collect_cbz(&path, out)?;
        } else if path
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("cbz"))
        {
            let meta = entry.metadata()?;
            // Recency is the newer of access and modification time: atime
            // can be frozen by noatime mounts (mtime then governs), and
            // when atime does work, reading a chapter bumps it — exactly
            // the LRU signal we want.
            let atime = meta.accessed().unwrap_or(SystemTime::UNIX_EPOCH);
            let mtime = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
            let accessed = atime.max(mtime);
            out.push(StoredChapter {
                path,
                size: meta.len(),
                accessed,
            });
        }
    }
    Ok(())
}

/// Make a string safe to use as a single FAT32-friendly file name component.
pub fn sanitize(raw: &str) -> String {
    let cleaned: String = raw
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();
    let trimmed = cleaned.trim_matches(|c: char| c == '.' || c.is_whitespace());
    if trimmed.is_empty() {
        "untitled".to_string()
    } else {
        // FAT32 name component limit is 255; stay well under it.
        trimmed.chars().take(120).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fetch::FakeFetcher;
    use filetime::{set_file_times, FileTime};

    fn fetcher_with_pages(pages: &[(&str, usize)]) -> FakeFetcher {
        let mut f = FakeFetcher::new();
        for (url, size) in pages {
            f = f.with(url, vec![0xAB; *size]);
        }
        f
    }

    #[test]
    fn sanitize_makes_keys_fat32_safe() {
        assert_eq!(
            sanitize("One Piece: Chapter 1/2?"),
            "One Piece_ Chapter 1_2_"
        );
        assert_eq!(sanitize("../../etc/passwd"), "_.._etc_passwd");
        assert_eq!(sanitize("  ...  "), "untitled");
        assert!(sanitize(&"x".repeat(500)).len() <= 120);
    }

    #[test]
    fn download_stores_and_skips_existing() {
        let dir = tempfile::tempdir().unwrap();
        let storage = ChapterStorage::new(dir.path(), 1024 * 1024);
        let fetcher = fetcher_with_pages(&[("https://cdn.example.com/p1.jpg", 100)]);
        let urls = vec![Url::parse("https://cdn.example.com/p1.jpg").unwrap()];

        let path = storage
            .download_chapter(&fetcher, "Manga", "ch1", &urls)
            .unwrap();
        assert!(path.exists());
        assert!(storage.has_chapter("Manga", "ch1"));

        // Second call with a fetcher that would fail proves we don't re-fetch.
        let empty_fetcher = FakeFetcher::new();
        storage
            .download_chapter(&empty_fetcher, "Manga", "ch1", &urls)
            .unwrap();
    }

    #[test]
    fn eviction_removes_least_recently_accessed_first() {
        let dir = tempfile::tempdir().unwrap();
        // Budget fits two ~1KB chapters but not three.
        let storage = ChapterStorage::new(dir.path(), 2500);
        let fetcher = fetcher_with_pages(&[
            ("https://cdn.example.com/a.jpg", 1000),
            ("https://cdn.example.com/b.jpg", 1000),
            ("https://cdn.example.com/c.jpg", 1000),
        ]);

        for (i, name) in ["a", "b", "c"].iter().enumerate() {
            let urls = vec![Url::parse(&format!("https://cdn.example.com/{name}.jpg")).unwrap()];
            let path = storage.chapter_path("Manga", &format!("ch{name}"));
            if !path.exists() {
                download_chapter_to_cbz(&fetcher, &urls, &path).unwrap();
            }
            // Give each file distinct, ordered timestamps: a oldest. Both
            // atime and mtime are set so recency ordering is deterministic.
            let t = FileTime::from_unix_time(1000 + i as i64 * 100, 0);
            set_file_times(&path, t, t).unwrap();
        }

        let evicted = storage.enforce_limit().unwrap();
        assert_eq!(evicted.len(), 1, "exactly one chapter should be evicted");
        assert!(
            evicted[0].ends_with("cha.cbz"),
            "oldest (cha) should go first: {evicted:?}"
        );
        assert!(!storage.has_chapter("Manga", "cha"));
        assert!(storage.has_chapter("Manga", "chb"));
        assert!(storage.has_chapter("Manga", "chc"));
        assert!(storage.total_size().unwrap() <= 2500);
    }

    #[test]
    fn predownload_respects_count_and_skips_stored() {
        let dir = tempfile::tempdir().unwrap();
        let storage = ChapterStorage::new(dir.path(), 1024 * 1024);
        let fetcher = fetcher_with_pages(&[
            ("https://cdn.example.com/1.jpg", 10),
            ("https://cdn.example.com/2.jpg", 10),
            ("https://cdn.example.com/3.jpg", 10),
        ]);

        let queue: Vec<(String, String, Vec<Url>)> = (1..=3)
            .map(|i| {
                (
                    "Manga".to_string(),
                    format!("ch{i}"),
                    vec![Url::parse(&format!("https://cdn.example.com/{i}.jpg")).unwrap()],
                )
            })
            .collect();

        // Pre-download 2 of 3.
        let downloaded = storage.predownload(&fetcher, &queue, 2).unwrap();
        assert_eq!(downloaded.len(), 2);
        assert!(storage.has_chapter("Manga", "ch1"));
        assert!(storage.has_chapter("Manga", "ch2"));
        assert!(!storage.has_chapter("Manga", "ch3"));

        // Next round downloads only the missing one.
        let downloaded = storage.predownload(&fetcher, &queue, 2).unwrap();
        assert_eq!(downloaded.len(), 1);
        assert!(storage.has_chapter("Manga", "ch3"));

        // Zero disables pre-downloading.
        let none = storage.predownload(&fetcher, &queue, 0).unwrap();
        assert!(none.is_empty());
    }
}
