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
    /// Source ids passed to `uninstall_source`, in order.
    uninstalled: RefCell<Vec<String>>,
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
            uninstalled: RefCell::new(Vec::new()),
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

    fn uninstall_source(&self, source_id: &str) -> Result<()> {
        self.uninstalled.borrow_mut().push(source_id.to_string());
        self.installed.borrow_mut().retain(|s| s.id != source_id);
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

/// The menu layout the app builds at `rot`: rotated dims for 90/270.
fn menu_layout(rot: u32) -> UiLayout {
    if rot % 180 == 90 {
        UiLayout::new(H, W)
    } else {
        UiLayout::new(W, H)
    }
}

/// The panel coordinates whose menu mapping (map_reader_tap at `rot`)
/// lands on reading-frame (`rx`, `ry`) — the inverse of the input
/// chokepoint, for aiming taps at rotated menus.
fn panel_point_for(rx: u32, ry: u32, rot: u32) -> (u32, u32) {
    match rot % 360 {
        90 => (W - 1 - ry, rx),
        180 => (W - 1 - rx, H - 1 - ry),
        270 => (ry, H - 1 - rx),
        _ => (rx, ry),
    }
}

/// [`tap_row`] aimed at a menu rendered at rotation `rot`.
fn tap_row_rot(i: usize, rot: u32) -> UiEvent {
    let l = menu_layout(rot);
    let (x, y) = panel_point_for(l.width / 2, l.row_top(i) + l.row_h / 2, rot);
    UiEvent::Tap { x, y }
}

/// [`tap_shelf_cell0`] aimed at a library shelf rendered at rotation `rot`.
fn tap_shelf_cell0_rot(rot: u32) -> UiEvent {
    let l = menu_layout(rot);
    let shelf = ShelfLayout::new(l.width, l.content_height(), SHELF_COLUMNS);
    let (cx, cy) = shelf.cell_origin(0);
    let (x, y) = panel_point_for(
        cx + shelf.cell_width() / 2,
        l.content_top() + cy + shelf.cell_height() / 2,
        rot,
    );
    UiEvent::Tap { x, y }
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
fn offline_home_shows_reconnect_row_and_offsets_taps() {
    let dir = tempfile::tempdir().unwrap();
    let lib = dir.path().join("Manga");
    make_cbz(&lib.join("Sample/vol1.cbz"), 3);

    // Offline (forced): content row 0 is the reconnect button — tapping it
    // attempts a reconnect and stays on Home (off-device that's a no-op).
    let mut a = app(&lib, FakeGateway::default(), vec![]);
    a.home_offline = true;
    a.activate(0, 10, 10).unwrap();
    assert!(
        matches!(a.screen(), Screen::Home),
        "offline row 0 reconnects, stays on Home"
    );

    // Offline: the standard entries are offset past the reconnect row, so the
    // first real entry (Library) is row 1.
    let mut b = app(&lib, FakeGateway::default(), vec![]);
    b.home_offline = true;
    b.activate(1, 10, 10).unwrap();
    assert!(
        matches!(b.screen(), Screen::Library { .. }),
        "offline row 1 is Library"
    );

    // Online (the default): no reconnect row, so Library is row 0.
    let mut c = app(&lib, FakeGateway::default(), vec![]);
    c.activate(0, 10, 10).unwrap();
    assert!(
        matches!(c.screen(), Screen::Library { .. }),
        "online row 0 is Library"
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
    // the middle band is still "back". The MENU taps that reach the
    // reader are rotation-aimed too — menus now follow the rotation.
    let tap_panel_bottom = UiEvent::Tap { x: W / 2, y: H - 1 };
    let tap_panel_top = UiEvent::Tap { x: W / 2, y: 0 };
    let tap_panel_middle = UiEvent::Tap { x: W / 2, y: H / 2 };
    let events = vec![
        tap_row_rot(0, 90),
        tap_shelf_cell0_rot(90),
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

// --- app-wide rotation: menus follow reader_rotation ---

#[test]
fn menu_taps_land_the_right_row_at_each_rotation() {
    for rot in [90u32, 180, 270] {
        let dir = tempfile::tempdir().unwrap();
        let lib = dir.path().join("Manga");
        make_cbz(&lib.join("Sample/vol1.cbz"), 2);

        // Row 0 opens the Library…
        let mut library_app = app(&lib, FakeGateway::default(), vec![tap_row_rot(0, rot)])
            .with_reader_settings(FitMode::Contain, rot);
        library_app.run().unwrap();
        assert!(
            matches!(library_app.screen(), Screen::Library { .. }),
            "rotation {rot}: the Library row tap must open the Library"
        );

        // …and row 3 opens Settings: per-row precision, not just "hit
        // something".
        let mut settings_app = app(&lib, FakeGateway::default(), vec![tap_row_rot(3, rot)])
            .with_reader_settings(FitMode::Contain, rot);
        settings_app.run().unwrap();
        assert!(
            matches!(settings_app.screen(), Screen::Settings),
            "rotation {rot}: the Settings row tap must open Settings"
        );
    }
}

#[test]
fn menus_render_rotated_into_the_panel() {
    // The title separator (the 0x55 hline under the title bar) must land
    // exactly where the tap mapping expects it, at every rotation.
    for rot in [0u32, 90, 180, 270] {
        let dir = tempfile::tempdir().unwrap();
        let mut app = app(dir.path(), FakeGateway::default(), vec![])
            .with_reader_settings(FitMode::Contain, rot);
        app.run().unwrap();
        let l = menu_layout(rot);
        let (x, y) = panel_point_for(l.width / 2, l.title_h - 1, rot);
        assert_eq!(
            app.display().pixel(x, y),
            0x55,
            "rotation {rot}: title separator not where the tap mapping points"
        );
    }
}

// --- reader controls sheet ---

/// An up-swipe starting in the bottom eighth of the (unrotated) panel.
fn bottom_edge_swipe_up() -> UiEvent {
    UiEvent::Swipe {
        x0: W / 2,
        y0: H - 20,
        x1: W / 2,
        y1: H - 320,
    }
}

/// Tap row `i` of the controls sheet (rotation 0: panel == reading frame).
fn tap_sheet_row(i: usize) -> UiEvent {
    let l = layout();
    let top = H - SHEET_ROW_COUNT * l.row_h;
    UiEvent::Tap {
        x: W / 2,
        y: top + i as u32 * l.row_h + l.row_h / 2,
    }
}

#[test]
fn controls_sheet_opens_from_bottom_edge_swipe_only() {
    let dir = tempfile::tempdir().unwrap();
    let lib = dir.path().join("Manga");
    let settings_dir = dir.path().join("data");
    make_cbz(&lib.join("Sample/vol1.cbz"), 5);

    let events = vec![
        tap_row(0),
        tap_shelf_cell0(),
        bottom_edge_swipe_up(),         // opens the sheet — must NOT rotate
        tap_sheet_row(SHEET_ROW_CLOSE), // Close
        reader_tap_next(),              // zones still unrotated -> page 1
        reader_tap_back(),
    ];
    let mut app = app(&lib, FakeGateway::default(), events).with_settings_dir(settings_dir.clone());
    app.run().unwrap();

    let settings = gideon_core::Settings::load(&settings_dir).unwrap();
    assert_eq!(
        settings.reader_rotation, 0,
        "a bottom-edge swipe opens the sheet, it never rotates"
    );
    let store = ProgressStore::load(&progress_path(&lib)).unwrap();
    assert_eq!(
        store.get("Sample/vol1.cbz").unwrap().current_page,
        1,
        "Close must return to the page with unrotated tap zones"
    );
}

#[test]
fn controls_sheet_rotate_matches_the_swipe_rotation() {
    let dir = tempfile::tempdir().unwrap();
    let lib = dir.path().join("Manga");
    let settings_dir = dir.path().join("data");
    make_cbz(&lib.join("Sample/vol1.cbz"), 5);

    // After the sheet's Rotate the zones follow the 90° orientation,
    // exactly like the mid-screen up-swipe.
    let tap_panel_bottom = UiEvent::Tap { x: W / 2, y: H - 1 };
    let tap_rotated_back = UiEvent::Tap { x: W / 2, y: H / 2 };
    let events = vec![
        tap_row(0),
        tap_shelf_cell0(),
        bottom_edge_swipe_up(),
        tap_sheet_row(0), // Rotate 90°
        tap_panel_bottom, // next page in the rotated orientation
        tap_rotated_back,
    ];
    let mut app = app(&lib, FakeGateway::default(), events).with_settings_dir(settings_dir.clone());
    app.run().unwrap();

    let settings = gideon_core::Settings::load(&settings_dir).unwrap();
    assert_eq!(settings.reader_rotation, 90, "locked (default) persists");
    let store = ProgressStore::load(&progress_path(&lib)).unwrap();
    assert_eq!(store.get("Sample/vol1.cbz").unwrap().current_page, 1);
}

#[test]
fn orientation_lock_toggle_persists_and_auto_keeps_rotation_session_only() {
    let dir = tempfile::tempdir().unwrap();
    let lib = dir.path().join("Manga");
    let settings_dir = dir.path().join("data");
    make_cbz(&lib.join("Sample/vol1.cbz"), 5);

    let mid_swipe_up = UiEvent::Swipe {
        x0: W / 2,
        y0: H - 150,
        x1: W / 2,
        y1: 100,
    };
    let tap_panel_bottom = UiEvent::Tap { x: W / 2, y: H - 1 };
    let tap_rotated_back = UiEvent::Tap { x: W / 2, y: H / 2 };
    let events = vec![
        tap_row(0),
        tap_shelf_cell0(),
        bottom_edge_swipe_up(),
        tap_sheet_row(SHEET_ROW_ORIENTATION), // locked -> auto (persisted)
        tap_sheet_row(SHEET_ROW_CLOSE),       // Close
        mid_swipe_up,                         // rotate to 90 — session-only now
        tap_panel_bottom,                     // the rotation still applies in-session
        tap_rotated_back,
    ];
    let mut app = app(&lib, FakeGateway::default(), events).with_settings_dir(settings_dir.clone());
    app.run().unwrap();

    let settings = gideon_core::Settings::load(&settings_dir).unwrap();
    assert!(
        !settings.reader_rotation_locked,
        "the toggle must persist immediately"
    );
    assert_eq!(
        settings.reader_rotation, 0,
        "unlocked (auto) rotation must not persist"
    );
    let store = ProgressStore::load(&progress_path(&lib)).unwrap();
    assert_eq!(
        store.get("Sample/vol1.cbz").unwrap().current_page,
        1,
        "the session-only rotation still drives the tap zones"
    );
}

#[test]
fn controls_sheet_labels_show_lock_state() {
    assert_eq!(controls_sheet_labels(true, false)[1], "Orientation: locked");
    assert_eq!(controls_sheet_labels(false, false)[1], "Orientation: auto");
    assert_eq!(controls_sheet_labels(true, false)[0], "Rotate 90°");
    assert_eq!(
        controls_sheet_labels(true, false)[SHEET_ROW_AUTO_SPREAD],
        "Auto-rotate spreads: off"
    );
    assert_eq!(
        controls_sheet_labels(true, true)[SHEET_ROW_AUTO_SPREAD],
        "Auto-rotate spreads: on"
    );
    assert_eq!(controls_sheet_labels(true, false)[SHEET_ROW_CLOSE], "Close");
}

#[test]
fn controls_sheet_rows_resolve_from_reading_taps() {
    // An 800-high reading frame with 48px rows and four rows: the sheet covers
    // [608, 800); above it is None (closes the sheet).
    assert_eq!(controls_sheet_row(800, 48, 607), None);
    assert_eq!(controls_sheet_row(800, 48, 608), Some(SHEET_ROW_ROTATE));
    assert_eq!(
        controls_sheet_row(800, 48, 656),
        Some(SHEET_ROW_ORIENTATION)
    );
    assert_eq!(
        controls_sheet_row(800, 48, 704),
        Some(SHEET_ROW_AUTO_SPREAD)
    );
    assert_eq!(controls_sheet_row(800, 48, 752), Some(SHEET_ROW_CLOSE));
    assert_eq!(controls_sheet_row(800, 48, 799), Some(SHEET_ROW_CLOSE));
}

#[test]
fn controls_sheet_origin_follows_the_reading_bottom_edge() {
    // The reading frame's bottom edge lands on a different panel edge per
    // rotation: bottom at 0, left at 90, top at 180, right at 270.
    assert_eq!(controls_sheet_origin(600, 800, 144, 0), (0, 656));
    assert_eq!(controls_sheet_origin(600, 800, 144, 90), (0, 0));
    assert_eq!(controls_sheet_origin(600, 800, 144, 180), (0, 0));
    assert_eq!(controls_sheet_origin(600, 800, 144, 270), (456, 0));
}

// --- accelerometer auto-rotation + physical page buttons ---

/// Settings with the orientation unlocked ("auto"), saved to `dir` so
/// `with_settings_dir` seeds the app into gyro-follow mode.
fn auto_orientation_settings(dir: &Path) {
    let settings = gideon_core::Settings {
        reader_rotation_locked: false,
        ..gideon_core::Settings::default()
    };
    settings.save(dir).unwrap();
}

#[test]
fn gyro_rotates_menus_in_auto_mode() {
    let dir = tempfile::tempdir().unwrap();
    let settings_dir = dir.path().join("data");
    auto_orientation_settings(&settings_dir);

    let mut app = app(
        dir.path(),
        FakeGateway::default(),
        vec![UiEvent::Rotate { rotation: 90 }],
    )
    .with_settings_dir(settings_dir);
    app.run().unwrap();

    // Home re-rendered rotated into the panel: the title separator lands
    // where the 90° mapping expects it (cf. menus_render_rotated_into_the_panel).
    let l = menu_layout(90);
    let (x, y) = panel_point_for(l.width / 2, l.title_h - 1, 90);
    assert_eq!(
        app.display().pixel(x, y),
        0x55,
        "a gyro report must rotate the menus in auto mode"
    );
}

#[test]
fn gyro_is_ignored_when_orientation_locked() {
    // No settings dir: orientation defaults to locked, so the gyro is off.
    let dir = tempfile::tempdir().unwrap();
    let mut app = app(
        dir.path(),
        FakeGateway::default(),
        vec![UiEvent::Rotate { rotation: 90 }],
    );
    app.run().unwrap();

    // The menus stayed upright: the separator is at the unrotated location.
    let l = menu_layout(0);
    assert_eq!(
        app.display().pixel(l.width / 2, l.title_h - 1),
        0x55,
        "a locked orientation must ignore the accelerometer"
    );
}

#[test]
fn gyro_rotates_the_reader_in_auto_mode() {
    let dir = tempfile::tempdir().unwrap();
    let lib = dir.path().join("Manga");
    let settings_dir = dir.path().join("data");
    make_cbz(&lib.join("Sample/vol1.cbz"), 5);
    auto_orientation_settings(&settings_dir);

    // Open the reader upright, then a gyro report rotates it to 90°, after
    // which the tap zones follow the new orientation (bottom = next).
    let tap_panel_bottom = UiEvent::Tap { x: W / 2, y: H - 1 };
    let tap_rotated_back = UiEvent::Tap { x: W / 2, y: H / 2 };
    let events = vec![
        tap_row(0),
        tap_shelf_cell0(),
        UiEvent::Rotate { rotation: 90 },
        tap_panel_bottom,
        tap_rotated_back,
    ];
    let mut app = app(&lib, FakeGateway::default(), events).with_settings_dir(settings_dir);
    app.run().unwrap();

    let store = ProgressStore::load(&progress_path(&lib)).unwrap();
    assert_eq!(
        store.get("Sample/vol1.cbz").unwrap().current_page,
        1,
        "the reader must rotate to the gyro orientation and map taps to it"
    );
}

#[test]
fn waking_snaps_the_menus_to_the_current_orientation() {
    // The Kobo gsensor reports only on *change*, so after a suspend/resume it
    // won't re-announce the current orientation — the wake path must resync
    // and rotate the menus itself, or they stay stuck at the pre-sleep angle
    // ("screen won't rotate after sleep").
    let dir = tempfile::tempdir().unwrap();
    let settings_dir = dir.path().join("data");
    auto_orientation_settings(&settings_dir);
    let (_count, sleeper) = counting_sleeper();

    let mut app = app(dir.path(), FakeGateway::default(), vec![UiEvent::Sleep])
        .with_settings_dir(settings_dir)
        .with_sleeper(sleeper);
    // The device is held at 90° when it wakes.
    app.input_mut().resync = Some(90);
    app.run().unwrap();

    let l = menu_layout(90);
    let (x, y) = panel_point_for(l.width / 2, l.title_h - 1, 90);
    assert_eq!(
        app.display().pixel(x, y),
        0x55,
        "waking must snap the menus to how the device is held"
    );
}

#[test]
fn waking_keeps_the_menus_upright_when_orientation_locked() {
    // No settings dir: orientation defaults to locked. A wake resync that
    // reports 90° must be ignored — a locked orientation never follows the
    // accelerometer, on wake or otherwise.
    let dir = tempfile::tempdir().unwrap();
    let (_count, sleeper) = counting_sleeper();

    let mut app =
        app(dir.path(), FakeGateway::default(), vec![UiEvent::Sleep]).with_sleeper(sleeper);
    app.input_mut().resync = Some(90);
    app.run().unwrap();

    let l = menu_layout(0);
    assert_eq!(
        app.display().pixel(l.width / 2, l.title_h - 1),
        0x55,
        "a locked orientation must ignore the wake resync"
    );
}

#[test]
fn waking_snaps_the_reader_to_the_current_orientation() {
    let dir = tempfile::tempdir().unwrap();
    let lib = dir.path().join("Manga");
    let settings_dir = dir.path().join("data");
    make_cbz(&lib.join("Sample/vol1.cbz"), 5);
    auto_orientation_settings(&settings_dir);
    let (_count, sleeper) = counting_sleeper();

    // Open the reader upright, sleep, then wake held at 90°: the tap zones must
    // now follow the 90° orientation (panel bottom = next page), proving the
    // reader rotated on wake without a fresh gyro report.
    let tap_panel_bottom = UiEvent::Tap { x: W / 2, y: H - 1 };
    let tap_rotated_back = UiEvent::Tap { x: W / 2, y: H / 2 };
    let events = vec![
        tap_row(0),
        tap_shelf_cell0(),
        UiEvent::Sleep,
        tap_panel_bottom,
        tap_rotated_back,
    ];
    let mut app = app(&lib, FakeGateway::default(), events)
        .with_settings_dir(settings_dir)
        .with_sleeper(sleeper);
    app.input_mut().resync = Some(90);
    app.run().unwrap();

    let store = ProgressStore::load(&progress_path(&lib)).unwrap();
    assert_eq!(
        store.get("Sample/vol1.cbz").unwrap().current_page,
        1,
        "waking must rotate the reader to how the device is held and map taps to it"
    );
}

#[test]
fn physical_forward_button_advances_when_upright() {
    let dir = tempfile::tempdir().unwrap();
    let lib = dir.path().join("Manga");
    make_cbz(&lib.join("Sample/vol1.cbz"), 5);

    let events = vec![
        tap_row(0),
        tap_shelf_cell0(),
        UiEvent::PageForward, // upright: forward advances to page 1
        reader_tap_back(),
    ];
    let mut app = app(&lib, FakeGateway::default(), events);
    app.run().unwrap();

    let store = ProgressStore::load(&progress_path(&lib)).unwrap();
    assert_eq!(store.get("Sample/vol1.cbz").unwrap().current_page, 1);
}

#[test]
fn physical_buttons_swap_when_upside_down() {
    // The physical page buttons follow the reading orientation: held upside
    // down (180°) the two keys have physically swapped places, so the
    // forward button goes BACK.
    let dir = tempfile::tempdir().unwrap();
    let lib = dir.path().join("Manga");
    make_cbz(&lib.join("Sample/vol1.cbz"), 5);
    let mut store = ProgressStore::default();
    store.update("Sample/vol1.cbz", 2, 5);
    store.save(&progress_path(&lib)).unwrap();

    let events = vec![
        tap_row_rot(0, 180),
        tap_shelf_cell0_rot(180),
        UiEvent::PageForward,                // 180°: forward goes back -> page 1
        UiEvent::Tap { x: W / 2, y: H / 2 }, // center is Back at any rotation
    ];
    let mut app =
        app(&lib, FakeGateway::default(), events).with_reader_settings(FitMode::Contain, 180);
    app.run().unwrap();

    let store = ProgressStore::load(&progress_path(&lib)).unwrap();
    assert_eq!(
        store.get("Sample/vol1.cbz").unwrap().current_page,
        1,
        "upside down, the forward button goes back"
    );
}

// --- slow-turn input debounce ---

/// A display whose `flush` sleeps, to simulate a slow (big-page / full-flash)
/// page render so the debounce path in `turn_reader_page` is exercised.
struct SlowDisplay {
    inner: MemoryDisplay,
    delay: std::time::Duration,
}

impl gideon_device::Display for SlowDisplay {
    fn width(&self) -> u32 {
        self.inner.width()
    }
    fn height(&self) -> u32 {
        self.inner.height()
    }
    fn blit(&mut self, page: &gideon_render::GrayPage, offset_y: u32) -> gideon_device::Result<()> {
        self.inner.blit(page, offset_y)
    }
    fn blit_rgb(
        &mut self,
        page: &gideon_render::RgbPage,
        offset_y: u32,
    ) -> gideon_device::Result<()> {
        self.inner.blit_rgb(page, offset_y)
    }
    fn overlay(
        &mut self,
        page: &gideon_render::GrayPage,
        x: u32,
        y: u32,
    ) -> gideon_device::Result<()> {
        self.inner.overlay(page, x, y)
    }
    fn flush(&mut self, mode: RefreshMode) -> gideon_device::Result<()> {
        std::thread::sleep(self.delay);
        self.inner.flush(mode)
    }
}

#[test]
fn slow_page_turn_flushes_queued_presses() {
    // A turn slower than SLOW_TURN drops whatever input queued while it
    // rendered, so a frustrated multi-press doesn't cascade past the target.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("slow.cbz");
    make_cbz(&path, 3);
    let doc = CbzDocument::open(&path).unwrap();
    let mut display = SlowDisplay {
        inner: MemoryDisplay::new(16, 16),
        delay: SLOW_TURN + std::time::Duration::from_millis(50),
    };
    let mut reader = Reader::new(doc, &mut display, FitMode::Contain, 0);
    // Keep this turn a partial refresh (the decode-lag case the debounce is
    // for), not the expected full flash.
    reader.set_full_refresh_interval(8);
    let mut input = FakeInput::new(vec![]);

    assert!(turn_reader_page(&mut reader, &mut input, true).unwrap());
    assert!(
        !reader.last_refresh_was_full(),
        "this turn is a partial refresh"
    );
    assert_eq!(
        input.discard_taps_calls, 1,
        "a slow partial turn must flush the queued frustration-presses"
    );
}

#[test]
fn slow_full_refresh_turn_keeps_input() {
    // A full-refresh turn is slow by design (~0.5s flash). It must NOT be
    // mistaken for a lagging decode and eat a deliberate press queued during
    // the flash.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("fullflash.cbz");
    make_cbz(&path, 3);
    let doc = CbzDocument::open(&path).unwrap();
    let mut display = SlowDisplay {
        inner: MemoryDisplay::new(16, 16),
        delay: SLOW_TURN + std::time::Duration::from_millis(50),
    };
    let mut reader = Reader::new(doc, &mut display, FitMode::Contain, 0);
    // Interval 1 => every turn is a full (flashing) refresh.
    reader.set_full_refresh_interval(1);
    let mut input = FakeInput::new(vec![]);

    assert!(turn_reader_page(&mut reader, &mut input, true).unwrap());
    assert!(
        reader.last_refresh_was_full(),
        "interval 1 makes every turn full"
    );
    assert_eq!(
        input.discard_taps_calls, 0,
        "a slow full-refresh turn must not flush input"
    );
}

#[test]
fn fast_page_turn_keeps_queued_presses() {
    // A fast turn must NOT flush input, so deliberate quick paging still
    // registers every press.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("fast.cbz");
    make_cbz(&path, 3);
    let doc = CbzDocument::open(&path).unwrap();
    let mut display = MemoryDisplay::new(16, 16);
    let mut reader = Reader::new(doc, &mut display, FitMode::Contain, 0);
    let mut input = FakeInput::new(vec![]);

    assert!(turn_reader_page(&mut reader, &mut input, true).unwrap());
    assert_eq!(
        input.discard_taps_calls, 0,
        "a fast turn must keep every press"
    );
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
    // One series per card: pagination counts cards, not chapters.
    for i in 0..capacity + 2 {
        make_cbz(&lib.join(format!("Series {i:02}/vol1.cbz")), 1);
    }

    let events = vec![tap_row(0), tap_nav_next(), tap_nav_next(), tap_nav_prev()];
    let mut app = app(&lib, FakeGateway::default(), events);
    app.run().unwrap();

    // Two pages: next, next (clamped), prev -> page 0.
    let Screen::Library { page, items } = app.screen() else {
        panic!("expected library screen");
    };
    assert_eq!(items.len(), capacity + 2);
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

// --- shelf grouping: one card per series ---

#[test]
fn three_chapters_of_one_series_make_one_card() {
    let dir = tempfile::tempdir().unwrap();
    let lib = dir.path().join("Manga");
    make_cbz(&lib.join("Series/vol1.cbz"), 2);
    make_cbz(&lib.join("Series/vol2.cbz"), 2);
    make_cbz(&lib.join("Series/vol3.cbz"), 2);

    let mut app = app(&lib, FakeGateway::default(), vec![tap_row(0)]);
    app.run().unwrap();

    let Screen::Library { items, .. } = app.screen() else {
        panic!("expected library screen");
    };
    assert_eq!(items.len(), 1, "chapters must not flood the shelf");
    assert_eq!(items[0].title(), "Series");
    assert_eq!(items[0].chapters.len(), 3);
}

#[test]
fn returning_to_the_library_rescans_for_newly_downloaded_chapters() {
    // The "I just read 209 but the cover opens 139" bug: a chapter downloaded
    // while the library sat on the nav stack must be in the card when you back
    // out — otherwise resume_chapter runs on a stale card that can't find what
    // you just read and falls back to an earlier chapter.
    let dir = tempfile::tempdir().unwrap();
    let lib = dir.path().join("Manga");
    make_cbz(&lib.join("Series/vol1.cbz"), 2);

    let mut app = app(&lib, FakeGateway::default(), vec![]);
    app.open_library().unwrap();
    // Something on top of the library (e.g. a chapter list while reading).
    app.stack.push(Screen::Settings);

    // A new chapter lands on disk while the library is buried on the stack.
    make_cbz(&lib.join("Series/vol2.cbz"), 2);

    app.pop().unwrap(); // back to the library — must rescan

    let Screen::Library { items, .. } = app.screen() else {
        panic!("expected library screen");
    };
    assert_eq!(items.len(), 1);
    assert_eq!(
        items[0].chapters.len(),
        2,
        "the newly-downloaded chapter is in the card after returning"
    );
}

#[test]
fn tapping_a_series_card_resumes_the_in_progress_chapter() {
    let dir = tempfile::tempdir().unwrap();
    let lib = dir.path().join("Manga");
    make_cbz(&lib.join("Series/vol1.cbz"), 2);
    make_cbz(&lib.join("Series/vol2.cbz"), 3);
    make_cbz(&lib.join("Series/vol3.cbz"), 2);
    // vol1 was finished more recently than vol2 was left half-read: the
    // tap must reopen vol2 (most recently read UNFINISHED), not vol1 or
    // vol3. Timestamps are hand-written — ProgressStore::update always
    // stamps "now", which the test can't order.
    let progress_file = progress_path(&lib);
    std::fs::create_dir_all(progress_file.parent().unwrap()).unwrap();
    std::fs::write(
        &progress_file,
        r#"{"progress":{
            "Series/vol1.cbz":{"current_page":1,"total_pages":2,"last_read_at":200},
            "Series/vol2.cbz":{"current_page":1,"total_pages":3,"last_read_at":100}
        },"last_opened":{"Series":"Series/vol2.cbz"}}"#,
    )
    .unwrap();

    let events = vec![
        tap_row(0),
        tap_shelf_cell0(),
        reader_tap_next(), // vol2: page 1 -> 2
        reader_tap_back(),
    ];
    let mut app = app(&lib, FakeGateway::default(), events);
    app.run().unwrap();

    let store = ProgressStore::load(&progress_file).unwrap();
    assert_eq!(
        store.get("Series/vol2.cbz").unwrap().current_page,
        2,
        "the in-progress chapter must be the one that opened"
    );
    assert_eq!(
        store.get("Series/vol1.cbz").unwrap().current_page,
        1,
        "the finished chapter stays untouched"
    );
    assert!(
        store.get("Series/vol3.cbz").is_none(),
        "the unread chapter was not opened"
    );
}

#[test]
fn resume_honors_stored_last_opened_over_any_timestamp() {
    // The reported bug: a tap jumped to a far-earlier chapter. The explicit
    // last-opened record is authoritative — even if an earlier chapter carries
    // a newer last_read_at (clock skew, or a save that landed late), the tap
    // opens the chapter actually last opened.
    let dir = tempfile::tempdir().unwrap();
    let lib = dir.path().join("Manga");
    make_cbz(&lib.join("Series/vol1.cbz"), 2);
    make_cbz(&lib.join("Series/vol2.cbz"), 2);
    make_cbz(&lib.join("Series/vol3.cbz"), 2);
    let progress_file = progress_path(&lib);
    std::fs::create_dir_all(progress_file.parent().unwrap()).unwrap();
    std::fs::write(
        &progress_file,
        r#"{"progress":{
            "Series/vol1.cbz":{"current_page":0,"total_pages":2,"last_read_at":9999},
            "Series/vol3.cbz":{"current_page":0,"total_pages":2,"last_read_at":1}
        },"last_opened":{"Series":"Series/vol3.cbz"}}"#,
    )
    .unwrap();

    let cell = tap_shelf_cell0();
    let UiEvent::Tap { x, y } = cell else {
        unreachable!()
    };
    let events = vec![tap_row(0), UiEvent::LongPress { x, y }];
    let mut app = app(&lib, FakeGateway::default(), events);
    app.run().unwrap();

    let Screen::BookMenu { entry, .. } = app.screen() else {
        panic!("expected the book menu");
    };
    assert_eq!(
        entry.relative_path, "Series/vol3.cbz",
        "resume opens the stored last-opened chapter, not the newest timestamp"
    );
}

#[test]
fn resuming_a_series_still_flows_into_the_next_chapter() {
    // Continuous reading from a resumed chapter: finishing vol1 (the
    // resume target — no progress yet means "start at the first") flows
    // into vol2 within the same card.
    let dir = tempfile::tempdir().unwrap();
    let lib = dir.path().join("Manga");
    make_cbz(&lib.join("Series/vol1.cbz"), 2);
    make_cbz(&lib.join("Series/vol2.cbz"), 2);

    let events = vec![
        tap_row(0),
        tap_shelf_cell0(),
        reader_tap_next(), // vol1 page 2 (last)
        reader_tap_next(), // past the end -> vol2 opens
        reader_tap_back(),
    ];
    let mut app = app(&lib, FakeGateway::default(), events);
    app.run().unwrap();

    let store = ProgressStore::load(&progress_path(&lib)).unwrap();
    assert!(
        store.get("Series/vol2.cbz").is_some(),
        "reading continued into the card's next chapter"
    );
}

#[test]
fn sideloaded_loose_file_still_gets_a_card() {
    let dir = tempfile::tempdir().unwrap();
    let lib = dir.path().join("Manga");
    make_cbz(&lib.join("Series/vol1.cbz"), 2);
    make_cbz(&lib.join("loose.cbz"), 2);

    let mut app = app(&lib, FakeGateway::default(), vec![tap_row(0)]);
    app.run().unwrap();

    let Screen::Library { items, .. } = app.screen() else {
        panic!("expected library screen");
    };
    let titles: Vec<String> = items.iter().map(|c| c.title()).collect();
    assert_eq!(titles, vec!["loose".to_string(), "Series".to_string()]);
    assert!(items[0].series.is_none(), "loose files are their own card");
}

#[test]
fn book_menu_targets_the_chapter_a_tap_would_open() {
    // Long press opens the BookMenu on the card's resume chapter, so "Delete
    // this chapter" removes what a tap would show. vol1 is finished (read most
    // recently, at=200), so the resume target is the most-recent UNFINISHED
    // chapter, vol2 — while "mark as unread" clears vol1, the latest read.
    let dir = tempfile::tempdir().unwrap();
    let lib = dir.path().join("Manga");
    make_cbz(&lib.join("Series/vol1.cbz"), 2);
    make_cbz(&lib.join("Series/vol2.cbz"), 3);
    let progress_file = progress_path(&lib);
    std::fs::create_dir_all(progress_file.parent().unwrap()).unwrap();
    std::fs::write(
        &progress_file,
        r#"{"progress":{
            "Series/vol1.cbz":{"current_page":1,"total_pages":2,"last_read_at":200},
            "Series/vol2.cbz":{"current_page":1,"total_pages":3,"last_read_at":100}
        },"last_opened":{"Series":"Series/vol2.cbz"}}"#,
    )
    .unwrap();

    let cell = tap_shelf_cell0();
    let UiEvent::Tap { x, y } = cell else {
        unreachable!()
    };
    let events = vec![tap_row(0), UiEvent::LongPress { x, y }];
    let mut app = app(&lib, FakeGateway::default(), events);
    app.run().unwrap();

    let Screen::BookMenu {
        entry,
        series_dir,
        read_key,
    } = app.screen()
    else {
        panic!("expected the book menu");
    };
    assert_eq!(entry.relative_path, "Series/vol2.cbz");
    assert_eq!(series_dir, "Series");
    assert_eq!(read_key.as_deref(), Some("Series/vol1.cbz"));
}

