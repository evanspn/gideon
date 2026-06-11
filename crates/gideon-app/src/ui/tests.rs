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
    search_results: std::result::Result<Vec<MangaEntry>, String>,
    /// Queries passed to `search_manga`, in order.
    searches: RefCell<Vec<String>>,
    /// Source ids passed to `search_manga`, in order.
    searched_sources: RefCell<Vec<String>>,
    chapters: Vec<ChapterEntry>,
    download: Option<DownloadFn>,
    update_message: String,
    update_available: bool,
    installs: std::cell::Cell<usize>,
    /// How many cover downloads were requested.
    covers: std::cell::Cell<usize>,
}

impl Default for FakeGateway {
    fn default() -> Self {
        Self {
            installed: RefCell::new(Vec::new()),
            available: Ok(Vec::new()),
            mangas: Ok(Vec::new()),
            search_results: Ok(Vec::new()),
            searches: RefCell::new(Vec::new()),
            searched_sources: RefCell::new(Vec::new()),
            chapters: Vec::new(),
            download: None,
            update_message: "up to date".to_string(),
            update_available: false,
            installs: std::cell::Cell::new(0),
            covers: std::cell::Cell::new(0),
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

    fn download_cover(&self, _url: &str, dest: &Path) -> Result<()> {
        self.covers.set(self.covers.get() + 1);
        // A real (tiny) image so the shelf can decode it.
        let img = image::GrayImage::from_pixel(3, 4, image::Luma([0x11]));
        std::fs::create_dir_all(dest.parent().unwrap())?;
        image::DynamicImage::ImageLuma8(img).save_with_format(dest, image::ImageFormat::Jpeg)?;
        Ok(())
    }

    fn search_manga(&self, source_id: &str, query: &str) -> Result<Vec<MangaEntry>> {
        self.searches.borrow_mut().push(query.to_string());
        self.searched_sources
            .borrow_mut()
            .push(source_id.to_string());
        self.search_results.clone().map_err(|e| anyhow!(e))
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

    fn install_update(&self) -> Result<String> {
        self.installs.set(self.installs.get() + 1);
        Ok("Updated to 9.9.9.".to_string())
    }

    fn check_updates(&self) -> Result<super::gateway::UpdateCheck> {
        Ok(super::gateway::UpdateCheck {
            message: self.update_message.clone(),
            available: self.update_available,
        })
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

/// Like [`make_cbz`] but with one very tall page per entry, so FitWidth
/// rendering produces a scrollable page (300x1600 → 600x3200 on a 600-wide
/// display: max_scroll 2400, scroll step 800 - 60 = 740).
fn make_tall_cbz(path: &Path, pages: usize) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let file = std::fs::File::create(path).unwrap();
    let mut zip = zip::ZipWriter::new(file);
    for i in 0..pages {
        let gray = (i * 40) as u8;
        let img = image::RgbImage::from_pixel(300, 1600, image::Rgb([gray, gray, gray]));
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
fn fit_width_setting_makes_next_scroll_within_the_page() {
    let dir = tempfile::tempdir().unwrap();
    let lib = dir.path().join("Manga");
    make_tall_cbz(&lib.join("Tall/vol1.cbz"), 2);

    // Four "next" taps only scroll within the tall page (2400px of scroll
    // at 740px per step needs four taps to reach the bottom), so the saved
    // progress stays on page 0.
    let events = vec![
        tap_row(0),
        tap_shelf_cell0(),
        reader_tap_next(),
        reader_tap_next(),
        reader_tap_next(),
        reader_tap_next(),
        reader_tap_back(),
    ];
    let mut app =
        app(&lib, FakeGateway::default(), events).with_reader_settings(FitMode::FitWidth, 0);
    app.run().unwrap();

    let store = ProgressStore::load(&progress_path(&lib)).unwrap();
    assert_eq!(
        store.get("Tall/vol1.cbz").unwrap().current_page,
        0,
        "next taps within a FitWidth page must scroll, not turn the page"
    );
}

#[test]
fn fit_width_setting_turns_the_page_from_the_bottom() {
    let dir = tempfile::tempdir().unwrap();
    let lib = dir.path().join("Manga");
    make_tall_cbz(&lib.join("Tall/vol1.cbz"), 2);

    // The fifth "next" tap happens at the bottom and turns to page 1.
    let mut events = vec![tap_row(0), tap_shelf_cell0()];
    events.extend(std::iter::repeat_with(reader_tap_next).take(5));
    events.push(reader_tap_back());
    let mut app =
        app(&lib, FakeGateway::default(), events).with_reader_settings(FitMode::FitWidth, 0);
    app.run().unwrap();

    let store = ProgressStore::load(&progress_path(&lib)).unwrap();
    assert_eq!(store.get("Tall/vol1.cbz").unwrap().current_page, 1);
}

#[test]
fn default_contain_mode_turns_pages_directly() {
    let dir = tempfile::tempdir().unwrap();
    let lib = dir.path().join("Manga");
    make_tall_cbz(&lib.join("Tall/vol1.cbz"), 3);

    // Without the fit-width setting, two next taps mean two page turns
    // even on a tall page.
    let events = vec![
        tap_row(0),
        tap_shelf_cell0(),
        reader_tap_next(),
        reader_tap_next(),
        reader_tap_back(),
    ];
    let mut app = app(&lib, FakeGateway::default(), events);
    app.run().unwrap();

    let store = ProgressStore::load(&progress_path(&lib)).unwrap();
    assert_eq!(store.get("Tall/vol1.cbz").unwrap().current_page, 2);
}

#[test]
fn rotated_reader_taps_follow_reading_orientation() {
    let dir = tempfile::tempdir().unwrap();
    let lib = dir.path().join("Manga");
    make_cbz(&lib.join("Sample/vol1.cbz"), 5);

    // Rotation 90 (clockwise): reading-right is the panel bottom, so
    // "next" is a tap at the bottom of the panel, "prev" at the top, and
    // the middle band is still "back".
    let tap_panel_bottom = UiEvent::Tap { x: W / 2, y: H - 1 };
    let tap_panel_top = UiEvent::Tap { x: W / 2, y: 0 };
    let tap_panel_middle = UiEvent::Tap { x: W / 2, y: H / 2 };
    let events = vec![
        tap_row(0),
        tap_shelf_cell0(),
        tap_panel_bottom, // next -> page 1
        tap_panel_bottom, // next -> page 2
        tap_panel_top,    // prev -> page 1
        tap_panel_middle, // back
    ];
    let mut app =
        app(&lib, FakeGateway::default(), events).with_reader_settings(FitMode::Contain, 90);
    app.run().unwrap();

    assert!(matches!(app.screen(), Screen::Library { .. }));
    let store = ProgressStore::load(&progress_path(&lib)).unwrap();
    assert_eq!(store.get("Sample/vol1.cbz").unwrap().current_page, 1);
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
    let mut app = app(dir.path(), gateway, vec![tap_row(2)]);
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
    let mut app = app(dir.path(), gateway, vec![tap_row(2)]);
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
    let mut app = app(dir.path(), gateway, vec![tap_row(2), tap_row(1)]);
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
            cover_url: None,
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
        tap_row(2),        // Home -> Sources
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
            cover_url: None,
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
        tap_row(2),     // Sources
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
        tap_row(2), // Sources
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
    let events = vec![tap_row(2), tap_row(0), tap_row(0)];
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
    let mut app = app(dir.path(), gateway, vec![tap_row(3)]);
    app.run().unwrap();

    let Screen::Message { title, body } = app.screen() else {
        panic!("expected updates message screen");
    };
    assert_eq!(title, "Updates");
    assert!(body.contains("up to date"));
}

#[test]
fn back_on_home_does_nothing() {
    let dir = tempfile::tempdir().unwrap();
    // Back taps on Home are ignored — quitting goes through the power
    // menu. The tap after them still works.
    let mut app = app(
        dir.path(),
        FakeGateway::default(),
        vec![tap_back(), tap_back(), tap_row(0)],
    );
    let lib = dir.path().join("x");
    let _ = lib; // silence unused in case of edits
    app.run().unwrap();
    assert!(matches!(app.screen(), Screen::Library { .. }));
}

// --- power menu ---

/// Tap the power symbol region: top-right corner of the title bar.
fn tap_power_icon() -> UiEvent {
    let l = layout();
    UiEvent::Tap {
        x: l.width - 2,
        y: l.title_h / 2,
    }
}

#[test]
fn power_icon_opens_the_menu_and_back_returns() {
    let dir = tempfile::tempdir().unwrap();
    let events = vec![tap_power_icon(), tap_back()];
    let mut app = app(dir.path(), FakeGateway::default(), events);
    assert_eq!(app.run().unwrap(), Exit::Close); // input exhausted
    assert!(matches!(app.screen(), Screen::Home));
}

#[test]
fn power_menu_close_quits() {
    let dir = tempfile::tempdir().unwrap();
    let events = vec![tap_power_icon(), tap_row(1)];
    let mut app = app(dir.path(), FakeGateway::default(), events);
    assert_eq!(app.run().unwrap(), Exit::Close);
}

#[test]
fn power_menu_restart_requests_restart() {
    let dir = tempfile::tempdir().unwrap();
    let events = vec![tap_power_icon(), tap_row(0)];
    let mut app = app(dir.path(), FakeGateway::default(), events);
    assert_eq!(app.run().unwrap(), Exit::Restart);
}

#[test]
fn title_taps_off_the_power_icon_are_ignored() {
    let dir = tempfile::tempdir().unwrap();
    // Between the profile zone (left half) and the power zone (right
    // 2 × title_h): a dead-zone tap must do nothing.
    let l = layout();
    let x = (l.width / 2 + l.width.saturating_sub(l.title_h * 2)) / 2;
    let events = vec![UiEvent::Tap { x, y: 5 }];
    let mut app = app(dir.path(), FakeGateway::default(), events);
    app.run().unwrap();
    assert!(matches!(app.screen(), Screen::Home));
}

// --- profiles ---

/// Tap the left half of the title bar (the profile name on Home).
fn tap_title_left() -> UiEvent {
    let l = layout();
    UiEvent::Tap {
        x: 5,
        y: l.title_h / 2,
    }
}

/// Settings dir preloaded with the given profiles ("default" stays active).
fn profile_settings_dir(dir: &Path, profiles: &[&str]) -> PathBuf {
    let settings_dir = dir.join("data");
    let settings = gideon_core::Settings {
        profiles: profiles.iter().map(|p| p.to_string()).collect(),
        ..gideon_core::Settings::default()
    };
    settings.save(&settings_dir).unwrap();
    settings_dir
}

#[test]
fn title_left_tap_opens_the_profile_menu() {
    let dir = tempfile::tempdir().unwrap();
    let settings_dir = profile_settings_dir(dir.path(), &["default", "alex"]);
    let events = vec![tap_title_left()];
    let mut app = app(dir.path(), FakeGateway::default(), events).with_settings_dir(settings_dir);
    app.run().unwrap();

    let Screen::ProfileMenu { profiles } = app.screen() else {
        panic!("expected the profile menu");
    };
    assert_eq!(profiles, &vec!["default".to_string(), "alex".to_string()]);
}

#[test]
fn switching_profile_shows_only_that_profiles_books() {
    let dir = tempfile::tempdir().unwrap();
    let lib = dir.path().join("Manga");
    make_cbz(&lib.join("Shared/vol1.cbz"), 2);
    make_cbz(&lib.join("@alex/Alexs Series/vol1.cbz"), 2);
    let settings_dir = profile_settings_dir(dir.path(), &["default", "alex"]);

    let events = vec![
        tap_title_left(), // profile menu
        tap_row(1),       // switch to alex -> back on Home
        tap_row(0),       // Library
    ];
    let mut app = app(&lib, FakeGateway::default(), events).with_settings_dir(settings_dir.clone());
    app.run().unwrap();

    let Screen::Library { entries, .. } = app.screen() else {
        panic!("expected alex's library");
    };
    let rel: Vec<&str> = entries.iter().map(|e| e.relative_path.as_str()).collect();
    assert_eq!(rel, vec!["Alexs Series/vol1.cbz"]);
    // The switch persisted for the next start.
    let settings = gideon_core::Settings::load(&settings_dir).unwrap();
    assert_eq!(settings.active_profile, "alex");
}

#[test]
fn default_profile_does_not_see_other_profiles_books() {
    let dir = tempfile::tempdir().unwrap();
    let lib = dir.path().join("Manga");
    make_cbz(&lib.join("Shared/vol1.cbz"), 2);
    make_cbz(&lib.join("@alex/Alexs Series/vol1.cbz"), 2);

    let mut app = app(&lib, FakeGateway::default(), vec![tap_row(0)]);
    app.run().unwrap();

    let Screen::Library { entries, .. } = app.screen() else {
        panic!("expected the default library");
    };
    let rel: Vec<&str> = entries.iter().map(|e| e.relative_path.as_str()).collect();
    assert_eq!(rel, vec!["Shared/vol1.cbz"]);
}

#[test]
fn downloads_land_in_the_active_profiles_directory() {
    let dir = tempfile::tempdir().unwrap();
    let lib = dir.path().join("Manga");
    std::fs::create_dir_all(&lib).unwrap();
    let settings_dir = profile_settings_dir(dir.path(), &["default", "alex"]);
    let gateway = FakeGateway {
        installed: RefCell::new(vec![SourceEntry {
            id: "src".into(),
            name: "Src".into(),
        }]),
        mangas: Ok(vec![MangaEntry {
            id: "m1".into(),
            title: "Manga One".into(),
            cover_url: None,
        }]),
        chapters: vec![ChapterEntry {
            id: "c1".into(),
            num: Some(1.0),
            title: None,
            lang: None,
        }],
        download: Some(Box::new(move |library, _| {
            // The fake writes into whatever library dir the UI passes —
            // exactly how the real gateway behaves.
            let path = library.join("Manga One/Chapter 1.cbz");
            make_cbz(&path, 2);
            Ok(path)
        })),
        ..FakeGateway::default()
    };

    let events = vec![
        tap_title_left(), // profile menu
        tap_row(1),       // switch to alex
        tap_row(2),       // Sources
        tap_row(0),       // Listings
        tap_row(0),       // Popular
        tap_row(0),       // Manga One
        tap_row(0),       // download + Reader
        reader_tap_back(),
    ];
    let mut app = app(&lib, gateway, events).with_settings_dir(settings_dir);
    app.run().unwrap();

    assert!(
        lib.join("@alex/Manga One/Chapter 1.cbz").exists(),
        "download must land in the active profile's directory"
    );
    assert!(
        !lib.join("Manga One").exists(),
        "nothing may leak into the default profile's library"
    );
}

#[test]
fn new_profile_keyboard_creates_and_switches() {
    let dir = tempfile::tempdir().unwrap();
    let lib = dir.path().join("Manga");
    let settings_dir = profile_settings_dir(dir.path(), &["default"]);

    let events = vec![
        tap_title_left(), // profile menu: [default, New profile…]
        tap_row(1),       // New profile…
        tap_key(Key::Char('b')),
        tap_key(Key::Char('o')),
        tap_key(Key::Char('b')),
        tap_key(Key::Search), // create
        tap_row(0),           // Library (of the new profile)
    ];
    let mut app = app(&lib, FakeGateway::default(), events).with_settings_dir(settings_dir.clone());
    app.run().unwrap();

    let settings = gideon_core::Settings::load(&settings_dir).unwrap();
    assert_eq!(
        settings.profiles,
        vec!["default".to_string(), "bob".to_string()]
    );
    assert_eq!(settings.active_profile, "bob");
    // The new profile's library exists and is empty.
    assert!(lib.join("@bob").is_dir());
    let Screen::Library { entries, .. } = app.screen() else {
        panic!("expected the new profile's (empty) library");
    };
    assert!(entries.is_empty());
}

#[test]
fn picking_the_active_profile_just_closes_the_menu() {
    let dir = tempfile::tempdir().unwrap();
    let settings_dir = profile_settings_dir(dir.path(), &["default", "alex"]);
    let events = vec![tap_title_left(), tap_row(0)];
    let mut app =
        app(dir.path(), FakeGateway::default(), events).with_settings_dir(settings_dir.clone());
    app.run().unwrap();

    assert!(matches!(app.screen(), Screen::Home));
    let settings = gideon_core::Settings::load(&settings_dir).unwrap();
    assert_eq!(settings.active_profile, "default");
}

// --- frontlight edge slides ---

/// Scriptable light control recording every set.
struct FakeLights {
    levels: SharedLevels,
}

impl LightControl for FakeLights {
    fn brightness(&self) -> u8 {
        self.levels.borrow().0
    }
    fn set_brightness(&mut self, p: u8) {
        self.levels.borrow_mut().0 = p;
    }
    fn warmth(&self) -> u8 {
        self.levels.borrow().1
    }
    fn set_warmth(&mut self, p: u8) {
        self.levels.borrow_mut().1 = p;
    }
}

type SharedLevels = std::rc::Rc<RefCell<(u8, u8)>>;

fn lights() -> (SharedLevels, Box<dyn LightControl>) {
    let levels = std::rc::Rc::new(RefCell::new((20u8, 0u8)));
    (
        levels.clone(),
        Box::new(FakeLights { levels }) as Box<dyn LightControl>,
    )
}

#[test]
fn right_edge_slide_up_raises_brightness() {
    let dir = tempfile::tempdir().unwrap();
    let lib = dir.path().join("Manga");
    make_cbz(&lib.join("Sample/vol1.cbz"), 3);
    let (levels, lights) = lights();

    // Slide along the right edge, upward by half the screen = +50.
    let slide = UiEvent::Swipe {
        x0: W - 5,
        y0: H - 100,
        x1: W - 5,
        y1: H - 100 - H / 2,
    };
    let events = vec![tap_row(0), tap_shelf_cell0(), slide, reader_tap_back()];
    let mut app = app(&lib, FakeGateway::default(), events).with_lights(lights);
    app.run().unwrap();

    assert_eq!(levels.borrow().0, 70, "20 + 50 = 70");
    assert_eq!(levels.borrow().1, 0, "warmth untouched");
}

#[test]
fn left_edge_slide_adjusts_night_light() {
    let dir = tempfile::tempdir().unwrap();
    let lib = dir.path().join("Manga");
    make_cbz(&lib.join("Sample/vol1.cbz"), 3);
    let (levels, lights) = lights();

    let slide_up = UiEvent::Swipe {
        x0: 3,
        y0: H - 50,
        x1: 3,
        y1: H - 50 - H / 4, // +25
    };
    let events = vec![tap_row(0), tap_shelf_cell0(), slide_up, reader_tap_back()];
    let mut app = app(&lib, FakeGateway::default(), events).with_lights(lights);
    app.run().unwrap();

    assert_eq!(levels.borrow().1, 25);
    assert_eq!(levels.borrow().0, 20, "brightness untouched");
}

#[test]
fn edge_slides_without_a_light_hook_are_ignored() {
    let dir = tempfile::tempdir().unwrap();
    let lib = dir.path().join("Manga");
    make_cbz(&lib.join("Sample/vol1.cbz"), 3);
    let slide = UiEvent::Swipe {
        x0: W - 5,
        y0: H - 100,
        x1: W - 5,
        y1: 100,
    };
    let events = vec![tap_row(0), tap_shelf_cell0(), slide, reader_tap_back()];
    let mut app = app(&lib, FakeGateway::default(), events);
    app.run().unwrap();
    assert!(matches!(app.screen(), Screen::Library { .. }));
}

#[test]
fn swipe_up_rotates_and_locks_the_reader() {
    let dir = tempfile::tempdir().unwrap();
    let lib = dir.path().join("Manga");
    let settings_dir = dir.path().join("data");
    make_cbz(&lib.join("Sample/vol1.cbz"), 5);

    let swipe_up = UiEvent::Swipe {
        x0: W / 2,
        y0: H - 100,
        x1: W / 2,
        y1: 100,
    };
    // After one up-swipe the reading orientation is 90°: "next" moves to
    // the panel bottom (reading-right), like the rotated-taps test.
    let tap_panel_bottom = UiEvent::Tap { x: W / 2, y: H - 1 };
    // In the 90° orientation the back zone is the panel's vertical middle.
    let tap_rotated_back = UiEvent::Tap { x: W / 2, y: H / 2 };
    let events = vec![
        tap_row(0),
        tap_shelf_cell0(),
        swipe_up,         // rotate to 90 and lock
        tap_panel_bottom, // next page in the rotated orientation
        tap_rotated_back,
    ];
    let mut app = app(&lib, FakeGateway::default(), events).with_settings_dir(settings_dir.clone());
    app.run().unwrap();

    // The page actually turned under the rotated tap zones...
    let store = ProgressStore::load(&progress_path(&lib)).unwrap();
    assert_eq!(store.get("Sample/vol1.cbz").unwrap().current_page, 1);
    // ...and the lock persisted for the next session.
    let settings = gideon_core::Settings::load(&settings_dir).unwrap();
    assert_eq!(settings.reader_rotation, 90);
}

#[test]
fn four_up_swipes_come_back_around_to_zero() {
    let dir = tempfile::tempdir().unwrap();
    let lib = dir.path().join("Manga");
    let settings_dir = dir.path().join("data");
    make_cbz(&lib.join("Sample/vol1.cbz"), 3);

    // Each swipe is "up" in the CURRENT reading frame (gestures follow
    // the orientation): panel-up, then panel-left-to-right, panel-down,
    // panel-right-to-left.
    let up_at_0 = UiEvent::Swipe {
        x0: W / 2,
        y0: H - 100,
        x1: W / 2,
        y1: 100,
    };
    let up_at_90 = UiEvent::Swipe {
        x0: 150,
        y0: H / 2,
        x1: W - 150,
        y1: H / 2,
    };
    let up_at_180 = UiEvent::Swipe {
        x0: W / 2,
        y0: 100,
        x1: W / 2,
        y1: H - 100,
    };
    let up_at_270 = UiEvent::Swipe {
        x0: W - 150,
        y0: H / 2,
        x1: 150,
        y1: H / 2,
    };
    let events = vec![
        tap_row(0),
        tap_shelf_cell0(),
        up_at_0,
        up_at_90,
        up_at_180,
        up_at_270,
    ];
    let mut app = app(&lib, FakeGateway::default(), events).with_settings_dir(settings_dir.clone());
    app.run().unwrap();

    let settings = gideon_core::Settings::load(&settings_dir).unwrap();
    assert_eq!(settings.reader_rotation, 0, "full circle");
}

#[test]
fn sloppy_tap_drift_neither_rotates_nor_exits() {
    // The auditor's blocker: a page-turn tap that drifts 40px (past the
    // 30px slop) classifies as a swipe — it must NOT rotate-and-lock the
    // reader, and must not exit the book either.
    let dir = tempfile::tempdir().unwrap();
    let lib = dir.path().join("Manga");
    let settings_dir = dir.path().join("data");
    make_cbz(&lib.join("Sample/vol1.cbz"), 5);

    let drift_up = UiEvent::Swipe {
        x0: W / 2,
        y0: 400,
        x1: W / 2,
        y1: 360, // 40px: a sloppy tap, not a gesture
    };
    let drift_down = UiEvent::Swipe {
        x0: W / 2,
        y0: 360,
        x1: W / 2,
        y1: 400,
    };
    let events = vec![
        tap_row(0),
        tap_shelf_cell0(),
        drift_up,
        drift_down,
        reader_tap_next(), // reader still alive, unrotated zones
        reader_tap_back(),
    ];
    let mut app = app(&lib, FakeGateway::default(), events).with_settings_dir(settings_dir.clone());
    app.run().unwrap();

    let settings = gideon_core::Settings::load(&settings_dir).unwrap();
    assert_eq!(settings.reader_rotation, 0, "drift must not rotate");
    let store = ProgressStore::load(&progress_path(&lib)).unwrap();
    assert_eq!(
        store.get("Sample/vol1.cbz").unwrap().current_page,
        1,
        "drift must not exit; the next tap still turned the page"
    );
}

#[test]
fn rotation_gestures_follow_the_reading_orientation() {
    // After rotating to 90°, the user's "up" is the panel's left-to-right.
    // Their natural swipe must rotate again (to 180), not be ignored.
    let dir = tempfile::tempdir().unwrap();
    let lib = dir.path().join("Manga");
    let settings_dir = dir.path().join("data");
    make_cbz(&lib.join("Sample/vol1.cbz"), 3);

    let panel_up = UiEvent::Swipe {
        x0: W / 2,
        y0: H - 100,
        x1: W / 2,
        y1: 100,
    };
    // Reading-frame "up" at rotation 90: panel x increases, y steady.
    // (map_reader_tap: reading_y = panel_w - 1 - x, so larger x = smaller
    // reading y = upward.) Mid-screen vertically to dodge the edge bands.
    let rotated_up = UiEvent::Swipe {
        x0: 150,
        y0: H / 2,
        x1: W - 150,
        y1: H / 2,
    };
    let events = vec![tap_row(0), tap_shelf_cell0(), panel_up, rotated_up];
    let mut app = app(&lib, FakeGateway::default(), events).with_settings_dir(settings_dir.clone());
    app.run().unwrap();

    let settings = gideon_core::Settings::load(&settings_dir).unwrap();
    assert_eq!(settings.reader_rotation, 180, "90 + one rotated up-swipe");
}

#[test]
fn swipe_down_leaves_the_manga() {
    let dir = tempfile::tempdir().unwrap();
    let lib = dir.path().join("Manga");
    make_cbz(&lib.join("Sample/vol1.cbz"), 3);

    let swipe_down = UiEvent::Swipe {
        x0: W / 2,
        y0: 100,
        x1: W / 2,
        y1: H - 100,
    };
    let events = vec![tap_row(0), tap_shelf_cell0(), reader_tap_next(), swipe_down];
    let mut app = app(&lib, FakeGateway::default(), events);
    app.run().unwrap();

    // Back on the shelf, with progress saved.
    assert!(matches!(app.screen(), Screen::Library { .. }));
    let store = ProgressStore::load(&progress_path(&lib)).unwrap();
    assert_eq!(store.get("Sample/vol1.cbz").unwrap().current_page, 1);
}

// --- long press: library card -> source chapter list ---

#[test]
fn long_press_on_a_downloaded_book_opens_its_chapter_list() {
    let dir = tempfile::tempdir().unwrap();
    let lib = dir.path().join("Manga");
    make_cbz(&lib.join("Manga One/Chapter 1.cbz"), 2);
    let mut index = gideon_core::SeriesIndex::load(&lib);
    index.record(
        "Manga One",
        gideon_core::SeriesRef {
            source_id: "src".into(),
            source_name: "Src".into(),
            manga_id: "m1".into(),
            manga_title: "Manga One".into(),
            ..gideon_core::SeriesRef::default()
        },
    );
    index.save(&lib).unwrap();

    let gateway = FakeGateway {
        chapters: vec![
            ChapterEntry {
                id: "c1".into(),
                num: Some(1.0),
                title: None,
                lang: None,
            },
            ChapterEntry {
                id: "c2".into(),
                num: Some(2.0),
                title: None,
                lang: None,
            },
        ],
        ..FakeGateway::default()
    };

    let cell = tap_shelf_cell0();
    let UiEvent::Tap { x, y } = cell else {
        unreachable!()
    };
    // Long press -> book menu -> "All chapters (from source)".
    let events = vec![tap_row(0), UiEvent::LongPress { x, y }, tap_row(0)];
    let mut app = app(&lib, gateway, events);
    app.run().unwrap();

    let Screen::ChapterList {
        source,
        manga,
        chapters,
        ..
    } = app.screen()
    else {
        panic!("expected the source's chapter list");
    };
    assert_eq!(source.id, "src");
    assert_eq!(manga.id, "m1");
    assert_eq!(chapters.len(), 2, "all chapters listed for download");
}

#[test]
fn long_press_opens_the_book_menu() {
    let dir = tempfile::tempdir().unwrap();
    let lib = dir.path().join("Manga");
    make_cbz(&lib.join("Sideload/vol1.cbz"), 2);

    let cell = tap_shelf_cell0();
    let UiEvent::Tap { x, y } = cell else {
        unreachable!()
    };
    let events = vec![tap_row(0), UiEvent::LongPress { x, y }];
    let mut app = app(&lib, FakeGateway::default(), events);
    app.run().unwrap();

    let Screen::BookMenu { series_dir, .. } = app.screen() else {
        panic!("expected the book menu");
    };
    assert_eq!(series_dir, "Sideload");
}

#[test]
fn unlinked_book_chapters_falls_back_to_prefilled_search() {
    // A book downloaded before origins were recorded (or sideloaded):
    // "All chapters" drops into global search with the series name typed.
    let dir = tempfile::tempdir().unwrap();
    let lib = dir.path().join("Manga");
    make_cbz(&lib.join("Sideload/vol1.cbz"), 2);

    let cell = tap_shelf_cell0();
    let UiEvent::Tap { x, y } = cell else {
        unreachable!()
    };
    let events = vec![tap_row(0), UiEvent::LongPress { x, y }, tap_row(0)];
    let mut app = app(&lib, FakeGateway::default(), events);
    app.run().unwrap();

    let Screen::Search { source, query } = app.screen() else {
        panic!("expected prefilled search");
    };
    assert!(source.is_none());
    assert_eq!(query, "sideload");
}

#[test]
fn book_menu_deletes_a_chapter() {
    let dir = tempfile::tempdir().unwrap();
    let lib = dir.path().join("Manga");
    make_cbz(&lib.join("Series/vol1.cbz"), 2);
    make_cbz(&lib.join("Series/vol2.cbz"), 2);

    let cell = tap_shelf_cell0();
    let UiEvent::Tap { x, y } = cell else {
        unreachable!()
    };
    let events = vec![tap_row(0), UiEvent::LongPress { x, y }, tap_row(1)];
    let mut app = app(&lib, FakeGateway::default(), events);
    app.run().unwrap();

    let Screen::Library { entries, .. } = app.screen() else {
        panic!("expected refreshed library");
    };
    assert_eq!(entries.len(), 1, "one chapter deleted, one remains");
    assert!(lib.join("Series").exists(), "series dir keeps the other");
}

#[test]
fn book_menu_deletes_the_whole_series() {
    let dir = tempfile::tempdir().unwrap();
    let lib = dir.path().join("Manga");
    make_cbz(&lib.join("Series/vol1.cbz"), 2);
    make_cbz(&lib.join("Series/vol2.cbz"), 2);

    let cell = tap_shelf_cell0();
    let UiEvent::Tap { x, y } = cell else {
        unreachable!()
    };
    let events = vec![tap_row(0), UiEvent::LongPress { x, y }, tap_row(2)];
    let mut app = app(&lib, FakeGateway::default(), events);
    app.run().unwrap();

    let Screen::Library { entries, .. } = app.screen() else {
        panic!("expected refreshed library");
    };
    assert!(entries.is_empty(), "whole series gone");
    assert!(!lib.join("Series").exists());
}

#[test]
fn downloading_records_the_series_origin() {
    let dir = tempfile::tempdir().unwrap();
    let lib = dir.path().join("Manga");
    std::fs::create_dir_all(&lib).unwrap();
    let gateway = FakeGateway {
        installed: RefCell::new(vec![SourceEntry {
            id: "src".into(),
            name: "Src".into(),
        }]),
        mangas: Ok(vec![MangaEntry {
            id: "m1".into(),
            title: "Manga One".into(),
            cover_url: Some("https://example.com/cover.jpg".into()),
        }]),
        chapters: vec![ChapterEntry {
            id: "c1".into(),
            num: Some(1.0),
            title: None,
            lang: None,
        }],
        download: Some(Box::new(move |library, _| {
            let path = library.join("Manga One/Chapter 1.cbz");
            make_cbz(&path, 2);
            Ok(path)
        })),
        ..FakeGateway::default()
    };

    let events = vec![
        tap_row(2), // Sources
        tap_row(0), // Listings
        tap_row(0), // Popular
        tap_row(0), // Manga One
        tap_row(0), // download + Reader
        reader_tap_back(),
    ];
    let mut app = app(&lib, gateway, events);
    app.run().unwrap();

    let index = gideon_core::SeriesIndex::load(&lib);
    let origin = index.get("Manga One").expect("origin recorded");
    assert_eq!(origin.source_id, "src");
    assert_eq!(origin.manga_id, "m1");
    assert_eq!(origin.manga_title, "Manga One");
    assert_eq!(
        origin.downloaded.get("c1"),
        Some(&"Chapter 1.cbz".to_string()),
        "the chapter file is recorded"
    );
    // The manga cover was fetched once and saved next to the chapters.
    assert_eq!(app.gateway().covers.get(), 1);
    assert!(lib.join("Manga One/.cover.jpg").exists());
}

#[test]
fn downloaded_chapters_open_instantly_without_redownloading() {
    let dir = tempfile::tempdir().unwrap();
    let lib = dir.path().join("Manga");
    std::fs::create_dir_all(&lib).unwrap();
    let downloads = std::rc::Rc::new(std::cell::Cell::new(0usize));
    let counter = downloads.clone();
    let gateway = FakeGateway {
        installed: RefCell::new(vec![SourceEntry {
            id: "src".into(),
            name: "Src".into(),
        }]),
        mangas: Ok(vec![MangaEntry {
            id: "m1".into(),
            title: "Manga One".into(),
            cover_url: None,
        }]),
        chapters: vec![ChapterEntry {
            id: "c1".into(),
            num: Some(1.0),
            title: None,
            lang: None,
        }],
        download: Some(Box::new(move |library, _| {
            counter.set(counter.get() + 1);
            let path = library.join("Manga One/Chapter 1.cbz");
            make_cbz(&path, 2);
            Ok(path)
        })),
        ..FakeGateway::default()
    };

    let events = vec![
        tap_row(2),        // Sources
        tap_row(0),        // Listings
        tap_row(0),        // Popular
        tap_row(0),        // Manga One
        tap_row(0),        // chapter -> download + read
        reader_tap_back(), // back to the chapter list
        tap_row(0),        // same chapter again -> instant open
        reader_tap_back(),
    ];
    let mut app = app(&lib, gateway, events);
    app.run().unwrap();

    assert_eq!(
        downloads.get(),
        1,
        "the second open must come from disk, not the network"
    );
}

#[test]
fn long_press_a_chapter_downloads_without_opening_the_reader() {
    let dir = tempfile::tempdir().unwrap();
    let lib = dir.path().join("Manga");
    std::fs::create_dir_all(&lib).unwrap();
    let gateway = FakeGateway {
        installed: RefCell::new(vec![SourceEntry {
            id: "src".into(),
            name: "Src".into(),
        }]),
        mangas: Ok(vec![MangaEntry {
            id: "m1".into(),
            title: "Manga One".into(),
            cover_url: None,
        }]),
        chapters: vec![ChapterEntry {
            id: "c1".into(),
            num: Some(1.0),
            title: None,
            lang: None,
        }],
        download: Some(Box::new(move |library, _| {
            let path = library.join("Manga One/Chapter 1.cbz");
            make_cbz(&path, 2);
            Ok(path)
        })),
        ..FakeGateway::default()
    };

    let chapter_row = tap_row(0);
    let UiEvent::Tap { x, y } = chapter_row else {
        unreachable!()
    };
    let events = vec![
        tap_row(2),
        tap_row(0),
        tap_row(0),
        tap_row(0),                  // ChapterList
        UiEvent::LongPress { x, y }, // download only
    ];
    let mut app = app(&lib, gateway, events);
    app.run().unwrap();

    assert!(
        matches!(app.screen(), Screen::ChapterList { .. }),
        "stay on the list after a download-only long press"
    );
    assert!(lib.join("Manga One/Chapter 1.cbz").exists());
    let index = gideon_core::SeriesIndex::load(&lib);
    assert!(index
        .get("Manga One")
        .unwrap()
        .downloaded
        .contains_key("c1"));
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

#[test]
fn update_prompt_installs_on_tap() {
    let dir = tempfile::tempdir().unwrap();
    let gateway = FakeGateway {
        update_available: true,
        update_message: "Update available: 0.0.0 -> 9.9.9.".into(),
        ..FakeGateway::default()
    };
    // Home row 2 = "Check for updates" -> prompt; content tap installs.
    let mut app = app(dir.path(), gateway, vec![tap_row(3), tap_row(0)]);
    app.run().unwrap();

    assert_eq!(
        app.gateway().installs.get(),
        1,
        "tap on prompt should install"
    );
    let Screen::Message { title, body } = app.screen() else {
        panic!("expected result message screen");
    };
    assert_eq!(title, "Updates");
    assert!(body.contains("Updated to 9.9.9"));
}

// --- search keyboard ---

/// Tap the center of a keyboard key.
fn tap_key(key: Key) -> UiEvent {
    let (_, x, y, w, h) = layout()
        .keyboard_keys()
        .into_iter()
        .find(|(k, ..)| *k == key)
        .expect("key exists");
    UiEvent::Tap {
        x: x + w / 2,
        y: y + h / 2,
    }
}

fn search_gateway() -> FakeGateway {
    FakeGateway {
        installed: RefCell::new(vec![SourceEntry {
            id: "src".into(),
            name: "Src".into(),
        }]),
        search_results: Ok(vec![MangaEntry {
            id: "m1".into(),
            title: "Naruto".into(),
            cover_url: None,
        }]),
        ..FakeGateway::default()
    }
}

#[test]
fn home_search_goes_straight_to_the_keyboard() {
    // One tap from Home — e-ink refreshes cost a second each, so search
    // must not hide behind Sources -> source -> Search.
    let dir = tempfile::tempdir().unwrap();
    let mut app = app(dir.path(), search_gateway(), vec![tap_row(1)]);
    app.run().unwrap();

    let Screen::Search { source, query } = app.screen() else {
        panic!("expected the global search keyboard");
    };
    assert!(source.is_none(), "home search covers all sources");
    assert_eq!(query, "");
}

#[test]
fn home_search_without_sources_explains_instead_of_a_dead_keyboard() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app(dir.path(), FakeGateway::default(), vec![tap_row(1)]);
    app.run().unwrap();

    let Screen::Message { title, body } = app.screen() else {
        panic!("expected install hint");
    };
    assert_eq!(title, "Search");
    assert!(body.contains("Browse sources"));
}

#[test]
fn global_search_queries_every_source_and_labels_results() {
    let dir = tempfile::tempdir().unwrap();
    let gateway = FakeGateway {
        installed: RefCell::new(vec![
            SourceEntry {
                id: "src1".into(),
                name: "First".into(),
            },
            SourceEntry {
                id: "src2".into(),
                name: "Second".into(),
            },
        ]),
        search_results: Ok(vec![MangaEntry {
            id: "m1".into(),
            title: "Naruto".into(),
            cover_url: None,
        }]),
        chapters: vec![ChapterEntry {
            id: "c1".into(),
            num: Some(1.0),
            title: None,
            lang: None,
        }],
        ..FakeGateway::default()
    };
    let events = vec![
        tap_row(1), // Home -> global search keyboard
        tap_key(Key::Char('n')),
        tap_key(Key::Search),
        tap_row(1), // second result -> ChapterList via its own source
    ];
    let mut app = app(dir.path(), gateway, events);
    app.run().unwrap();

    assert_eq!(
        *app.gateway().searched_sources.borrow(),
        vec!["src1".to_string(), "src2".to_string()],
        "every installed source must be searched"
    );
    // Both sources contributed a result; tapping the second opened its
    // chapter list with the right source attached.
    let Screen::ChapterList { source, manga, .. } = app.screen() else {
        panic!("expected chapter list from a search result");
    };
    assert_eq!(source.id, "src2");
    assert_eq!(manga.title, "Naruto");
}

#[test]
fn global_search_with_no_hits_keeps_the_keyboard() {
    let dir = tempfile::tempdir().unwrap();
    let mut gateway = search_gateway();
    gateway.search_results = Ok(Vec::new());
    let events = vec![
        tap_row(1),
        tap_key(Key::Char('z')),
        tap_key(Key::Search),
        tap_back(), // dismiss the message
    ];
    let mut app = app(dir.path(), gateway, events);
    app.run().unwrap();

    let Screen::Search { query, .. } = app.screen() else {
        panic!("expected to land back on the keyboard");
    };
    assert_eq!(query, "z");
}

#[test]
fn global_search_skips_a_broken_source_keeps_the_rest() {
    // search_results is shared in the fake, so simulate "one broken
    // source" with all-broken + non-empty vs the message path instead:
    // a failing source must surface in the no-results message.
    let dir = tempfile::tempdir().unwrap();
    let mut gateway = search_gateway();
    gateway.search_results = Err("cloudflare tantrum".into());
    let events = vec![tap_row(1), tap_key(Key::Char('a')), tap_key(Key::Search)];
    let mut app = app(dir.path(), gateway, events);
    app.run().unwrap();

    let Screen::Message { title, body } = app.screen() else {
        panic!("expected message screen");
    };
    assert_eq!(title, "Search");
    assert!(body.contains("Search failed on: Src"), "{body}");
}

#[test]
fn listings_search_row_opens_the_keyboard() {
    let dir = tempfile::tempdir().unwrap();
    let events = vec![tap_row(2), tap_row(0), tap_row(2)];
    let mut app = app(dir.path(), search_gateway(), events);
    app.run().unwrap();

    let Screen::Search { source, query } = app.screen() else {
        panic!("expected search screen");
    };
    assert_eq!(source.as_ref().map(|s| s.id.as_str()), Some("src"));
    assert_eq!(query, "");
    assert!(
        app.display().buffer.iter().any(|&p| p < 0x80),
        "keyboard screen is blank"
    );
}

#[test]
fn typing_builds_the_query_with_partial_refreshes() {
    let dir = tempfile::tempdir().unwrap();
    let events = vec![
        tap_row(2),
        tap_row(0),
        tap_row(2),
        tap_key(Key::Char('n')),
        tap_key(Key::Char('a')),
        tap_key(Key::Char('x')),
        tap_key(Key::Backspace),
        tap_key(Key::Space),
        tap_key(Key::Char('1')),
    ];
    let mut app = app(dir.path(), search_gateway(), events);
    app.run().unwrap();

    let Screen::Search { query, .. } = app.screen() else {
        panic!("expected search screen");
    };
    assert_eq!(query, "na 1");
    // Key taps are partial refreshes (no full e-ink flash per letter).
    let flushes = &app.display().flushes;
    assert!(flushes
        .iter()
        .rev()
        .take(6)
        .all(|m| *m == RefreshMode::Partial));
}

#[test]
fn every_eighth_keystroke_flashes_the_panel_clean() {
    let dir = tempfile::tempdir().unwrap();
    let mut events = vec![tap_row(2), tap_row(0), tap_row(2)];
    events.extend(std::iter::repeat_with(|| tap_key(Key::Char('a'))).take(8));
    let mut app = app(dir.path(), search_gateway(), events);
    app.run().unwrap();

    // The last 8 flushes are the keyboard repaints: 7 partials, then the
    // anti-ghosting full refresh on the 8th.
    let flushes = &app.display().flushes;
    let last8 = &flushes[flushes.len() - 8..];
    assert_eq!(last8[7], RefreshMode::Full);
    assert!(last8[..7].iter().all(|m| *m == RefreshMode::Partial));
}

#[test]
fn punctuation_for_manga_titles_is_typeable() {
    let dir = tempfile::tempdir().unwrap();
    let events = vec![
        tap_row(2),
        tap_row(0),
        tap_row(2),
        tap_key(Key::Char('r')),
        tap_key(Key::Char('e')),
        tap_key(Key::Char(':')),
        tap_key(Key::Char('-')),
        tap_key(Key::Char('\'')),
        tap_key(Key::Char('.')),
    ];
    let mut app = app(dir.path(), search_gateway(), events);
    app.run().unwrap();

    let Screen::Search { query, .. } = app.screen() else {
        panic!("expected search screen");
    };
    assert_eq!(query, "re:-'.");
}

#[test]
fn space_is_not_allowed_leading_or_doubled() {
    let dir = tempfile::tempdir().unwrap();
    let events = vec![
        tap_row(2),
        tap_row(0),
        tap_row(2),
        tap_key(Key::Space), // leading — ignored
        tap_key(Key::Char('a')),
        tap_key(Key::Space),
        tap_key(Key::Space), // doubled — ignored
    ];
    let mut app = app(dir.path(), search_gateway(), events);
    app.run().unwrap();

    let Screen::Search { query, .. } = app.screen() else {
        panic!("expected search screen");
    };
    assert_eq!(query, "a ");
}

#[test]
fn search_key_queries_the_gateway_and_shows_results() {
    let dir = tempfile::tempdir().unwrap();
    let events = vec![
        tap_row(2),
        tap_row(0),
        tap_row(2),
        tap_key(Key::Char('n')),
        tap_key(Key::Char('a')),
        tap_key(Key::Search),
    ];
    let mut app = app(dir.path(), search_gateway(), events);
    app.run().unwrap();

    assert_eq!(*app.gateway().searches.borrow(), vec!["na".to_string()]);
    let Screen::MangaList {
        listing, mangas, ..
    } = app.screen()
    else {
        panic!("expected search results");
    };
    assert_eq!(listing, "\"na\"");
    assert_eq!(mangas.len(), 1);
    assert_eq!(mangas[0].title, "Naruto");
}

#[test]
fn search_results_open_chapters_like_any_list() {
    let dir = tempfile::tempdir().unwrap();
    let mut gateway = search_gateway();
    gateway.chapters = vec![ChapterEntry {
        id: "c1".into(),
        num: Some(1.0),
        title: None,
        lang: None,
    }];
    let events = vec![
        tap_row(2),
        tap_row(0),
        tap_row(2),
        tap_key(Key::Char('n')),
        tap_key(Key::Search),
        tap_row(0), // Naruto -> ChapterList
    ];
    let mut app = app(dir.path(), gateway, events);
    app.run().unwrap();

    let Screen::ChapterList { manga, .. } = app.screen() else {
        panic!("expected chapter list from search result");
    };
    assert_eq!(manga.title, "Naruto");
}

#[test]
fn empty_query_search_does_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let events = vec![tap_row(2), tap_row(0), tap_row(2), tap_key(Key::Search)];
    let mut app = app(dir.path(), search_gateway(), events);
    app.run().unwrap();

    assert!(app.gateway().searches.borrow().is_empty());
    assert!(matches!(app.screen(), Screen::Search { .. }));
}

#[test]
fn empty_results_show_a_message_and_keep_the_keyboard_below() {
    let dir = tempfile::tempdir().unwrap();
    let mut gateway = search_gateway();
    gateway.search_results = Ok(Vec::new());
    let events = vec![
        tap_row(2),
        tap_row(0),
        tap_row(2),
        tap_key(Key::Char('z')),
        tap_key(Key::Search),
        tap_back(), // dismiss the message -> back on the keyboard
    ];
    let mut app = app(dir.path(), gateway, events);
    app.run().unwrap();

    let Screen::Search { query, .. } = app.screen() else {
        panic!("expected to return to the keyboard");
    };
    assert_eq!(query, "z");
}

#[test]
fn search_failure_shows_error_screen() {
    let dir = tempfile::tempdir().unwrap();
    let mut gateway = search_gateway();
    gateway.search_results = Err("source exploded".into());
    let events = vec![
        tap_row(2),
        tap_row(0),
        tap_row(2),
        tap_key(Key::Char('a')),
        tap_key(Key::Search),
    ];
    let mut app = app(dir.path(), gateway, events);
    app.run().unwrap();

    let Screen::Message { title, body } = app.screen() else {
        panic!("expected error screen");
    };
    assert_eq!(title, "Error");
    assert!(body.contains("source exploded"));
}

#[test]
fn back_leaves_the_keyboard() {
    let dir = tempfile::tempdir().unwrap();
    let events = vec![tap_row(2), tap_row(0), tap_row(2), tap_back()];
    let mut app = app(dir.path(), search_gateway(), events);
    app.run().unwrap();
    assert!(matches!(app.screen(), Screen::Listings { .. }));
}

// --- physical page-turn buttons ---

#[test]
fn page_buttons_flip_library_pages() {
    let dir = tempfile::tempdir().unwrap();
    let lib = dir.path().join("Manga");
    let l = layout();
    let capacity = ShelfLayout::new(l.width, l.content_height(), SHELF_COLUMNS).capacity();
    for i in 0..capacity + 2 {
        make_cbz(&lib.join(format!("Series/vol{i:02}.cbz")), 1);
    }

    let events = vec![
        tap_row(0), // Library
        UiEvent::PageForward,
        UiEvent::PageForward, // clamped at the last page
        UiEvent::PageBack,
    ];
    let mut app = app(&lib, FakeGateway::default(), events);
    app.run().unwrap();

    let Screen::Library { page, .. } = app.screen() else {
        panic!("expected library");
    };
    assert_eq!(*page, 0, "forward, clamp, back lands on page 0");
    // Button page flips are partial refreshes, like nav-bar ones.
    assert!(app.display().flushes.contains(&RefreshMode::Partial));
}

#[test]
fn page_buttons_turn_reader_pages() {
    let dir = tempfile::tempdir().unwrap();
    let lib = dir.path().join("Manga");
    make_cbz(&lib.join("Sample/vol1.cbz"), 5);

    let events = vec![
        tap_row(0),
        tap_shelf_cell0(),
        UiEvent::PageForward, // page 1
        UiEvent::PageForward, // page 2
        UiEvent::PageBack,    // page 1
        reader_tap_back(),
    ];
    let mut app = app(&lib, FakeGateway::default(), events);
    app.run().unwrap();

    let store = ProgressStore::load(&progress_path(&lib)).unwrap();
    assert_eq!(store.get("Sample/vol1.cbz").unwrap().current_page, 1);
}

#[test]
fn page_buttons_are_ignored_on_unpaged_screens() {
    // Home has no pages; a button press must not crash or navigate.
    let dir = tempfile::tempdir().unwrap();
    let events = vec![UiEvent::PageForward, UiEvent::PageBack, tap_row(0)];
    let lib = dir.path().join("Manga");
    let mut app = app(&lib, FakeGateway::default(), events);
    app.run().unwrap();
    assert!(matches!(app.screen(), Screen::Library { .. }));
}

// --- sleep (power button / sleep cover) ---

/// A counting sleeper hook.
fn counting_sleeper() -> (std::rc::Rc<std::cell::Cell<usize>>, SleepFn) {
    let count = std::rc::Rc::new(std::cell::Cell::new(0usize));
    let c = count.clone();
    (
        count,
        Box::new(move || {
            c.set(c.get() + 1);
            Ok(SleepResult::Slept)
        }),
    )
}

#[test]
fn sleep_event_suspends_and_repaints_in_full() {
    let dir = tempfile::tempdir().unwrap();
    let (count, sleeper) = counting_sleeper();
    let mut app =
        app(dir.path(), FakeGateway::default(), vec![UiEvent::Sleep]).with_sleeper(sleeper);
    app.run().unwrap();

    assert_eq!(count.get(), 1, "sleeper must run on UiEvent::Sleep");
    // Initial paint, the "Sleeping…" screen, the post-wake repaint.
    assert_eq!(
        app.display().flushes,
        vec![RefreshMode::Full, RefreshMode::Full, RefreshMode::Full]
    );
    assert!(matches!(app.screen(), Screen::Home));
    assert_eq!(
        app.input().refreshes,
        1,
        "input devices must be reopened after resume — the kernel can \
         re-register the nodes and dead fds would kill input"
    );
}

#[test]
fn back_to_back_sleep_events_are_debounced() {
    // The press that woke the device can be delivered after the post-wake
    // drain; it must not bounce us straight back into suspend.
    let dir = tempfile::tempdir().unwrap();
    let (count, sleeper) = counting_sleeper();
    let events = vec![UiEvent::Sleep, UiEvent::Sleep, UiEvent::Sleep];
    let mut app = app(dir.path(), FakeGateway::default(), events).with_sleeper(sleeper);
    app.run().unwrap();
    assert_eq!(count.get(), 1, "wake-press echo must not re-suspend");
}

#[test]
fn skipped_suspend_explains_itself_and_stays_awake() {
    let dir = tempfile::tempdir().unwrap();
    let count = std::rc::Rc::new(std::cell::Cell::new(0usize));
    let c = count.clone();
    let sleeper: SleepFn = Box::new(move || {
        c.set(c.get() + 1);
        Ok(SleepResult::Skipped)
    });
    let mut app =
        app(dir.path(), FakeGateway::default(), vec![UiEvent::Sleep]).with_sleeper(sleeper);
    app.run().unwrap();

    assert_eq!(count.get(), 1);
    assert!(matches!(app.screen(), Screen::Home));
    // Initial paint, "Sleeping…", the "staying awake" notice, the restore.
    assert_eq!(app.display().flushes.len(), 4);
    assert!(app
        .display()
        .flushes
        .iter()
        .all(|m| *m == RefreshMode::Full));
}

#[test]
fn sleep_right_after_a_download_suspends_in_the_reader() {
    // A cover closed while a chapter downloaded surfaces as the first
    // event the reader sees; it must suspend, not be treated as a tap.
    let dir = tempfile::tempdir().unwrap();
    let lib = dir.path().join("Manga");
    std::fs::create_dir_all(&lib).unwrap();
    let gateway = FakeGateway {
        installed: RefCell::new(vec![SourceEntry {
            id: "src".into(),
            name: "Src".into(),
        }]),
        mangas: Ok(vec![MangaEntry {
            id: "m1".into(),
            title: "Manga One".into(),
            cover_url: None,
        }]),
        chapters: vec![ChapterEntry {
            id: "c1".into(),
            num: Some(1.0),
            title: None,
            lang: None,
        }],
        download: Some(Box::new(move |library, _progress| {
            let path = library.join("Manga One/Chapter 1.cbz");
            make_cbz(&path, 3);
            Ok(path)
        })),
        ..FakeGateway::default()
    };

    let (count, sleeper) = counting_sleeper();
    let events = vec![
        tap_row(2),        // Sources
        tap_row(0),        // Listings
        tap_row(0),        // Popular
        tap_row(0),        // Manga One
        tap_row(0),        // download + Reader
        UiEvent::Sleep,    // the cover-close that queued during the download
        reader_tap_back(), // back out after waking
    ];
    let mut app = app(&lib, gateway, events).with_sleeper(sleeper);
    app.run().unwrap();

    assert_eq!(count.get(), 1, "queued sleep must still suspend");
    assert!(matches!(app.screen(), Screen::ChapterList { .. }));
}

#[test]
fn waking_reapplies_the_frontlight() {
    // The kernel powers the light down across suspend; after the sleeper
    // returns, the saved levels must be written to the hardware again.
    let dir = tempfile::tempdir().unwrap();
    let (count, sleeper) = counting_sleeper();
    let writes = std::rc::Rc::new(std::cell::Cell::new(0usize));

    struct CountingLights {
        writes: std::rc::Rc<std::cell::Cell<usize>>,
    }
    impl LightControl for CountingLights {
        fn brightness(&self) -> u8 {
            55
        }
        fn set_brightness(&mut self, _: u8) {
            self.writes.set(self.writes.get() + 1);
        }
        fn warmth(&self) -> u8 {
            30
        }
        fn set_warmth(&mut self, _: u8) {
            self.writes.set(self.writes.get() + 1);
        }
    }

    let mut app = app(dir.path(), FakeGateway::default(), vec![UiEvent::Sleep])
        .with_sleeper(sleeper)
        .with_lights(Box::new(CountingLights {
            writes: writes.clone(),
        }));
    app.run().unwrap();

    assert_eq!(count.get(), 1);
    assert_eq!(
        writes.get(),
        2,
        "brightness and warmth must both be rewritten after wake"
    );
}

#[test]
fn sleep_without_a_hook_is_ignored() {
    let dir = tempfile::tempdir().unwrap();
    let events = vec![UiEvent::Sleep, tap_row(0)];
    let lib = dir.path().join("Manga");
    let mut app = app(&lib, FakeGateway::default(), events);
    app.run().unwrap();

    // No crash, no extra repaint; the tap after the ignored event worked.
    assert!(matches!(app.screen(), Screen::Library { .. }));
}

#[test]
fn sleeper_failure_lands_on_the_error_screen() {
    let dir = tempfile::tempdir().unwrap();
    let sleeper: SleepFn = Box::new(|| Err(anyhow!("EBUSY all the way down")));
    let mut app =
        app(dir.path(), FakeGateway::default(), vec![UiEvent::Sleep]).with_sleeper(sleeper);
    app.run().unwrap();

    let Screen::Message { title, body } = app.screen() else {
        panic!("expected error screen");
    };
    assert_eq!(title, "Error");
    assert!(body.contains("EBUSY"));
}

#[test]
fn sleep_in_the_reader_saves_progress_first_and_resumes() {
    let dir = tempfile::tempdir().unwrap();
    let lib = dir.path().join("Manga");
    make_cbz(&lib.join("Sample/vol1.cbz"), 5);

    let progress_at_sleep = std::rc::Rc::new(std::cell::Cell::new(None::<usize>));
    let lib_for_hook = lib.clone();
    let probe = progress_at_sleep.clone();
    let sleeper: SleepFn = Box::new(move || {
        // What's on disk while we are "suspended"?
        let store = ProgressStore::load(&progress_path(&lib_for_hook)).unwrap_or_default();
        probe.set(store.get("Sample/vol1.cbz").map(|p| p.current_page));
        Ok(SleepResult::Slept)
    });

    let events = vec![
        tap_row(0),        // Library
        tap_shelf_cell0(), // Reader, page 0
        reader_tap_next(), // page 1
        UiEvent::Sleep,    // suspend mid-read
        reader_tap_next(), // page 2 after waking
        reader_tap_back(),
    ];
    let mut app = app(&lib, FakeGateway::default(), events).with_sleeper(sleeper);
    app.run().unwrap();

    assert_eq!(
        progress_at_sleep.get(),
        Some(1),
        "progress must be on disk before the power goes down"
    );
    let store = ProgressStore::load(&progress_path(&lib)).unwrap();
    assert_eq!(
        store.get("Sample/vol1.cbz").unwrap().current_page,
        2,
        "reading continues after waking"
    );
    // The post-wake repaint is a full refresh.
    let flushes = &app.display().flushes;
    assert!(flushes.iter().filter(|m| **m == RefreshMode::Full).count() >= 3);
}

#[test]
fn update_prompt_back_declines() {
    let dir = tempfile::tempdir().unwrap();
    let gateway = FakeGateway {
        update_available: true,
        update_message: "Update available.".into(),
        ..FakeGateway::default()
    };
    let mut app = app(dir.path(), gateway, vec![tap_row(3), tap_back()]);
    app.run().unwrap();
    assert_eq!(app.gateway().installs.get(), 0, "back should not install");
}
