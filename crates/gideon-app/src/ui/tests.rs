//! Browse-UI state machine tests: MemoryDisplay + FakeInput + FakeGateway,
//! no network and no WASM runtime.

use std::cell::RefCell;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Result};

use gideon_core::ProgressStore;
use gideon_device::{FakeInput, MemoryDisplay, RefreshMode, UiEvent};

use super::*;

// --- fixtures ---

fn make_cbz(path: &Path, pages: usize) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let file = std::fs::File::create(path).unwrap();
    let mut zip = zip::ZipWriter::new(file);
    for i in 0..pages {
        let gray = (i * 40) as u8;
        let img = image::RgbImage::from_pixel(8, 8, image::Rgb([gray, gray, gray]));
        let mut buf = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgb8(img)
            .write_to(&mut buf, image::ImageFormat::Png)
            .unwrap();
        zip.start_file(
            format!("{:03}.png", i + 1),
            zip::write::SimpleFileOptions::default(),
        )
        .unwrap();
        zip.write_all(&buf.into_inner()).unwrap();
    }
    zip.finish().unwrap();
}

type DownloadFn = Box<dyn Fn(&Path, &mut dyn FnMut(usize, usize)) -> Result<PathBuf>>;

/// Scriptable gateway. `installed` is interiorly mutable so installs are
/// observable on the refreshed screen.
struct FakeGateway {
    installed: RefCell<Vec<SourceEntry>>,
    available: std::result::Result<Vec<SourceEntry>, String>,
    mangas: std::result::Result<Vec<MangaEntry>, String>,
    chapters: Vec<ChapterEntry>,
    download: Option<DownloadFn>,
    update_message: String,
}

impl Default for FakeGateway {
    fn default() -> Self {
        Self {
            installed: RefCell::new(Vec::new()),
            available: Ok(Vec::new()),
            mangas: Ok(Vec::new()),
            chapters: Vec::new(),
            download: None,
            update_message: "up to date".to_string(),
        }
    }
}

impl SourceGateway for FakeGateway {
    fn installed_sources(&self) -> Result<Vec<SourceEntry>> {
        Ok(self.installed.borrow().clone())
    }

    fn available_sources(&self) -> Result<Vec<SourceEntry>> {
        self.available.clone().map_err(|e| anyhow!(e))
    }

    fn install_source(&self, source_id: &str) -> Result<()> {
        let available = self.available.clone().unwrap_or_default();
        let source = available
            .into_iter()
            .find(|s| s.id == source_id)
            .ok_or_else(|| anyhow!("unknown source {source_id}"))?;
        self.installed.borrow_mut().push(source);
        Ok(())
    }

    fn list_manga(&self, _source_id: &str, _listing: &str) -> Result<Vec<MangaEntry>> {
        self.mangas.clone().map_err(|e| anyhow!(e))
    }

    fn chapters(&self, _source_id: &str, _manga_id: &str) -> Result<Vec<ChapterEntry>> {
        Ok(self.chapters.clone())
    }

    fn download_chapter(
        &self,
        _source_id: &str,
        _manga_id: &str,
        _chapter_id: &str,
        library: &Path,
        progress: &mut dyn FnMut(usize, usize),
    ) -> Result<PathBuf> {
        let download = self
            .download
            .as_ref()
            .ok_or_else(|| anyhow!("no download configured"))?;
        download(library, progress)
    }

    fn check_updates(&self) -> Result<String> {
        Ok(self.update_message.clone())
    }
}

const W: u32 = 600;
const H: u32 = 800;

fn app(
    library: &Path,
    gateway: FakeGateway,
    events: Vec<UiEvent>,
) -> UiApp<MemoryDisplay, FakeInput, FakeGateway> {
    UiApp::new(
        MemoryDisplay::new(W, H),
        FakeInput::new(events),
        gateway,
        library.to_path_buf(),
    )
}

fn layout() -> UiLayout {
    UiLayout::new(W, H)
}

fn tap_row(i: usize) -> UiEvent {
    let l = layout();
    UiEvent::Tap {
        x: l.width / 2,
        y: l.row_top(i) + l.row_h / 2,
    }
}

fn tap_back() -> UiEvent {
    let l = layout();
    UiEvent::Tap {
        x: 1,
        y: l.nav_top() + 1,
    }
}

fn tap_nav_prev() -> UiEvent {
    let l = layout();
    UiEvent::Tap {
        x: l.width / 2,
        y: l.nav_top() + 1,
    }
}

fn tap_nav_next() -> UiEvent {
    let l = layout();
    UiEvent::Tap {
        x: l.width - 2,
        y: l.nav_top() + 1,
    }
}