#[test]
fn cover_tap_without_a_record_resumes_the_furthest_read_chapter() {
    // No last_opened record (an old library, just upgraded): the fallback must
    // open the FURTHEST chapter read — where you are in the series — not an
    // earlier one that happens to carry a newer timestamp. This is the
    // "I'm on 209 but it opens 139" bug: vol1 was touched most recently
    // (at=300) but vol3 is further along, so the tap opens vol3.
    let dir = tempfile::tempdir().unwrap();
    let lib = dir.path().join("Manga");
    make_cbz(&lib.join("Series/vol1.cbz"), 2);
    make_cbz(&lib.join("Series/vol2.cbz"), 5);
    make_cbz(&lib.join("Series/vol3.cbz"), 4);
    let progress_file = progress_path(&lib);
    std::fs::create_dir_all(progress_file.parent().unwrap()).unwrap();
    std::fs::write(
        &progress_file,
        r#"{"progress":{
            "Series/vol1.cbz":{"current_page":1,"total_pages":2,"last_read_at":300},
            "Series/vol3.cbz":{"current_page":2,"total_pages":4,"last_read_at":100}
        }}"#,
    )
    .unwrap();

    let cell = tap_shelf_cell0();
    let UiEvent::Tap { x, y } = cell else {
        unreachable!()
    };
    let events = vec![tap_row(0), UiEvent::LongPress { x, y }];
    let mut app = app(&lib, FakeGateway::default(), events);
    app.run().unwrap();

    let Screen::BookMenu { entry, .. } = app.screen() else {
        panic!("expected the book menu");
    };
    assert_eq!(
        entry.relative_path, "Series/vol3.cbz",
        "resumes the furthest chapter read, not the more-recently-touched earlier one"
    );
}

