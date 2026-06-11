//! Library scanning and reading-progress persistence.
//!
//! A library is just a directory tree containing `.cbz` files. Reading
//! progress is stored out-of-band in a single JSON file so the archives
//! themselves are never modified.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::natsort::natural_cmp;
use crate::Result;

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

    /// Persist the store to `path` atomically (write temp file then rename).
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, serde_json::to_string_pretty(self)?)?;
        fs::rename(&tmp, path)?;
        Ok(())
    }

    pub fn get(&self, key: &str) -> Option<ReadingProgress> {
        self.progress.get(key).copied()
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

    pub fn len(&self) -> usize {
        self.progress.len()
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
    fn finished_detection() {
        let p = ReadingProgress {
            current_page: 19,
            total_pages: 20,
            last_read_at: 0,
        };
        assert!(p.is_finished());
        assert_eq!(p.percent(), 100.0);
    }
}