/// Tap the first cover cell on the library shelf.
fn tap_shelf_cell0() -> UiEvent {
    let l = layout();
    let shelf = ShelfLayout::new(l.width, l.content_height(), SHELF_COLUMNS);
    let (cx, cy) = shelf.cell_origin(0);
    UiEvent::Tap {
        x: cx + shelf.cell_width() / 2,
        y: l.content_top() + cy + shelf.cell_height() / 2,
    }
}

fn reader_tap_next() -> UiEvent {
    UiEvent::Tap { x: W - 1, y: 100 }
}

fn reader_tap_prev() -> UiEvent {
    UiEvent::Tap { x: 0, y: 100 }
}

fn reader_tap_back() -> UiEvent {
    UiEvent::Tap { x: W / 2, y: 100 }
}

// --- tests ---

#[test]
fn home_renders_rows_and_is_not_blank() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app(dir.path(), FakeGateway::default(), vec![]);
    app.run().unwrap();
    assert!(matches!(app.screen(), Screen::Home));
    assert_eq!(app.display().flushes, vec![RefreshMode::Full]);
    assert!(
        app.display().buffer.iter().any(|&p| p < 0x80),
        "home screen is blank"
    );
}

#[test]
fn home_to_library_to_reader_page_turns_and_back() {
    let dir = tempfile::tempdir().unwrap();
    let lib = dir.path().join("Manga");
    make_cbz(&lib.join("Sample/vol1.cbz"), 5);

    let events = vec![
        tap_row(0),        // Home -> Library
        tap_shelf_cell0(), // open the reader
        reader_tap_next(), // page 2
        reader_tap_next(), // page 3
        reader_tap_prev(), // page 2
        reader_tap_back(), // back to Library
        tap_back(),        // back to Home
    ];
    let mut app = app(&lib, FakeGateway::default(), events);
    app.run().unwrap();

    assert!(matches!(app.screen(), Screen::Home));
    // Progress was saved under the library-relative key.
    let store = ProgressStore::load(&progress_path(&lib)).unwrap();
    let progress = store.get("Sample/vol1.cbz").expect("progress saved");
    assert_eq!(progress.current_page, 1); // 0 -> 1 -> 2 -> back to 1
    assert_eq!(progress.total_pages, 5);

    // Screen changes are full refreshes; reader page turns are partial.
    let flushes = &app.display().flushes;
    assert_eq!(flushes[0], RefreshMode::Full); // home
    assert!(flushes.contains(&RefreshMode::Partial)); // page turns
}

#[test]
fn reader_resumes_from_saved_progress() {
    let dir = tempfile::tempdir().unwrap();
    let lib = dir.path().join("Manga");
    make_cbz(&lib.join("Sample/vol1.cbz"), 5);
    let mut store = ProgressStore::default();
    store.update("Sample/vol1.cbz", 3, 5);
    store.save(&progress_path(&lib)).unwrap();

    let events = vec![tap_row(0), tap_shelf_cell0(), reader_tap_next()];
    let mut app = app(&lib, FakeGateway::default(), events);
    app.run().unwrap();

    let store = ProgressStore::load(&progress_path(&lib)).unwrap();
    assert_eq!(store.get("Sample/vol1.cbz").unwrap().current_page, 4);
}

#[test]
fn empty_library_shows_hint_not_error() {
    let dir = tempfile::tempdir().unwrap();
    let lib = dir.path().join("Manga"); // does not exist yet
    let mut app = app(&lib, FakeGateway::default(), vec![tap_row(0)]);
    app.run().unwrap();
    assert!(matches!(app.screen(), Screen::Library { .. }));
    assert!(lib.exists(), "library directory should be created");
}

#[test]
fn library_paginates_with_prev_next() {
    let dir = tempfile::tempdir().unwrap();
    let lib = dir.path().join("Manga");
    let l = layout();
    let capacity = ShelfLayout::new(l.width, l.content_height(), SHELF_COLUMNS).capacity();
    for i in 0..capacity + 2 {
        make_cbz(&lib.join(format!("Series/vol{i:02}.cbz")), 1);
    }

    let events = vec![tap_row(0), tap_nav_next(), tap_nav_next(), tap_nav_prev()];
    let mut app = app(&lib, FakeGateway::default(), events);
    app.run().unwrap();

    // Two pages: next, next (clamped), prev -> page 0.
    let Screen::Library { page, entries } = app.screen() else {
        panic!("expected library screen");
    };
    assert_eq!(entries.len(), capacity + 2);
    assert_eq!(*page, 0);
    // Page flips within a screen are partial refreshes.
    let flushes = &app.display().flushes;
    assert_eq!(
        flushes
            .iter()
            .filter(|m| **m == RefreshMode::Partial)
            .count(),
        2
    );
}