/// Write a solid-red series cover where the shelf looks for it.
fn make_red_cover(series_dir: &Path) {
    std::fs::create_dir_all(series_dir).unwrap();
    let img = image::RgbImage::from_pixel(30, 40, image::Rgb([255, 0, 0]));
    image::DynamicImage::ImageRgb8(img)
        .save_with_format(series_dir.join(".cover.jpg"), image::ImageFormat::Jpeg)
        .unwrap();
}

#[test]
fn library_with_cover_art_renders_in_color() {
    let dir = tempfile::tempdir().unwrap();
    let lib = dir.path().join("Manga");
    make_cbz(&lib.join("Series/vol1.cbz"), 2);
    make_red_cover(&lib.join("Series"));

    let mut app = app(&lib, FakeGateway::default(), vec![tap_row(0)]);
    app.run().unwrap();
    assert!(matches!(app.screen(), Screen::Library { .. }));

    // The shelf went through blit_rgb: MemoryDisplay collapses it with
    // Rec.601 luma, so a red cover lands at ~76 — the grayscale path
    // (the image crate's BT.709 weights) would give ~54.
    let l = layout();
    let shelf = ShelfLayout::new(l.width, l.content_height(), SHELF_COLUMNS);
    let (cx, cy) = shelf.cell_origin(0);
    let cover_h = shelf.cell_height() - shelf.title_height - shelf.progress_bar_height;
    let px = app.display().pixel(
        cx + shelf.cell_width() / 2,
        l.content_top() + cy + cover_h / 2,
    );
    assert!(
        (66..=86).contains(&px),
        "expected the Rec.601 luma of red (~76) from the RGB path, got {px}"
    );
    assert_eq!(
        app.display().blits.last(),
        Some(&true),
        "the color shelf must arrive via blit_rgb"
    );
    // Color shelves always flush in full, so the Kaleido color waveform
    // (GCC16, FULL-only) can fire.
    assert_eq!(app.display().flushes.last(), Some(&RefreshMode::Full));
}

#[test]
fn color_library_page_flips_stay_partial() {
    let dir = tempfile::tempdir().unwrap();
    let lib = dir.path().join("Manga");
    let l = layout();
    let capacity = ShelfLayout::new(l.width, l.content_height(), SHELF_COLUMNS).capacity();
    // One covered series per card, enough cards for a second shelf page.
    for i in 0..capacity + 1 {
        make_cbz(&lib.join(format!("Series {i:02}/vol1.cbz")), 1);
        make_red_cover(&lib.join(format!("Series {i:02}")));
    }

    let events = vec![tap_row(0), tap_nav_next()];
    let mut app = app(&lib, FakeGateway::default(), events);
    app.run().unwrap();

    // Color page flips pass the caller's Partial through: the MTK driver
    // runs them on the NON-flashing color waveform (GLRC16), so the shelf
    // doesn't flash on every flip.
    assert_eq!(app.display().flushes.last(), Some(&RefreshMode::Partial));
}

#[test]
fn shelf_covers_are_cached_across_repaints() {
    let dir = tempfile::tempdir().unwrap();
    let lib = dir.path().join("Manga");
    make_cbz(&lib.join("Series/vol1.cbz"), 1);
    make_red_cover(&lib.join("Series"));
    let cover = lib.join("Series/.cover.jpg");

    let app = app(&lib, FakeGateway::default(), vec![]);
    let entry = LibraryEntry {
        path: lib.join("Series/vol1.cbz"),
        relative_path: "Series/vol1.cbz".to_string(),
    };
    let cell = (60, 80);
    let first = app.shelf_cover(&entry, cell, 6);
    assert!(
        first.width() <= cell.0 && first.height() <= cell.1,
        "the cache holds cell-sized thumbnails, not full decodes"
    );

    // Replace the cover with garbage but keep its mtime: a cache hit keeps
    // serving the old pixels, a re-decode would fall back elsewhere.
    let mtime =
        filetime::FileTime::from_last_modification_time(&std::fs::metadata(&cover).unwrap());
    std::fs::write(&cover, b"not a jpeg").unwrap();
    filetime::set_file_mtime(&cover, mtime).unwrap();
    assert_eq!(
        app.shelf_cover(&entry, cell, 6),
        first,
        "an unchanged mtime must serve the cached cover, not re-decode"
    );

    // Bumping the mtime invalidates the cache entry: the garbage file
    // fails to decode and the cover falls back (here: the CBZ's page).
    filetime::set_file_mtime(&cover, filetime::FileTime::from_unix_time(99, 0)).unwrap();
    assert_ne!(
        app.shelf_cover(&entry, cell, 6),
        first,
        "a changed mtime must re-decode the cover"
    );
}

#[test]
fn shelf_cover_cache_evicts_lru_not_wholesale() {
    let dir = tempfile::tempdir().unwrap();
    let lib = dir.path().join("Manga");
    // Shelf capacity 2 → the cache budget is 4 entries (two pages).
    for i in 0..5 {
        make_cbz(&lib.join(format!("S{i}/vol1.cbz")), 1);
        make_red_cover(&lib.join(format!("S{i}")));
    }
    let app = app(&lib, FakeGateway::default(), vec![]);
    let entry = |i: usize| LibraryEntry {
        path: lib.join(format!("S{i}/vol1.cbz")),
        relative_path: format!("S{i}/vol1.cbz"),
    };
    let cell = (60, 80);
    for i in 0..5 {
        app.shelf_cover(&entry(i), cell, 2);
    }

    let cache = app.cover_cache.borrow();
    assert_eq!(cache.entries.len(), 4, "budget is two shelf pages");
    let cached = |i: usize| {
        cache
            .entries
            .keys()
            .any(|(path, ..)| path.ends_with(format!("S{i}/.cover.jpg")))
    };
    assert!(!cached(0), "only the least recently used entry is evicted");
    for i in 1..5 {
        assert!(cached(i), "recently used entry S{i} must stay warm");
    }
}