#[test]
fn sources_screen_lists_installed_then_available() {
    let dir = tempfile::tempdir().unwrap();
    let gateway = FakeGateway {
        installed: RefCell::new(vec![SourceEntry {
            id: "a.installed".into(),
            name: "Installed Source".into(),
        }]),
        available: Ok(vec![
            SourceEntry {
                id: "a.installed".into(),
                name: "Installed Source".into(),
            },
            SourceEntry {
                id: "b.new".into(),
                name: "New Source".into(),
            },
        ]),
        ..FakeGateway::default()
    };
    let mut app = app(dir.path(), gateway, vec![tap_row(1)]);
    app.run().unwrap();

    let Screen::Sources { rows, .. } = app.screen() else {
        panic!("expected sources screen");
    };
    assert_eq!(rows.len(), 3);
    assert!(matches!(&rows[0], SourceRow::Installed(s) if s.id == "a.installed"));
    assert!(matches!(&rows[1], SourceRow::Separator(t) if t.contains("available")));
    // Already-installed sources are filtered from the available section.
    assert!(matches!(&rows[2], SourceRow::Available(s) if s.id == "b.new"));
}

#[test]
fn source_list_fetch_error_shows_note_row_and_continues() {
    let dir = tempfile::tempdir().unwrap();
    let gateway = FakeGateway {
        installed: RefCell::new(vec![SourceEntry {
            id: "a".into(),
            name: "A".into(),
        }]),
        available: Err("network unreachable".into()),
        ..FakeGateway::default()
    };
    let mut app = app(dir.path(), gateway, vec![tap_row(1)]);
    app.run().unwrap();

    let Screen::Sources { rows, .. } = app.screen() else {
        panic!("expected sources screen despite fetch error");
    };
    assert!(matches!(&rows[0], SourceRow::Installed(_)));
    assert!(
        matches!(&rows[2], SourceRow::Note(t) if t.contains("network unreachable")),
        "fetch error should be surfaced as a row"
    );
}

#[test]
fn tapping_available_source_installs_and_refreshes() {
    let dir = tempfile::tempdir().unwrap();
    let gateway = FakeGateway {
        available: Ok(vec![SourceEntry {
            id: "b.new".into(),
            name: "New Source".into(),
        }]),
        ..FakeGateway::default()
    };
    // Rows: [Separator, Available("New Source")] -> tap row 1 installs.
    let mut app = app(dir.path(), gateway, vec![tap_row(1), tap_row(1)]);
    app.run().unwrap();

    let Screen::Sources { rows, .. } = app.screen() else {
        panic!("expected sources screen");
    };
    assert!(
        matches!(&rows[0], SourceRow::Installed(s) if s.id == "b.new"),
        "installed source should appear in the installed section after install"
    );
    // And it is no longer offered for install.
    assert!(!rows
        .iter()
        .any(|r| matches!(r, SourceRow::Available(s) if s.id == "b.new")));
}

#[test]
fn full_browse_download_and_read_flow() {
    let dir = tempfile::tempdir().unwrap();
    let lib = dir.path().join("Manga");
    std::fs::create_dir_all(&lib).unwrap();

    let progress_calls = std::rc::Rc::new(RefCell::new(Vec::new()));
    let calls = progress_calls.clone();
    let gateway = FakeGateway {
        installed: RefCell::new(vec![SourceEntry {
            id: "src".into(),
            name: "Src".into(),
        }]),
        mangas: Ok(vec![MangaEntry {
            id: "m1".into(),
            title: "Manga One".into(),
        }]),
        chapters: vec![ChapterEntry {
            id: "c1".into(),
            num: Some(1.0),
            title: Some("Beginnings".into()),
            lang: Some("en".into()),
        }],
        download: Some(Box::new(move |library, progress| {
            // The fake "downloads" by writing a CBZ into the library, the
            // way the real gateway does.
            let path = library.join("Manga One/Chapter 1.cbz");
            make_cbz(&path, 3);
            for i in 0..=3 {
                progress(i, 3);
            }
            calls.borrow_mut().push(3usize);
            Ok(path)
        })),
        ..FakeGateway::default()
    };

    let events = vec![
        tap_row(1),        // Home -> Sources
        tap_row(0),        // installed "Src" -> Listings
        tap_row(0),        // Popular -> MangaList
        tap_row(0),        // Manga One -> ChapterList
        tap_row(0),        // Chapter 1 -> download + Reader
        reader_tap_next(), // page 2
        reader_tap_back(), // back to ChapterList
    ];
    let mut app = app(&lib, gateway, events);
    app.run().unwrap();

    // The CBZ landed in the library and the download closure ran.
    assert!(lib.join("Manga One/Chapter 1.cbz").exists());
    assert_eq!(*progress_calls.borrow(), vec![3]);

    // Reader progress saved under the library-relative key.
    let store = ProgressStore::load(&progress_path(&lib)).unwrap();
    let progress = store.get("Manga One/Chapter 1.cbz").expect("progress");
    assert_eq!(progress.current_page, 1);

    // Back lands on the chapter list.
    let Screen::ChapterList { manga, .. } = app.screen() else {
        panic!("expected chapter list after closing the reader");
    };
    assert_eq!(manga.title, "Manga One");
}