#[test]
fn library_without_covers_stays_on_the_grayscale_path() {
    let dir = tempfile::tempdir().unwrap();
    let lib = dir.path().join("Manga");
    make_cbz(&lib.join("Series/vol1.cbz"), 2);

    let mut app = app(&lib, FakeGateway::default(), vec![tap_row(0)]);
    app.run().unwrap();

    // The CBZ's first page is gray; nothing here may take the color path
    // (covers come only from downloaded .cover.jpg art).
    assert!(matches!(app.screen(), Screen::Library { .. }));
    assert!(app
        .display()
        .flushes
        .iter()
        .all(|m| *m == RefreshMode::Full));
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
    let mut app = app(dir.path(), gateway, vec![tap_row(4)]);
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
    // Row 0 is the Wi-Fi toggle now; Restart is 1, Close is 2.
    let events = vec![tap_power_icon(), tap_row(2)];
    let mut app = app(dir.path(), FakeGateway::default(), events);
    assert_eq!(app.run().unwrap(), Exit::Close);
}

#[test]
fn power_menu_restart_requests_restart() {
    let dir = tempfile::tempdir().unwrap();
    let events = vec![tap_power_icon(), tap_row(1)];
    let mut app = app(dir.path(), FakeGateway::default(), events);
    assert_eq!(app.run().unwrap(), Exit::Restart);
}

#[test]
fn predownload_targets_picks_the_next_unread_chapters() {
    // The selection logic (run on the UI thread, then handed to the worker):
    // the default "Pre-download ahead" is 2, so from c1 the targets are c2, c3.
    let dir = tempfile::tempdir().unwrap();
    let lib = dir.path().join("Manga");
    std::fs::create_dir_all(&lib).unwrap();
    let app = app(&lib, FakeGateway::default(), vec![]);

    let source = SourceEntry {
        id: "src".into(),
        name: "Src".into(),
    };
    let manga = MangaEntry {
        id: "m1".into(),
        title: "Manga One".into(),
        cover_url: None,
    };
    let chapters: Vec<ChapterEntry> = (1..=4)
        .map(|i| ChapterEntry {
            id: format!("c{i}"),
            num: Some(i as f32),
            title: None,
            lang: Some("en".into()),
        })
        .collect();

    let targets = app.predownload_targets(&source, &manga, &chapters, "c1");
    let ids: Vec<&str> = targets.iter().map(|c| c.id.as_str()).collect();
    assert_eq!(ids, vec!["c2", "c3"], "two ahead of c1, not c1 or c4");
}

#[test]
fn predownload_window_does_not_march_through_the_series() {
    // The infinite-download bug: re-triggering the look-ahead from the same
    // chapter must NOT walk further into the series as chapters get stored.
    let dir = tempfile::tempdir().unwrap();
    let lib = dir.path().join("Manga");
    std::fs::create_dir_all(&lib).unwrap();
    let app = app(&lib, FakeGateway::default(), vec![]);

    let source = SourceEntry {
        id: "src".into(),
        name: "Src".into(),
    };
    let manga = MangaEntry {
        id: "m1".into(),
        title: "Manga One".into(),
        cover_url: None,
    };
    let chapters: Vec<ChapterEntry> = (1..=6)
        .map(|i| ChapterEntry {
            id: format!("c{i}"),
            num: Some(i as f32),
            title: None,
            lang: Some("en".into()),
        })
        .collect();

    // From c1, the window (2) is c2, c3.
    let first = app.predownload_targets(&source, &manga, &chapters, "c1");
    let ids: Vec<&str> = first.iter().map(|c| c.id.as_str()).collect();
    assert_eq!(ids, vec!["c2", "c3"]);

    // Those two get stored.
    let guard = std::sync::Mutex::new(());
    for id in ["c2", "c3"] {
        let path = lib.join(format!("Manga One/{id}.cbz"));
        make_cbz(&path, 2);
        record_chapter_in_index(&lib, &guard, &source, &manga, id, &path);
    }

    // Re-trigger from the SAME chapter: the window is satisfied, so nothing new
    // — it must NOT march on to c4, c5.
    let second = app.predownload_targets(&source, &manga, &chapters, "c1");
    assert!(
        second.is_empty(),
        "window stays anchored at c1; it never marches to c4/c5 (got {:?})",
        second.iter().map(|c| &c.id).collect::<Vec<_>>()
    );
}

/// A minimal `Send + Clone` gateway whose `background_clone` returns a working
/// copy — so the background pre-download worker actually runs. `download_chapter`
/// writes a CBZ named after the chapter id under the manga's directory.
///
/// `started` (if set) is signalled with the chapter id as each download begins,
/// and `delay_ms` holds the download open afterward — together they let a test
/// catch the worker mid-chapter (e.g. to cancel the rest of the queue).
#[derive(Clone)]
struct BgGateway {
    manga_dir: String,
    pages: usize,
    delay_ms: u64,
    started: Option<std::sync::mpsc::Sender<String>>,
}

impl BgGateway {
    fn new(manga_dir: &str, pages: usize) -> Self {
        Self {
            manga_dir: manga_dir.into(),
            pages,
            delay_ms: 0,
            started: None,
        }
    }
}

impl SourceGateway for BgGateway {
    fn installed_sources(&self) -> Result<Vec<SourceEntry>> {
        Ok(vec![])
    }
    fn available_sources(&self) -> Result<Vec<SourceEntry>> {
        Ok(vec![])
    }
    fn install_source(&self, _source_id: &str) -> Result<()> {
        Ok(())
    }
    fn uninstall_source(&self, _source_id: &str) -> Result<()> {
        Ok(())
    }
    fn list_manga(&self, _source_id: &str, _listing: &str) -> Result<Vec<MangaEntry>> {
        Ok(vec![])
    }
    fn search_manga(&self, _source_id: &str, _query: &str) -> Result<Vec<MangaEntry>> {
        Ok(vec![])
    }
    fn download_cover(&self, _url: &str, _dest: &Path) -> Result<()> {
        Ok(())
    }
    fn chapters(&self, _source_id: &str, _manga_id: &str) -> Result<Vec<ChapterEntry>> {
        Ok(vec![])
    }
    fn download_chapter(
        &self,
        _source_id: &str,
        _manga_id: &str,
        chapter_id: &str,
        library: &Path,
        progress: &mut dyn FnMut(usize, usize),
    ) -> Result<PathBuf> {
        if let Some(tx) = &self.started {
            let _ = tx.send(chapter_id.to_string());
        }
        if self.delay_ms > 0 {
            std::thread::sleep(std::time::Duration::from_millis(self.delay_ms));
        }
        let path = library
            .join(&self.manga_dir)
            .join(format!("{chapter_id}.cbz"));
        make_cbz(&path, self.pages);
        progress(self.pages, self.pages);
        Ok(path)
    }
    fn background_clone(&self) -> Option<Box<dyn SourceGateway + Send>> {
        Some(Box::new(self.clone()))
    }
    fn check_updates(&self) -> Result<super::gateway::UpdateCheck> {
        Ok(super::gateway::UpdateCheck {
            message: String::new(),
            available: false,
        })
    }
    fn install_update(&self) -> Result<String> {
        Ok(String::new())
    }
}

#[test]
fn predownload_runs_in_the_background_without_blocking() {
    let dir = tempfile::tempdir().unwrap();
    let lib = dir.path().join("Manga");
    std::fs::create_dir_all(&lib).unwrap();

    let gateway = BgGateway::new("Manga One", 3);
    let mut app = UiApp::new(
        MemoryDisplay::new(W, H),
        FakeInput::new(vec![]),
        gateway,
        lib.clone(),
    );

    let source = SourceEntry {
        id: "src".into(),
        name: "Src".into(),
    };
    let manga = MangaEntry {
        id: "m1".into(),
        title: "Manga One".into(),
        cover_url: None,
    };
    let chapters: Vec<ChapterEntry> = (1..=4)
        .map(|i| ChapterEntry {
            id: format!("c{i}"),
            num: Some(i as f32),
            title: None,
            lang: Some("en".into()),
        })
        .collect();

    // Returns immediately — the next two chapters (c2, c3) are queued onto the
    // worker thread, not downloaded inline.
    app.predownload_ahead(&source, &manga, &chapters, "c1");

    // The worker fetches them on its own thread; give it a moment.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        if app.downloaded_chapter_path(&source, &manga, "c2").is_some()
            && app.downloaded_chapter_path(&source, &manga, "c3").is_some()
        {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }

    assert!(
        app.downloaded_chapter_path(&source, &manga, "c2").is_some(),
        "c2 was pre-downloaded in the background"
    );
    assert!(
        app.downloaded_chapter_path(&source, &manga, "c3").is_some(),
        "c3 was pre-downloaded in the background"
    );
    assert!(
        app.downloaded_chapter_path(&source, &manga, "c4").is_none(),
        "only 2 chapters ahead are fetched"
    );
    assert!(
        app.downloaded_chapter_path(&source, &manga, "c1").is_none(),
        "the current chapter is not re-fetched"
    );
}

#[test]
fn leaving_a_manga_cancels_its_queued_pre_downloads() {
    // The bug: after you leave a manga, its queued look-ahead kept downloading
    // in the background. Now popping the chapter list cancels everything not yet
    // started. We catch the worker mid-c2, cancel, and assert c3/c4 never land.
    let dir = tempfile::tempdir().unwrap();
    let lib = dir.path().join("Manga");
    std::fs::create_dir_all(&lib).unwrap();

    let (started_tx, started_rx) = std::sync::mpsc::channel::<String>();
    let gateway = BgGateway {
        manga_dir: "Manga One".into(),
        pages: 2,
        delay_ms: 300, // hold c2 open long enough to cancel the rest
        started: Some(started_tx),
    };
    let mut app = UiApp::new(
        MemoryDisplay::new(W, H),
        FakeInput::new(vec![]),
        gateway,
        lib.clone(),
    );

    let source = SourceEntry {
        id: "src".into(),
        name: "Src".into(),
    };
    let manga = MangaEntry {
        id: "m1".into(),
        title: "Manga One".into(),
        cover_url: None,
    };

    // Stand inside the manga's chapter list, then queue c2, c3, c4 ahead.
    assert!(app.ensure_predownloader());
    let epoch = app.predownloader.as_ref().unwrap().epoch();
    for id in ["c2", "c3", "c4"] {
        app.predownloader.as_mut().unwrap().queue(PreloadJob {
            source: source.clone(),
            manga: manga.clone(),
            chapter_id: id.into(),
            epoch,
        });
    }
    app.stack.push(Screen::ChapterList {
        source: source.clone(),
        manga: manga.clone(),
        chapters: vec![],
        page: 0,
    });

    // The worker has begun c2 — leave the manga while it's still downloading.
    assert_eq!(
        started_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap(),
        "c2"
    );
    app.pop().unwrap(); // pops the chapter list → cancels the queued rest

    // c2 (already in flight) finishes; c3/c4 are dropped. Wait for c2 to land,
    // then give the worker ample time to (not) fetch the cancelled ones.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    while std::time::Instant::now() < deadline
        && app.downloaded_chapter_path(&source, &manga, "c2").is_none()
    {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    std::thread::sleep(std::time::Duration::from_millis(300));

    assert!(
        app.downloaded_chapter_path(&source, &manga, "c2").is_some(),
        "the chapter already downloading when you left still completes"
    );
    assert!(
        app.downloaded_chapter_path(&source, &manga, "c3").is_none(),
        "leaving the manga cancels the not-yet-started look-ahead"
    );
    assert!(
        app.downloaded_chapter_path(&source, &manga, "c4").is_none(),
        "leaving the manga cancels the not-yet-started look-ahead"
    );
}

#[test]
fn series_without_a_source_link_shows_downloaded_chapters() {
    // Side-loaded files (no SeriesIndex origin): opening the series must list
    // what's on disk instead of reaching for a source — the offline path.
    let dir = tempfile::tempdir().unwrap();
    let lib = dir.path().join("Manga");
    make_cbz(&lib.join("Series/vol1.cbz"), 2);
    make_cbz(&lib.join("Series/vol2.cbz"), 3);

    let mut app = app(&lib, FakeGateway::default(), vec![]);
    app.open_series_chapters("Series").unwrap();

    let Screen::DownloadedChapters { entries, title, .. } = app.screen() else {
        panic!("expected the downloaded-chapters list");
    };
    assert_eq!(title, "Series");
    let rel: Vec<&str> = entries.iter().map(|e| e.relative_path.as_str()).collect();
    assert_eq!(rel, vec!["Series/vol1.cbz", "Series/vol2.cbz"]);
}

#[test]
fn tapping_a_downloaded_chapter_opens_it_offline() {
    let dir = tempfile::tempdir().unwrap();
    let lib = dir.path().join("Manga");
    make_cbz(&lib.join("Series/vol1.cbz"), 2);
    make_cbz(&lib.join("Series/vol2.cbz"), 2);

    // Read vol1: one page forward, then back out (no source involved).
    let events = vec![reader_tap_next(), reader_tap_back()];
    let mut app = app(&lib, FakeGateway::default(), events);
    app.open_downloaded_chapters("Series").unwrap();

    let UiEvent::Tap { x, y } = tap_row(0) else {
        unreachable!()
    };
    app.activate(0, x, y).unwrap();

    let store = ProgressStore::load(&progress_path(&lib)).unwrap();
    assert!(
        store.get("Series/vol1.cbz").is_some(),
        "tapping a downloaded chapter opened it and recorded progress"
    );
    // Still on the offline list afterward (the reader returned Back).
    assert!(matches!(app.screen(), Screen::DownloadedChapters { .. }));
}

/// A tap on the ⋮ button of chapter row `i` (right edge of the row).
fn tap_row_kebab(i: usize) -> (u32, u32) {
    let l = layout();
    (l.width - 2, l.row_top(i) + l.row_h / 2)
}

#[test]
fn chapter_kebab_opens_read_menu_and_toggles_read_state() {
    let dir = tempfile::tempdir().unwrap();
    let lib = dir.path().join("Manga");
    make_cbz(&lib.join("Series/vol1.cbz"), 3);
    let key = "Series/vol1.cbz";
    let mut app = app(&lib, FakeGateway::default(), vec![]);
    app.open_downloaded_chapters("Series").unwrap();

    // ⋮ on row 0 opens the read-status menu (does NOT open the reader).
    let (kx, ky) = tap_row_kebab(0);
    app.activate(0, kx, ky).unwrap();
    assert!(
        matches!(app.screen(), Screen::ChapterMenu { .. }),
        "the ⋮ button opens the read menu"
    );

    // Row 0 = "Mark as read" → finished.
    let UiEvent::Tap { x, y } = tap_row(0) else {
        unreachable!()
    };
    app.activate(0, x, y).unwrap();
    let store = ProgressStore::load(&progress_path(&lib)).unwrap();
    assert!(
        store.get(key).is_some_and(|p| p.is_finished()),
        "Mark as read records the chapter as finished"
    );
    assert!(matches!(app.screen(), Screen::DownloadedChapters { .. }));

    // ⋮ again, then row 1 = "Mark as unread" → progress cleared.
    app.activate(0, kx, ky).unwrap();
    app.activate(1, x, y).unwrap();
    let store = ProgressStore::load(&progress_path(&lib)).unwrap();
    assert!(
        store.get(key).is_none(),
        "Mark as unread clears the chapter's progress"
    );
}

fn wifi_net(ssid: &str, secured: bool, saved: bool) -> gideon_device::network::WifiNetwork {
    gideon_device::network::WifiNetwork {
        ssid: ssid.into(),
        signal: -50,
        secured,
        saved,
        connected: false,
    }
}

#[test]
fn wifi_list_tap_secured_network_asks_for_password() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app(dir.path(), FakeGateway::default(), vec![]);
    app.stack.push(Screen::WifiList {
        networks: vec![wifi_net("HomeNet", true, false)],
    });
    // Row 0 is the Wi-Fi toggle now; the first network is row 1.
    app.activate(1, 10, 10).unwrap();
    assert!(
        matches!(app.screen(), Screen::WifiPassword { ssid, .. } if ssid.as_str() == "HomeNet"),
        "tapping a new secured network opens the password keyboard"
    );
}

#[test]
fn wifi_list_toggle_off_returns_to_previous_screen() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app(dir.path(), FakeGateway::default(), vec![]);
    // [Home, WifiList]; rows: Wi-Fi toggle(0), net(1), "Scan again"(2).
    app.stack.push(Screen::WifiList {
        networks: vec![wifi_net("X", true, false)],
    });
    app.activate(0, 10, 10).unwrap();
    assert!(
        matches!(app.screen(), Screen::Home),
        "flipping the Wi-Fi toggle off pops back to the previous screen"
    );
}