#[test]
fn manga_list_paginates() {
    let dir = tempfile::tempdir().unwrap();
    let per_page = layout().rows_per_page();
    let mangas: Vec<MangaEntry> = (0..per_page * 2 + 3)
        .map(|i| MangaEntry {
            id: format!("m{i}"),
            title: format!("Manga {i}"),
        })
        .collect();
    let gateway = FakeGateway {
        installed: RefCell::new(vec![SourceEntry {
            id: "src".into(),
            name: "Src".into(),
        }]),
        mangas: Ok(mangas),
        ..FakeGateway::default()
    };

    let events = vec![
        tap_row(1),     // Sources
        tap_row(0),     // Listings
        tap_row(0),     // Popular
        tap_nav_next(), // page 2
        tap_nav_next(), // page 3
        tap_nav_next(), // clamped at page 3
        tap_nav_prev(), // page 2
    ];
    let mut app = app(dir.path(), gateway, events);
    app.run().unwrap();

    let Screen::MangaList { page, mangas, .. } = app.screen() else {
        panic!("expected manga list");
    };
    assert_eq!(*page, 1);
    assert_eq!(mangas.len(), per_page * 2 + 3);
}

#[test]
fn listing_failure_shows_error_screen_with_back() {
    let dir = tempfile::tempdir().unwrap();
    let gateway = FakeGateway {
        installed: RefCell::new(vec![SourceEntry {
            id: "src".into(),
            name: "Src".into(),
        }]),
        mangas: Err("server exploded".into()),
        ..FakeGateway::default()
    };

    let events = vec![
        tap_row(1), // Sources
        tap_row(0), // Listings
        tap_row(0), // Popular -> fails
        tap_row(0), // tap the error screen -> back
    ];
    let mut app = app(dir.path(), gateway, events);
    app.run().unwrap();

    // After tapping the error screen we are back on Listings.
    assert!(matches!(app.screen(), Screen::Listings { .. }));
}

#[test]
fn error_screen_renders_the_message() {
    let dir = tempfile::tempdir().unwrap();
    let gateway = FakeGateway {
        installed: RefCell::new(vec![SourceEntry {
            id: "src".into(),
            name: "Src".into(),
        }]),
        mangas: Err("server exploded".into()),
        ..FakeGateway::default()
    };
    let events = vec![tap_row(1), tap_row(0), tap_row(0)];
    let mut app = app(dir.path(), gateway, events);
    app.run().unwrap();

    let Screen::Message { title, body } = app.screen() else {
        panic!("expected error screen");
    };
    assert_eq!(title, "Error");
    assert!(body.contains("server exploded"));
}

#[test]
fn check_updates_shows_message_screen() {
    let dir = tempfile::tempdir().unwrap();
    let gateway = FakeGateway {
        update_message: "gideon 0.1.0 is up to date.".into(),
        ..FakeGateway::default()
    };
    let mut app = app(dir.path(), gateway, vec![tap_row(2)]);
    app.run().unwrap();

    let Screen::Message { title, body } = app.screen() else {
        panic!("expected updates message screen");
    };
    assert_eq!(title, "Updates");
    assert!(body.contains("up to date"));
}

#[test]
fn back_on_home_quits() {
    let dir = tempfile::tempdir().unwrap();
    // Two back taps; the app must quit on the first and not consume more.
    let mut app = app(
        dir.path(),
        FakeGateway::default(),
        vec![tap_back(), tap_back()],
    );
    app.run().unwrap();
    assert!(matches!(app.screen(), Screen::Home));
}

#[test]
fn chapter_labels_format_num_title_lang() {
    let full = ChapterEntry {
        id: "c".into(),
        num: Some(12.5),
        title: Some("The Fall".into()),
        lang: Some("en".into()),
    };
    assert_eq!(full.label(), "Ch 12.5 — The Fall [en]");

    let bare = ChapterEntry {
        id: "c".into(),
        num: None,
        title: None,
        lang: None,
    };
    assert_eq!(bare.label(), "Ch ?");
}