#[test]
fn wifi_toggle_off_closes_the_whole_menu_from_the_power_menu() {
    // Opened via Power → Wi-Fi, toggling off should return all the way to the
    // library, not leave you sitting on the Power menu.
    let dir = tempfile::tempdir().unwrap();
    let mut app = app(dir.path(), FakeGateway::default(), vec![]);
    app.stack.push(Screen::PowerMenu);
    app.stack.push(Screen::WifiList {
        networks: vec![wifi_net("X", true, false)],
    });
    app.activate(0, 10, 10).unwrap();
    assert!(
        matches!(app.screen(), Screen::Home),
        "the Wi-Fi toggle closes the entire menu stack back to Home"
    );
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

    let Screen::Library { items, .. } = app.screen() else {
        panic!("expected alex's library");
    };
    let titles: Vec<String> = items.iter().map(|c| c.title()).collect();
    assert_eq!(titles, vec!["Alexs Series".to_string()]);
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

    let Screen::Library { items, .. } = app.screen() else {
        panic!("expected the default library");
    };
    let titles: Vec<String> = items.iter().map(|c| c.title()).collect();
    assert_eq!(titles, vec!["Shared".to_string()]);
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
    let Screen::Library { items, .. } = app.screen() else {
        panic!("expected the new profile's (empty) library");
    };
    assert!(items.is_empty());
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

// --- settings screen ---

#[test]
fn settings_rows_cycle_and_persist_to_disk() {
    let dir = tempfile::tempdir().unwrap();
    let settings_dir = dir.path().join("data");
    gideon_core::Settings::default()
        .save(&settings_dir)
        .unwrap();

    let events = vec![
        tap_row(3), // Home -> Settings
        tap_row(0), // pre-download 2 -> 3
        tap_row(0), // 3 -> 5
        tap_row(1), // storage 2 GB -> 5 GB
        tap_row(3), // auto-check on -> off
        tap_back(),
    ];
    let mut app =
        app(dir.path(), FakeGateway::default(), events).with_settings_dir(settings_dir.clone());
    app.run().unwrap();

    assert!(matches!(app.screen(), Screen::Home));
    let settings = gideon_core::Settings::load(&settings_dir).unwrap();
    assert_eq!(settings.predownload_unread_chapters, 5);
    assert_eq!(
        settings.storage_size_limit.bytes(),
        5 * 1024 * 1024 * 1024,
        "2 GB cycles to 5 GB"
    );
    assert!(!settings.auto_check_updates);
    // Value cycles repaint in place with partial refreshes.
    assert!(app.display().flushes.contains(&RefreshMode::Partial));
}

#[test]
fn storage_limit_cycle_wraps_around() {
    let dir = tempfile::tempdir().unwrap();
    let settings_dir = dir.path().join("data");
    gideon_core::Settings {
        storage_size_limit: gideon_core::StorageSize(5 * 1024 * 1024 * 1024),
        ..gideon_core::Settings::default()
    }
    .save(&settings_dir)
    .unwrap();

    let events = vec![tap_row(3), tap_row(1)];
    let mut app =
        app(dir.path(), FakeGateway::default(), events).with_settings_dir(settings_dir.clone());
    app.run().unwrap();

    let settings = gideon_core::Settings::load(&settings_dir).unwrap();
    assert_eq!(
        settings.storage_size_limit.bytes(),
        500 * 1024 * 1024,
        "5 GB wraps back to 500 MB"
    );
}

#[test]
fn reader_fit_toggle_applies_to_the_next_book_immediately() {
    let dir = tempfile::tempdir().unwrap();
    let lib = dir.path().join("Manga");
    let settings_dir = dir.path().join("data");
    gideon_core::Settings::default()
        .save(&settings_dir)
        .unwrap();
    make_tall_cbz(&lib.join("Tall/vol1.cbz"), 2);

    // Toggle contain -> fit-width, then open a tall page: a "next" tap
    // must scroll within the page (no page turn), without a restart.
    let events = vec![
        tap_row(3), // Settings
        tap_row(2), // Reader fit: contain -> fit-width
        tap_back(), // Home
        tap_row(0), // Library
        tap_shelf_cell0(),
        reader_tap_next(),
        reader_tap_back(),
    ];
    let mut app = app(&lib, FakeGateway::default(), events).with_settings_dir(settings_dir.clone());
    app.run().unwrap();

    let settings = gideon_core::Settings::load(&settings_dir).unwrap();
    assert_eq!(settings.reader_fit, "fit-width");
    let store = ProgressStore::load(&progress_path(&lib)).unwrap();
    assert_eq!(
        store.get("Tall/vol1.cbz").unwrap().current_page,
        0,
        "the reader must pick up the new fit immediately (scroll, not turn)"
    );
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

// --- chapter continuation ---

#[test]
fn finishing_a_chapter_flows_into_the_next() {
    let dir = tempfile::tempdir().unwrap();
    let lib = dir.path().join("Manga");
    std::fs::create_dir_all(&lib).unwrap();
    let calls = std::rc::Rc::new(std::cell::Cell::new(0usize));
    let counter = calls.clone();
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
        // Newest-first, like real sources: chapter 2 sits ABOVE chapter 1.
        chapters: vec![
            ChapterEntry {
                id: "c2".into(),
                num: Some(2.0),
                title: None,
                lang: None,
            },
            ChapterEntry {
                id: "c1".into(),
                num: Some(1.0),
                title: None,
                lang: None,
            },
        ],
        download: Some(Box::new(move |library, _| {
            counter.set(counter.get() + 1);
            let path = library.join(format!("Manga One/Chapter {}.cbz", counter.get()));
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
        tap_row(1),        // chapter 1 (second row: newest-first)
        reader_tap_next(), // page 2 (last page of chapter 1)
        reader_tap_next(), // past the end -> chapter 2 downloads + opens
        reader_tap_back(),
    ];
    let mut app = app(&lib, gateway, events);
    app.run().unwrap();

    assert_eq!(calls.get(), 2, "chapter 2 must auto-download");
    let store = ProgressStore::load(&progress_path(&lib)).unwrap();
    assert!(
        store.get("Manga One/Chapter 2.cbz").is_some(),
        "reading continued into chapter 2"
    );
    assert!(matches!(app.screen(), Screen::ChapterList { .. }));
}

#[test]
fn last_chapter_end_stays_put() {
    let dir = tempfile::tempdir().unwrap();
    let lib = dir.path().join("Manga");
    make_cbz(&lib.join("Solo/only.cbz"), 2);

    let events = vec![
        tap_row(0),
        tap_shelf_cell0(),
        reader_tap_next(), // page 2 (last)
        reader_tap_next(), // past the end, no next chapter: ignored
        reader_tap_next(), // still ignored
        reader_tap_back(),
    ];
    let mut app = app(&lib, FakeGateway::default(), events);
    app.run().unwrap();
    assert!(matches!(app.screen(), Screen::Library { .. }));
    let store = ProgressStore::load(&progress_path(&lib)).unwrap();
    assert_eq!(store.get("Solo/only.cbz").unwrap().current_page, 1);
}

#[test]
fn library_reading_continues_into_the_next_file() {
    let dir = tempfile::tempdir().unwrap();
    let lib = dir.path().join("Manga");
    make_cbz(&lib.join("Series/vol01.cbz"), 2);
    make_cbz(&lib.join("Series/vol02.cbz"), 2);

    let events = vec![
        tap_row(0),
        tap_shelf_cell0(),
        reader_tap_next(), // vol01 page 2
        reader_tap_next(), // past the end -> vol02 opens
        reader_tap_next(), // vol02 page 2
        reader_tap_back(),
    ];
    let mut app = app(&lib, FakeGateway::default(), events);
    app.run().unwrap();

    let store = ProgressStore::load(&progress_path(&lib)).unwrap();
    assert_eq!(
        store.get("Series/vol02.cbz").map(|p| p.current_page),
        Some(1),
        "vol02 was opened and read"
    );
}

#[test]
fn next_chapter_orders_by_number_not_position() {
    let ch = |id: &str, num: Option<f32>| ChapterEntry {
        id: id.into(),
        num,
        title: None,
        lang: None,
    };
    // Newest-first list: 3, 2, 1.
    let list = vec![
        ch("c3", Some(3.0)),
        ch("c2", Some(2.0)),
        ch("c1", Some(1.0)),
    ];
    assert_eq!(next_chapter(&list, "c1").map(|c| c.id), Some("c2".into()));
    assert_eq!(next_chapter(&list, "c2").map(|c| c.id), Some("c3".into()));
    assert_eq!(next_chapter(&list, "c3"), None, "no chapter after the last");
    // Without numbers: assume newest-first, step toward the front.
    let bare = vec![ch("b3", None), ch("b2", None), ch("b1", None)];
    assert_eq!(next_chapter(&bare, "b2").map(|c| c.id), Some("b3".into()));
    assert_eq!(next_chapter(&bare, "b3"), None);
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
fn unlinked_book_chapters_shows_downloaded_list() {
    // A book downloaded before origins were recorded (or sideloaded) has no
    // source to fetch — "All chapters" shows the downloaded chapters instead of
    // stranding the reader, so it works offline.
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

    let Screen::DownloadedChapters { entries, .. } = app.screen() else {
        panic!("expected the downloaded-chapters list");
    };
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].relative_path, "Sideload/vol1.cbz");
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
    // Row 2 is "Delete this chapter" (row 1 is now "Mark as unread").
    let events = vec![tap_row(0), UiEvent::LongPress { x, y }, tap_row(2)];
    let mut app = app(&lib, FakeGateway::default(), events);
    app.run().unwrap();

    let Screen::Library { items, .. } = app.screen() else {
        panic!("expected refreshed library");
    };
    assert_eq!(items.len(), 1, "the series keeps its (single) card");
    assert_eq!(
        items[0].chapters.len(),
        1,
        "one chapter deleted, one remains"
    );
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
    // Row 3 is "Delete whole series" (shifted by the new "Mark as unread" row).
    let events = vec![tap_row(0), UiEvent::LongPress { x, y }, tap_row(3)];
    let mut app = app(&lib, FakeGateway::default(), events);
    app.run().unwrap();

    let Screen::Library { items, .. } = app.screen() else {
        panic!("expected refreshed library");
    };
    assert!(items.is_empty(), "whole series gone");
    assert!(!lib.join("Series").exists());
}

#[test]
fn book_menu_marks_the_latest_read_chapter_unread() {
    // "I clicked the wrong thing" undo: vol1 was read (and finished); the menu's
    // "Mark as unread" (row 1) forgets vol1's progress, leaving vol2 untouched.
    let dir = tempfile::tempdir().unwrap();
    let lib = dir.path().join("Manga");
    make_cbz(&lib.join("Series/vol1.cbz"), 2);
    make_cbz(&lib.join("Series/vol2.cbz"), 3);
    let progress_file = progress_path(&lib);
    std::fs::create_dir_all(progress_file.parent().unwrap()).unwrap();
    std::fs::write(
        &progress_file,
        r#"{"progress":{
            "Series/vol1.cbz":{"current_page":1,"total_pages":2,"last_read_at":200},
            "Series/vol2.cbz":{"current_page":1,"total_pages":3,"last_read_at":100}
        },"last_opened":{"Series":"Series/vol2.cbz"}}"#,
    )
    .unwrap();

    let cell = tap_shelf_cell0();
    let UiEvent::Tap { x, y } = cell else {
        unreachable!()
    };
    // Long press → BookMenu, then row 1 = "Mark as unread".
    let events = vec![tap_row(0), UiEvent::LongPress { x, y }, tap_row(1)];
    let mut app = app(&lib, FakeGateway::default(), events);
    app.run().unwrap();

    let store = ProgressStore::load(&progress_file).unwrap();
    assert!(
        store.get("Series/vol1.cbz").is_none(),
        "the latest-read chapter is now unread"
    );
    assert!(
        store.get("Series/vol2.cbz").is_some(),
        "the other chapter's progress is untouched"
    );
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
    // Home row 4 = "Check for updates" -> prompt; content tap installs,
    // and a successful install restarts the app in place so the new
    // binary is live immediately.
    let mut app = app(dir.path(), gateway, vec![tap_row(4), tap_row(0)]);
    assert_eq!(app.run().unwrap(), Exit::Restart);
    assert_eq!(
        app.gateway().installs.get(),
        1,
        "tap on prompt should install"
    );
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
fn global_search_with_no_hits_opens_results_then_back_to_keyboard() {
    // No installed source matched: the results screen still opens (offering
    // "Search more sources"), and Back returns to the keyboard with the
    // query intact so it can be refined.
    let dir = tempfile::tempdir().unwrap();
    let mut gateway = search_gateway();
    gateway.search_results = Ok(Vec::new());
    let events = vec![
        tap_row(1),
        tap_key(Key::Char('z')),
        tap_key(Key::Search),
        tap_back(), // leave the (empty) results -> back to the keyboard
    ];
    let mut app = app(dir.path(), gateway, events);
    app.run().unwrap();

    let Screen::Search { query, .. } = app.screen() else {
        panic!("expected to land back on the keyboard");
    };
    assert_eq!(query, "z");
}

#[test]
fn global_search_with_a_failing_source_still_opens_results() {
    // A source that errors is skipped (logged to stderr), never fatal. Even
    // with no hits the results screen opens, so its "Search more sources"
    // row is there to widen the search.
    let dir = tempfile::tempdir().unwrap();
    let mut gateway = search_gateway();
    gateway.search_results = Err("cloudflare tantrum".into());
    let events = vec![tap_row(1), tap_key(Key::Char('a')), tap_key(Key::Search)];
    let mut app = app(dir.path(), gateway, events);
    app.run().unwrap();

    let Screen::SearchResults { results, .. } = app.screen() else {
        panic!("expected the results screen even with no hits");
    };
    assert!(results.is_empty(), "a failing source contributes nothing");
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

// --- widening to not-yet-installed sources ("Search more sources") ---

#[test]
fn widen_installs_matching_sources_and_merges_their_results() {
    // One installed source (a hit), two more available but not installed.
    // Tapping "Search more sources" pulls them in; both match, so both are
    // kept installed and their hits are merged into the results.
    let dir = tempfile::tempdir().unwrap();
    let gateway = FakeGateway {
        installed: RefCell::new(vec![SourceEntry {
            id: "src".into(),
            name: "Src".into(),
        }]),
        available: Ok(vec![
            SourceEntry {
                id: "extra1".into(),
                name: "Extra One".into(),
            },
            SourceEntry {
                id: "extra2".into(),
                name: "Extra Two".into(),
            },
        ]),
        search_results: Ok(vec![MangaEntry {
            id: "m1".into(),
            title: "Naruto".into(),
            cover_url: None,
        }]),
        ..FakeGateway::default()
    };
    let events = vec![
        tap_row(1), // Home -> global search keyboard (no history)
        tap_key(Key::Char('n')),
        tap_key(Key::Search), // -> SearchResults (1 hit from src)
        tap_row(1),           // the "Search more sources" row (index 1)
    ];
    let mut app = app(dir.path(), gateway, events);
    app.run().unwrap();

    let Screen::SearchResults { results, tried, .. } = app.screen() else {
        panic!("expected widened results");
    };
    assert_eq!(results.len(), 3, "src + extra1 + extra2 each contributed");
    // Every source was searched, none left untried.
    assert!(["src", "extra1", "extra2"]
        .iter()
        .all(|id| tried.iter().any(|t| t == id)));
    // The matching extras were kept installed; nothing was uninstalled.
    let installed: Vec<String> = app
        .gateway()
        .installed
        .borrow()
        .iter()
        .map(|s| s.id.clone())
        .collect();
    assert!(installed.contains(&"extra1".to_string()));
    assert!(installed.contains(&"extra2".to_string()));
    assert!(app.gateway().uninstalled.borrow().is_empty());
}

#[test]
fn widen_with_no_matches_uninstalls_the_sources_it_tried() {
    // Nothing matches anywhere. Widening installs the two available sources,
    // finds no hits, and removes them again — the library isn't polluted.
    let dir = tempfile::tempdir().unwrap();
    let gateway = FakeGateway {
        installed: RefCell::new(vec![SourceEntry {
            id: "src".into(),
            name: "Src".into(),
        }]),
        available: Ok(vec![
            SourceEntry {
                id: "extra1".into(),
                name: "Extra One".into(),
            },
            SourceEntry {
                id: "extra2".into(),
                name: "Extra Two".into(),
            },
        ]),
        search_results: Ok(Vec::new()),
        ..FakeGateway::default()
    };
    let events = vec![
        tap_row(1),
        tap_key(Key::Char('z')),
        tap_key(Key::Search), // -> empty SearchResults
        tap_row(0),           // the "Search more sources" row (index 0)
    ];
    let mut app = app(dir.path(), gateway, events);
    app.run().unwrap();

    // No new matches -> a message sits on top of the (still empty) results.
    let Screen::Message { title, body } = app.screen() else {
        panic!("expected a 'no new matches' message");
    };
    assert_eq!(title, "Search more");
    assert!(body.contains("no new matches"), "{body}");
    // Both tried-but-empty sources were removed again.
    let mut uninstalled = app.gateway().uninstalled.borrow().clone();
    uninstalled.sort();
    assert_eq!(
        uninstalled,
        vec!["extra1".to_string(), "extra2".to_string()]
    );
    let installed: Vec<String> = app
        .gateway()
        .installed
        .borrow()
        .iter()
        .map(|s| s.id.clone())
        .collect();
    assert_eq!(installed, vec!["src".to_string()], "library left as it was");
}

#[test]
fn widen_never_uninstalls_a_source_the_user_already_had() {
    // A reopened recent carries a `tried` from when it was cached — it can
    // predate the user installing another source. Widening from it finds no
    // new match in that already-installed source, and must NOT delete it.
    let dir = tempfile::tempdir().unwrap();
    let gateway = FakeGateway {
        installed: RefCell::new(vec![
            SourceEntry {
                id: "src".into(),
                name: "Src".into(),
            },
            SourceEntry {
                id: "extra1".into(),
                name: "Extra One".into(),
            },
        ]),
        available: Ok(vec![
            SourceEntry {
                id: "extra1".into(),
                name: "Extra One".into(),
            },
            SourceEntry {
                id: "extra2".into(),
                name: "Extra Two".into(),
            },
        ]),
        // Nothing new matches during the widen.
        search_results: Ok(Vec::new()),
        ..FakeGateway::default()
    };
    let mut app = app(dir.path(), gateway, vec![]);
    // A results screen whose `tried` only knows about "src" (extra1 was
    // installed later), exactly as a reopened recent would look.
    app.stack.push(Screen::SearchResults {
        query: "n".into(),
        results: vec![(
            SourceEntry {
                id: "src".into(),
                name: "Src".into(),
            },
            MangaEntry {
                id: "m1".into(),
                title: "Naruto".into(),
                cover_url: None,
            },
        )],
        tried: vec!["src".into()],
        page: 0,
    });
    app.widen_search().unwrap();

    let uninstalled = app.gateway().uninstalled.borrow().clone();
    assert!(
        !uninstalled.contains(&"extra1".to_string()),
        "must not remove a source the user already had: {uninstalled:?}"
    );
    // Only the genuinely widen-added, no-hit source is removed.
    assert_eq!(uninstalled, vec!["extra2".to_string()]);
    assert!(
        app.gateway()
            .installed
            .borrow()
            .iter()
            .any(|s| s.id == "extra1"),
        "extra1 should still be installed"
    );
}

#[test]
fn widen_with_nothing_left_to_try_says_so() {
    // Every available source is already installed (and was searched), so
    // there's nothing for a widen to pull in.
    let dir = tempfile::tempdir().unwrap();
    let gateway = FakeGateway {
        installed: RefCell::new(vec![SourceEntry {
            id: "src".into(),
            name: "Src".into(),
        }]),
        available: Ok(vec![SourceEntry {
            id: "src".into(),
            name: "Src".into(),
        }]),
        search_results: Ok(vec![MangaEntry {
            id: "m1".into(),
            title: "Naruto".into(),
            cover_url: None,
        }]),
        ..FakeGateway::default()
    };
    let events = vec![
        tap_row(1),
        tap_key(Key::Char('n')),
        tap_key(Key::Search), // -> SearchResults (1 hit)
        tap_row(1),           // "Search more sources"
    ];
    let mut app = app(dir.path(), gateway, events);
    app.run().unwrap();

    let Screen::Message { title, body } = app.screen() else {
        panic!("expected a 'no more sources' message");
    };
    assert_eq!(title, "Search more");
    assert!(body.contains("No more sources"), "{body}");
}

// --- recent searches ---

#[test]
fn recent_search_is_remembered_and_reopened_from_cache() {
    // After a global search, opening search again lands on the recents
    // screen; tapping the recent reopens its cached results without
    // re-querying any source.
    let dir = tempfile::tempdir().unwrap();
    let events = vec![
        tap_row(1), // Home -> keyboard (no history yet)
        tap_key(Key::Char('n')),
        tap_key(Key::Search), // -> SearchResults, remembers "n"
        tap_back(),           // -> keyboard
        tap_back(),           // -> Home
        tap_row(1),           // -> RecentSearches (history exists now)
        tap_row(1),           // tap the recent "n" -> cached results
    ];
    let mut app = app(dir.path(), search_gateway(), events);
    app.run().unwrap();

    // Only the original search hit the gateway; the reopen came from cache.
    assert_eq!(*app.gateway().searches.borrow(), vec!["n".to_string()]);
    let Screen::SearchResults { query, results, .. } = app.screen() else {
        panic!("expected the cached results to reopen");
    };
    assert_eq!(query, "n");
    assert_eq!(results.len(), 1);
}

#[test]
fn recents_screen_new_search_row_opens_the_keyboard() {
    let dir = tempfile::tempdir().unwrap();
    let events = vec![
        tap_row(1),
        tap_key(Key::Char('n')),
        tap_key(Key::Search), // remembers "n"
        tap_back(),
        tap_back(),
        tap_row(1), // -> RecentSearches
        tap_row(0), // "New search…" -> keyboard
    ];
    let mut app = app(dir.path(), search_gateway(), events);
    app.run().unwrap();

    let Screen::Search { source, query } = app.screen() else {
        panic!("expected the search keyboard");
    };
    assert!(source.is_none());
    assert_eq!(query, "");
}

// --- physical page-turn buttons ---

#[test]
fn page_buttons_flip_library_pages() {
    let dir = tempfile::tempdir().unwrap();
    let lib = dir.path().join("Manga");
    let l = layout();
    let capacity = ShelfLayout::new(l.width, l.content_height(), SHELF_COLUMNS).capacity();
    // One series per card: the buttons page through cards.
    for i in 0..capacity + 2 {
        make_cbz(&lib.join(format!("Series {i:02}/vol1.cbz")), 1);
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

// --- battery ---

#[test]
fn home_title_includes_battery_percent_when_known() {
    assert_eq!(
        home_title("0.3.0", "default", Some(47)),
        "gideon v0.3.0 — default — 47%"
    );
    assert_eq!(
        home_title("0.3.0", "alex", None),
        "gideon v0.3.0 — alex",
        "no battery, no dangling separator"
    );
}

#[test]
fn battery_probe_feeds_home_and_sleep_without_breaking_either() {
    let dir = tempfile::tempdir().unwrap();
    let (count, sleeper) = counting_sleeper();
    let reads = std::rc::Rc::new(std::cell::Cell::new(0usize));
    let probe = reads.clone();
    let mut app = app(dir.path(), FakeGateway::default(), vec![UiEvent::Sleep])
        .with_sleeper(sleeper)
        .with_battery(Box::new(move || {
            probe.set(probe.get() + 1);
            Some(47)
        }));
    app.run().unwrap();

    assert_eq!(count.get(), 1, "sleep still suspends with a battery probe");
    assert!(
        reads.get() >= 2,
        "both the Home title and the sleep notice must read the battery"
    );
    assert!(matches!(app.screen(), Screen::Home));
    assert!(
        app.display().buffer.iter().any(|&p| p < 0x80),
        "home screen is blank"
    );
}

#[test]
fn update_prompt_back_declines() {
    let dir = tempfile::tempdir().unwrap();
    let gateway = FakeGateway {
        update_available: true,
        update_message: "Update available.".into(),
        ..FakeGateway::default()
    };
    let mut app = app(dir.path(), gateway, vec![tap_row(4), tap_back()]);
    app.run().unwrap();
    assert_eq!(app.gateway().installs.get(), 0, "back should not install");
}
