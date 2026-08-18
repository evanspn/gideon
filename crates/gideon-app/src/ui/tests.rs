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
    /// Canned MyAnimeList "Popular manga" titles for the Home tab.
    popular: std::result::Result<Vec<MangaEntry>, String>,
    search_results: std::result::Result<Vec<MangaEntry>, String>,
    /// Queries passed to `search_manga`, in order.
    searches: RefCell<Vec<String>>,
    /// Source ids passed to `search_manga`, in order.
    searched_sources: RefCell<Vec<String>>,
    /// Canned MyAnimeList title variants returned by `title_variants`.
    variants: Vec<String>,
    /// When set, `search_manga` only returns `search_results` for exactly
    /// this query (any other query gets no hits) — lets a test model a
    /// source that knows a manga under one name only.
    hit_query: Option<String>,
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
            popular: Ok(Vec::new()),
            search_results: Ok(Vec::new()),
            searches: RefCell::new(Vec::new()),
            searched_sources: RefCell::new(Vec::new()),
            variants: Vec::new(),
            hit_query: None,
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

    fn popular_manga(&self) -> Result<Vec<MangaEntry>> {
        self.popular.clone().map_err(|e| anyhow!(e))
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
        if let Some(hit) = &self.hit_query {
            if query != hit {
                return Ok(Vec::new());
            }
        }
        self.search_results.clone().map_err(|e| anyhow!(e))
    }

    fn title_variants(&self, _query: &str) -> Vec<String> {
        self.variants.clone()
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

/// The settings a profile actually sees: the device-global file overlaid with
/// that profile's own. Personal fields (reader fit, colour profile, library
/// view, cleanup delay…) live beside the profile's library, not in the device
/// file, so a test asserting on one has to read the merge — same as the app.
fn effective_settings(settings_dir: &Path, library_dir: &Path) -> gideon_core::Settings {
    gideon_core::Settings::load(settings_dir)
        .unwrap_or_default()
        .with_profile(&gideon_core::ProfileSettings::load(library_dir))
}

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

/// A tap on the four-zone nav bar the top-level destinations carry:
/// 0 Library, 1 Today, 2 Discover, 3 Settings. Mirrors the zone maths in
/// `tap_main_nav`.
fn tap_nav(zone: u32) -> UiEvent {
    let l = layout();
    UiEvent::Tap {
        x: l.width * (2 * zone + 1) / 8,
        y: l.nav_top() + l.nav_h / 2,
    }
}

/// The library is the landing screen now, so a test that wants the old Home
/// menu (sources, search, popular, updates) opens Discover first. Every row
/// index below that is unchanged, which is what keeps this a one-line
/// migration.
fn nav_discover() -> UiEvent {
    tap_nav(2)
}

/// Today: the destination that carries the device's status chrome — battery
/// in the title, and the power / bell / Bluetooth icons beside it.
fn tap_today() -> UiEvent {
    tap_nav(1)
}

/// Tap row `i` of a modal sheet with `rows` rows. Sheets are anchored to the
/// bottom and `sheet_height` is a title row plus one row each, so the
/// geometry follows from the row count alone.
fn tap_modal_row(rows: u32, i: usize) -> UiEvent {
    let l = layout();
    let top = H - (rows + 1) * l.row_h;
    UiEvent::Tap {
        x: W / 2,
        y: top + l.row_h + i as u32 * l.row_h + l.row_h / 2,
    }
}

/// Tap row `i` of the book sheet a long press on a library card opens:
/// 0 all chapters, 1 mark as unread, 2 delete chapter, 3 delete series,
/// 4 close.
fn tap_book_row(i: usize) -> UiEvent {
    tap_modal_row(5, i)
}

/// Tap row `i` of the profiles sheet. `names` is how many profiles it lists;
/// the rows after them are "New profile…", "Name the default profile…" when
/// a default still exists, and "Close".
fn tap_profile_row(i: usize, names: usize, has_default: bool) -> UiEvent {
    let rows = names + 2 + usize::from(has_default);
    tap_modal_row(rows as u32, i)
}

/// Tap row `i` of the delete confirmation sheet: 0 confirms, 1 cancels.
fn tap_confirm_row(i: usize) -> UiEvent {
    tap_modal_row(2, i)
}

/// The quick-settings sheet's geometry, mirroring `quick_sheet_layout`:
/// `(top, tile grid, first action row's y)`.
fn quick_sheet() -> (u32, gideon_render::widgets::GridLayout, u32) {
    let l = layout();
    let gap = super::SETTINGS_GAP;
    let title_h = (l.text_px * 1.6) as u32;
    let cell_h = l.row_h * 5 / 4;
    let rows = (QUICK_TILES as u32).div_ceil(super::SETTINGS_COLS);
    let grid_h = rows * cell_h + gap * rows.saturating_sub(1);
    let actions_h = l.row_h * QUICK_ACTIONS as u32;
    let h = (title_h + gap + grid_h + gap + actions_h).min(l.height);
    let top = l.height - h;
    let grid = gideon_render::widgets::GridLayout::new(
        l.pad,
        top + title_h + gap,
        l.width.saturating_sub(l.pad * 2),
        super::SETTINGS_COLS,
        cell_h,
        gap,
    );
    (top, grid, top + title_h + gap + grid_h + gap)
}

/// How many value tiles the quick sheet shows, and how many action rows sit
/// under them ("All settings", "Close").
const QUICK_TILES: usize = 5;
const QUICK_ACTIONS: usize = 2;

/// Tap tile `i` of the quick-settings grid.
fn tap_quick_tile(i: usize) -> UiEvent {
    let (_, grid, _) = quick_sheet();
    let (cx, cy, cw, ch) = grid.cell(i);
    UiEvent::Tap {
        x: cx + cw / 2,
        y: cy + ch / 2,
    }
}

/// Tap action row `i` of the quick-settings sheet: 0 "All settings", 1 "Close".
fn tap_quick_action(i: usize) -> UiEvent {
    let l = layout();
    let (_, _, actions_top) = quick_sheet();
    UiEvent::Tap {
        x: W / 2,
        y: actions_top + i as u32 * l.row_h + l.row_h / 2,
    }
}

/// Open the full Settings screen the way a user does: the Settings tab raises
/// the quick sheet, and "All settings" goes through to everything else.
fn open_settings() -> Vec<UiEvent> {
    vec![tap_nav(3), tap_quick_action(0)]
}

/// The page a setting's tile is drawn on, and a tap at the centre of it.
/// Computed from the same grid the screen uses, so a test never has to know
/// which page or column a setting landed in — and a tile that moves takes its
/// tap with it instead of quietly firing its neighbour.
fn settings_row_at(label: &str) -> (usize, UiEvent) {
    let l = layout();
    let head_h = l.row_h * 2 / 3;
    let cell_h = l.row_h * 5 / 4;
    let cols = super::SETTINGS_COLS;
    let gap = super::SETTINGS_GAP;
    let avail = l
        .nav_top()
        .saturating_sub(l.content_top() + l.pad / 2 + l.pad);
    let groups = super::settings_groups(&gideon_core::Settings::default());
    let paginate =
        |groups, avail| super::paginate_settings(groups, head_h, cell_h, gap, cols as usize, avail);
    let mut pages = paginate(groups.clone(), avail);
    if pages.len() > 1 {
        pages = paginate(groups, avail.saturating_sub(l.row_h));
    }
    for (p, page) in pages.iter().enumerate() {
        let mut y = l.content_top() + l.pad / 2;
        for (_, rows) in page {
            y += head_h;
            let grid = gideon_render::widgets::GridLayout::new(
                l.pad,
                y,
                l.width.saturating_sub(l.pad * 2),
                cols,
                cell_h,
                gap,
            );
            for (i, (name, ..)) in rows.iter().enumerate() {
                if name == label {
                    let (cx, cy, cw, ch) = grid.cell(i);
                    return (
                        p,
                        UiEvent::Tap {
                            x: cx + cw / 2,
                            y: cy + ch / 2,
                        },
                    );
                }
            }
            y += grid.height(rows.len()) + gap;
        }
    }
    panic!("no setting labelled {label:?}");
}

/// A tap on the settings pager strip: left half back, right half forward.
fn settings_pager(forward: bool) -> UiEvent {
    let l = layout();
    UiEvent::Tap {
        x: if forward {
            l.width * 3 / 4
        } else {
            l.width / 4
        },
        y: l.nav_top() - l.row_h / 2,
    }
}

/// Everything needed to tap the settings row labelled `label` from page 0 of
/// the Settings screen, ending back on page 0 so calls compose.
fn tap_setting(label: &str) -> Vec<UiEvent> {
    let (page, tap) = settings_row_at(label);
    let mut events: Vec<UiEvent> = (0..page).map(|_| settings_pager(true)).collect();
    events.push(tap);
    events.extend((0..page).map(|_| settings_pager(false)));
    events
}

/// Tap card `i` of Discover's grid. Discover is two columns of cards now, so
/// a tap at the middle of the screen lands in the gutter between them.
fn tap_card(i: usize) -> UiEvent {
    let l = layout();
    let gap = l.pad / 2;
    let top = l.content_top() + gap;
    let avail = l.nav_top().saturating_sub(top + gap);
    // Mirrors `discover_grid`: cards are laid out for the count on screen,
    // which is four, or five when the offline reconnect card is showing.
    let rows = 2u32;
    let cell_h = (avail.saturating_sub(gap * (rows - 1)) / rows).clamp(l.row_h, l.row_h * 5 / 2);
    let grid = gideon_render::widgets::GridLayout::new(
        l.pad,
        top,
        l.width.saturating_sub(l.pad * 2),
        super::SETTINGS_COLS,
        cell_h,
        gap,
    );
    let (cx, cy, cw, ch) = grid.cell(i);
    UiEvent::Tap {
        x: cx + cw / 2,
        y: cy + ch / 2,
    }
}

fn tap_row(i: usize) -> UiEvent {
    let l = layout();
    UiEvent::Tap {
        x: l.width / 2,
        y: l.row_top(i) + l.row_h / 2,
    }
}

/// A long press centered on menu row `i` (the row-targeted twin of
/// [`tap_row`]).
fn long_press_row(i: usize) -> UiEvent {
    let l = layout();
    UiEvent::LongPress {
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

/// Center x of a multi-page nav-bar button (the layout used by every
/// paginated list).
fn nav_button_center(target: TapTarget) -> u32 {
    let l = layout();
    l.nav_buttons(true)
        .into_iter()
        .find(|(t, ..)| *t == target)
        .map(|(_, x, w)| x + w / 2)
        .expect("nav button present in the paged layout")
}

fn nav_tap(target: TapTarget) -> UiEvent {
    UiEvent::Tap {
        x: nav_button_center(target),
        y: layout().nav_top() + 1,
    }
}

fn tap_nav_prev() -> UiEvent {
    nav_tap(TapTarget::Prev)
}

fn tap_nav_next() -> UiEvent {
    nav_tap(TapTarget::Next)
}

fn tap_nav_first() -> UiEvent {
    nav_tap(TapTarget::First)
}

fn tap_nav_last() -> UiEvent {
    nav_tap(TapTarget::Last)
}

/// Tap the sort button parked at the right edge of a chapter list's title bar.
fn tap_sort() -> UiEvent {
    let l = layout();
    UiEvent::Tap {
        x: sort_button_x(&l) + l.pad,
        y: 1,
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
/// [`tap_card`] at rotation `rot`: the card centre in reading orientation,
/// mapped back into panel coordinates.
fn tap_card_rot(i: usize, rot: u32) -> UiEvent {
    let l = menu_layout(rot);
    let gap = l.pad / 2;
    let top = l.content_top() + gap;
    let avail = l.nav_top().saturating_sub(top + gap);
    let cell_h = (avail.saturating_sub(gap) / 2).clamp(l.row_h, l.row_h * 5 / 2);
    let grid = gideon_render::widgets::GridLayout::new(
        l.pad,
        top,
        l.width.saturating_sub(l.pad * 2),
        super::SETTINGS_COLS,
        cell_h,
        gap,
    );
    let (cx, cy, cw, ch) = grid.cell(i);
    let (x, y) = panel_point_for(cx + cw / 2, cy + ch / 2, rot);
    UiEvent::Tap { x, y }
}

/// [`tap_nav`] at rotation `rot`: the nav zone in reading orientation,
/// mapped back into panel coordinates.
fn tap_nav_rot(zone: u32, rot: u32) -> UiEvent {
    let l = menu_layout(rot);
    let (x, y) = panel_point_for(l.width * (2 * zone + 1) / 8, l.nav_top() + l.nav_h / 2, rot);
    UiEvent::Tap { x, y }
}

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
fn the_landing_screen_is_today_and_is_not_blank() {
    // Today is what the device opens on: the chapter you were in the middle
    // of and what is waiting unread. The Library is one nav tap away, and it
    // is the whole library rather than the couple of rows a launcher shows.
    let dir = tempfile::tempdir().unwrap();
    let mut app = app(dir.path(), FakeGateway::default(), vec![]);
    app.run().unwrap();
    assert!(matches!(app.screen(), Screen::Stats));
    // Exactly one paint, and a full one: e-ink flashes on every full
    // refresh, so a launch that painted the landing twice would flash twice
    // before the reader saw anything.
    assert_eq!(app.display().flushes, vec![RefreshMode::Full]);
    assert!(
        app.display().buffer.iter().any(|&p| p < 0x80),
        "landing screen is blank"
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
    a.goto_root(Screen::Home).unwrap();
    a.home_offline = true;
    a.activate(0, 10, 10).unwrap();
    assert!(
        matches!(a.screen(), Screen::Home),
        "offline row 0 reconnects, stays on Home"
    );

    // Offline: the cards are offset past the reconnect card, so "Browse
    // sources" — the second destination — is card 2.
    let mut b = app(&lib, FakeGateway::default(), vec![]);
    b.goto_root(Screen::Home).unwrap();
    b.home_offline = true;
    let UiEvent::Tap { x, y } = tap_card(2) else {
        unreachable!()
    };
    b.handle_tap(x, y).unwrap();
    assert!(
        matches!(b.screen(), Screen::Sources { .. }),
        "offline card 2 is Browse sources"
    );

    // Online (the default): no reconnect card, so Browse sources is card 1.
    let mut c = app(&lib, FakeGateway::default(), vec![]);
    c.goto_root(Screen::Home).unwrap();
    // Painting Discover probes connectivity; pin the online case so the
    // offset under test is the one being asserted and not the probe's mood.
    c.home_offline = false;
    let UiEvent::Tap { x, y } = tap_card(1) else {
        unreachable!()
    };
    c.handle_tap(x, y).unwrap();
    assert!(
        matches!(c.screen(), Screen::Sources { .. }),
        "online card 1 is Browse sources"
    );
}

#[test]
fn home_to_library_to_reader_page_turns_and_back() {
    let dir = tempfile::tempdir().unwrap();
    let lib = dir.path().join("Manga");
    make_cbz(&lib.join("Sample/vol1.cbz"), 5);

    let events = vec![
        tap_nav(0),        // Home -> Library
        tap_shelf_cell0(), // open the reader
        reader_tap_next(), // page 2
        reader_tap_next(), // page 3
        reader_tap_prev(), // page 2
        reader_tap_back(), // back to Library
    ];
    let mut app = app(&lib, FakeGateway::default(), events);
    app.run().unwrap();

    assert!(matches!(app.screen(), Screen::Library { .. }));
    // Progress was saved under the library-relative key.
    let store = ProgressStore::load(&progress_path(&lib)).unwrap();
    let progress = store.get("Sample/vol1.cbz").expect("progress saved");
    assert_eq!(progress.current_page, 1); // 0 -> 1 -> 2 -> back to 1
    assert_eq!(progress.total_pages, 5);

    // Screen changes are full refreshes; reader page turns are partial.
    let flushes = &app.display().flushes;
    assert_eq!(flushes[0], RefreshMode::Full); // the landing library
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

    let events = vec![tap_nav(0), tap_shelf_cell0(), reader_tap_next()];
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
        tap_nav(0),
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
    let mut events = vec![tap_nav(0), tap_shelf_cell0()];
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
        tap_nav(0),
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
        tap_nav_rot(0, 90),
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

        // Row 1 of the Discover menu opens Sources… (the row indices are
        // Discover's, so each app starts there rather than on the library
        // the device lands on.)
        let mut sources_app = app(&lib, FakeGateway::default(), vec![tap_card_rot(1, rot)])
            .with_reader_settings(FitMode::Contain, rot);
        sources_app.goto_root(Screen::Home).unwrap();
        sources_app.run().unwrap();
        assert!(
            matches!(sources_app.screen(), Screen::Sources { .. }),
            "rotation {rot}: the Browse sources row tap must open Sources"
        );

        // …and row 0 opens search, which with no sources installed says so
        // rather than opening a dead keyboard: per-row precision, not just
        // "hit something".
        let mut search_app = app(&lib, FakeGateway::default(), vec![tap_card_rot(0, rot)])
            .with_reader_settings(FitMode::Contain, rot);
        search_app.goto_root(Screen::Home).unwrap();
        search_app.run().unwrap();
        assert!(
            matches!(search_app.screen(), Screen::Message { title, .. } if title == "Search"),
            "rotation {rot}: the search row tap must open search"
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

#[test]
fn leaving_the_reader_drains_the_exit_gestures_tail() {
    // Swiping down to exit the reader can trail a stray touch that the panel
    // reports as a separate tap; draining on exit stops it landing on the
    // library underneath and opening a book at random.
    let dir = tempfile::tempdir().unwrap();
    let lib = dir.path().join("Manga");
    make_cbz(&lib.join("Sample/vol1.cbz"), 5);

    let swipe_down_exit = UiEvent::Swipe {
        x0: W / 2,
        y0: H / 4,
        x1: W / 2,
        y1: H - 20, // well past the quarter-height exit threshold
    };
    let events = vec![
        tap_nav(0),        // Home -> Library
        tap_shelf_cell0(), // open the book
        swipe_down_exit,   // swipe down -> back to the library
    ];
    let mut app = app(&lib, FakeGateway::default(), events);
    app.run().unwrap();

    assert!(
        matches!(app.screen(), Screen::Library { .. }),
        "swipe-down returns to the library"
    );
    assert!(
        app.input().discard_queued_calls >= 1,
        "leaving the reader drains the exit gesture's tail so it can't tap a book"
    );
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
        tap_nav(0),
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
        tap_nav(0),
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
        tap_nav(0),
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
        tap_nav(0),
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
        tap_nav(0),
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
        tap_nav(0),
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
    // down (180°) the two keys have physically swapped places, so the BACK
    // button advances the page. (Observed via the advance, since progress is
    // furthest-page-wins and never records a lower page.)
    let dir = tempfile::tempdir().unwrap();
    let lib = dir.path().join("Manga");
    make_cbz(&lib.join("Sample/vol1.cbz"), 5);
    let mut store = ProgressStore::default();
    store.update("Sample/vol1.cbz", 2, 5);
    store.save(&progress_path(&lib)).unwrap();

    let events = vec![
        tap_nav_rot(0, 180),
        tap_row_rot(0, 180),
        tap_shelf_cell0_rot(180),
        UiEvent::PageBack, // 180°: the back button goes forward -> page 3
        UiEvent::Tap { x: W / 2, y: H / 2 }, // center is Back at any rotation
    ];
    let mut app =
        app(&lib, FakeGateway::default(), events).with_reader_settings(FitMode::Contain, 180);
    app.run().unwrap();

    let store = ProgressStore::load(&progress_path(&lib)).unwrap();
    assert_eq!(
        store.get("Sample/vol1.cbz").unwrap().current_page,
        3,
        "upside down, the back button advances the page"
    );
}

#[test]
fn bluetooth_remote_direction_is_absolute_upside_down() {
    // Regression: the remote's next/previous must NOT flip with rotation. A
    // remote is a separate object in your hand — it doesn't rotate with the
    // device — so at 180° (where the bezel buttons swap, see the test above)
    // RemoteNext still advances the page. Furthest-page-wins means a wrongly
    // reversed turn would leave progress at 2, so reaching 3 proves it advanced.
    let dir = tempfile::tempdir().unwrap();
    let lib = dir.path().join("Manga");
    make_cbz(&lib.join("Sample/vol1.cbz"), 5);
    let mut store = ProgressStore::default();
    store.update("Sample/vol1.cbz", 2, 5);
    store.save(&progress_path(&lib)).unwrap();

    let events = vec![
        tap_nav_rot(0, 180),
        tap_row_rot(0, 180),
        tap_shelf_cell0_rot(180),
        UiEvent::RemoteNext, // 180°: still advances -> page 3 (no swap)
        UiEvent::Tap { x: W / 2, y: H / 2 }, // center is Back at any rotation
    ];
    let mut app =
        app(&lib, FakeGateway::default(), events).with_reader_settings(FitMode::Contain, 180);
    app.run().unwrap();

    let store = ProgressStore::load(&progress_path(&lib)).unwrap();
    assert_eq!(
        store.get("Sample/vol1.cbz").unwrap().current_page,
        3,
        "the remote's Next advances even upside down — direction is absolute"
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
    let mut app = app(&lib, FakeGateway::default(), vec![tap_nav(0)]);
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

    let events = vec![tap_nav(0), tap_nav_next(), tap_nav_next(), tap_nav_prev()];
    let mut app = app(&lib, FakeGateway::default(), events);
    app.run().unwrap();

    // Two pages: next, next (clamped), prev -> page 0.
    let Screen::Library { page, items } = app.screen() else {
        panic!("expected library screen");
    };
    eprintln!(
        "DBG capacity={capacity} items={} page={page} flushes={:?}",
        items.len(),
        app.display().flushes
    );
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

    let mut app = app(&lib, FakeGateway::default(), vec![tap_nav(0)]);
    app.run().unwrap();

    let Screen::Library { items, .. } = app.screen() else {
        panic!("expected library screen");
    };
    assert_eq!(items.len(), 1, "chapters must not flood the shelf");
    assert_eq!(items[0].title(), "Series");
    assert_eq!(items[0].chapters.len(), 3);
}

#[test]
fn library_orders_most_recently_read_first() {
    // Top-left is the most-recently-read series, so picking up where you left
    // off is always the first tap. Alphabetically A < B < C, but reading
    // order should override that entirely; a series never read yet falls to
    // the back, behind everything that has been.
    let dir = tempfile::tempdir().unwrap();
    let lib = dir.path().join("Manga");
    make_cbz(&lib.join("A/vol1.cbz"), 2);
    make_cbz(&lib.join("B/vol1.cbz"), 2); // never read
    make_cbz(&lib.join("C/vol1.cbz"), 2);

    std::fs::create_dir_all(progress_path(&lib).parent().unwrap()).unwrap();
    std::fs::write(
        progress_path(&lib),
        r#"{"progress":{
            "A/vol1.cbz": {"current_page": 0, "total_pages": 2, "last_read_at": 100},
            "C/vol1.cbz": {"current_page": 0, "total_pages": 2, "last_read_at": 300}
        }}"#,
    )
    .unwrap();

    let mut app = app(&lib, FakeGateway::default(), vec![]);
    app.open_library().unwrap();

    let Screen::Library { items, .. } = app.screen() else {
        panic!("expected library screen");
    };
    let titles: Vec<String> = items.iter().map(|c| c.title()).collect();
    assert_eq!(
        titles,
        vec!["C", "A", "B"],
        "most recently read (C) leads, unread (B) trails"
    );
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
        tap_nav(0),
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
    let events = vec![tap_nav(0), UiEvent::LongPress { x, y }];
    let mut app = app(&lib, FakeGateway::default(), events);
    app.run().unwrap();

    let Some(Sheet::Book { entry, .. }) = app.sheet() else {
        panic!("expected the book sheet");
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
        tap_nav(0),
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

    let mut app = app(&lib, FakeGateway::default(), vec![tap_nav(0)]);
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
    let events = vec![tap_nav(0), UiEvent::LongPress { x, y }];
    let mut app = app(&lib, FakeGateway::default(), events);
    app.run().unwrap();

    let Some(Sheet::Book {
        entry,
        series_dir,
        read_key,
    }) = app.sheet()
    else {
        panic!("expected the book sheet");
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
    let events = vec![tap_nav(0), UiEvent::LongPress { x, y }];
    let mut app = app(&lib, FakeGateway::default(), events);
    app.run().unwrap();

    let Some(Sheet::Book { entry, .. }) = app.sheet() else {
        panic!("expected the book sheet");
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
    // This test is about the cover SHELF specifically; the dense list is the
    // default view now, so pin this profile to the shelf.
    gideon_core::ProfileSettings {
        library_view: Some("shelf".into()),
        ..Default::default()
    }
    .save(&lib)
    .unwrap();

    let mut app = app(&lib, FakeGateway::default(), vec![tap_nav(0)]);
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

    // Pin this profile to the cover shelf: the flip under test is the
    // colour one, and the dense list is the default view now.
    gideon_core::ProfileSettings {
        library_view: Some("shelf".into()),
        ..Default::default()
    }
    .save(&lib)
    .unwrap();

    let events = vec![tap_nav(0), tap_nav_next()];
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

    let mut app = app(&lib, FakeGateway::default(), vec![tap_nav(0)]);
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
    let mut app = app(dir.path(), gateway, vec![nav_discover(), tap_card(1)]);
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
    let mut app = app(dir.path(), gateway, vec![nav_discover(), tap_card(1)]);
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
    let mut app = app(
        dir.path(),
        gateway,
        vec![nav_discover(), tap_card(1), tap_row(1)],
    );
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
        nav_discover(),
        tap_card(1),       // Home -> Sources
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
        nav_discover(),
        tap_card(1),    // Sources
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
        nav_discover(),
        tap_card(1), // Sources
        tap_row(0),  // Listings
        tap_row(0),  // Popular -> fails
        tap_row(0),  // tap the error screen -> back
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
    let events = vec![nav_discover(), tap_card(1), tap_row(0), tap_row(0)];
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
    let mut app = app(dir.path(), gateway, vec![nav_discover(), tap_card(3)]);
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
        vec![nav_discover(), tap_back(), tap_back(), tap_card(0)],
    );
    app.run().unwrap();
    assert!(
        matches!(app.screen(), Screen::Message { title, .. } if title == "Search"),
        "the row tap after two ignored Back taps still works"
    );
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
    // Today carries the status chrome (battery, power, bell), so that is
    // where the power symbol is tapped from.
    let events = vec![tap_today(), tap_power_icon(), tap_back()];
    let mut app = app(dir.path(), FakeGateway::default(), events);
    assert_eq!(app.run().unwrap(), Exit::Close); // input exhausted
    assert!(matches!(app.screen(), Screen::Stats));
}

#[test]
fn power_menu_close_quits() {
    let dir = tempfile::tempdir().unwrap();
    // Row 0 is the Wi-Fi toggle now; Restart is 1, Close is 2.
    let events = vec![nav_discover(), tap_power_icon(), tap_row(2)];
    let mut app = app(dir.path(), FakeGateway::default(), events);
    assert_eq!(app.run().unwrap(), Exit::Close);
}

#[test]
fn power_menu_restart_requests_restart() {
    let dir = tempfile::tempdir().unwrap();
    let events = vec![nav_discover(), tap_power_icon(), tap_row(1)];
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
            persistent: false,
        });
    }
    app.stack.push(Screen::ChapterList {
        source: source.clone(),
        manga: manga.clone(),
        chapters: vec![],
        page: 0,
        sort: ChapterSort::default(),
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
fn switching_profile_repoints_the_predownloader_at_the_new_library() {
    // The bug: the pre-downloader's worker thread closes over the library
    // directory it was built with and keeps writing there for its whole life.
    // Since it's only ever built once (lazily, on first use) and switching
    // profiles didn't drop it, a download queued after switching profiles kept
    // landing in the *previous* profile's directory — mixing the two profiles'
    // libraries. Now a profile switch drops it so the next queue rebuilds it
    // against the new profile's directory.
    let dir = tempfile::tempdir().unwrap();
    let lib = dir.path().join("Manga");
    std::fs::create_dir_all(&lib).unwrap();

    let gateway = BgGateway::new("Manga One", 2);
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

    // Spin up the worker on the default profile...
    assert!(app.ensure_predownloader());
    // ...then switch to a different profile before queuing anything on it.
    app.switch_profile("bob").unwrap();

    let epoch = app.predownloader.as_ref().map_or(0, |w| w.epoch());
    // ensure_predownloader must (re)build here, scoped to bob's library.
    assert!(app.ensure_predownloader());
    app.predownloader.as_mut().unwrap().queue(PreloadJob {
        source: source.clone(),
        manga: manga.clone(),
        chapter_id: "c1".into(),
        epoch,
        persistent: true,
    });

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while std::time::Instant::now() < deadline
        && app.downloaded_chapter_path(&source, &manga, "c1").is_none()
    {
        std::thread::sleep(std::time::Duration::from_millis(25));
    }

    assert!(
        app.downloaded_chapter_path(&source, &manga, "c1").is_some(),
        "the chapter queued after switching profiles is downloaded"
    );
    assert!(
        lib.join("@bob/Manga One/c1.cbz").exists(),
        "it lands under bob's profile directory"
    );
    assert!(
        !lib.join("Manga One/c1.cbz").exists(),
        "it must not leak into the default profile's directory"
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

#[test]
fn download_ahead_counts_are_round_sizes_then_all_remaining() {
    // Plenty left: the round sizes that fit, then an "all remaining".
    assert_eq!(download_ahead_counts(30), vec![5, 10, 25, 30]);
    // Fewer left than the bigger round sizes: only the ones that fit, then all.
    assert_eq!(download_ahead_counts(8), vec![5, 8]);
    // Just a couple left: only "all remaining".
    assert_eq!(download_ahead_counts(2), vec![2]);
}

fn dl_context(total: usize, index: usize) -> DownloadContext {
    DownloadContext {
        source: SourceEntry {
            id: "s".into(),
            name: "S".into(),
        },
        manga: MangaEntry {
            id: "m".into(),
            title: "M".into(),
            cover_url: None,
        },
        chapters: (1..=total)
            .map(|i| ChapterEntry {
                id: format!("c{i}"),
                num: Some(i as f32),
                title: None,
                lang: None,
            })
            .collect(),
        index,
    }
}

#[test]
fn chapter_menu_rows_adapt_to_source_link_and_download_state() {
    let ctx = Some(dl_context(2, 0));
    let labels = |rows: Vec<(String, bool, ChapterMenuAction)>| {
        rows.into_iter().map(|(l, _, _)| l).collect::<Vec<_>>()
    };

    // Source link, not on disk: download actions only.
    assert_eq!(
        labels(chapter_menu_rows(&ctx, &None, false)),
        vec!["Download this chapter", "Download from here…"]
    );
    // Source link, on disk: "download this" drops, read-status + delete appear.
    assert_eq!(
        labels(chapter_menu_rows(&ctx, &Some("M/c1.cbz".into()), false)),
        vec![
            "Download from here…",
            "Mark as read",
            "Mark as unread",
            "Delete download",
        ]
    );
    // No source link (offline list), on disk: just read-status + delete.
    assert_eq!(
        labels(chapter_menu_rows(&None, &Some("M/c1.cbz".into()), false)),
        vec!["Mark as read", "Mark as unread", "Delete download"]
    );
}

#[test]
fn chapter_kebab_on_a_source_list_opens_the_download_menu() {
    let dir = tempfile::tempdir().unwrap();
    let lib = dir.path().join("Manga");
    std::fs::create_dir_all(&lib).unwrap();
    let mut app = app(&lib, FakeGateway::default(), vec![]);
    app.stack.push(Screen::ChapterList {
        source: SourceEntry {
            id: "s".into(),
            name: "S".into(),
        },
        manga: MangaEntry {
            id: "m".into(),
            title: "M".into(),
            cover_url: None,
        },
        chapters: (1..=3)
            .map(|i| ChapterEntry {
                id: format!("c{i}"),
                num: Some(i as f32),
                title: None,
                lang: None,
            })
            .collect(),
        page: 0,
        sort: ChapterSort::default(),
    });

    // ⋮ on row 0 opens the chapter action menu carrying the source context.
    let (kx, ky) = tap_row_kebab(0);
    app.activate(0, kx, ky).unwrap();
    let Screen::ChapterMenu { download, key, .. } = app.screen() else {
        panic!("the ⋮ button opens the chapter action menu");
    };
    assert!(
        download.is_some(),
        "the menu carries the source download context"
    );
    assert!(key.is_none(), "this chapter isn't downloaded yet");

    // Row 1 ("Download from here…") opens the count picker.
    let UiEvent::Tap { x, y } = tap_row(1) else {
        unreachable!()
    };
    app.activate(1, x, y).unwrap();
    assert!(
        matches!(app.screen(), Screen::DownloadAheadMenu { .. }),
        "\"Download from here…\" opens the count picker"
    );
}

#[test]
fn download_from_here_queues_a_persistent_batch_that_survives_leaving() {
    // A deliberate "download these" must keep going after you leave the manga —
    // unlike the automatic look-ahead, which is cancelled.
    let dir = tempfile::tempdir().unwrap();
    let lib = dir.path().join("Manga");
    std::fs::create_dir_all(&lib).unwrap();

    let (started_tx, started_rx) = std::sync::mpsc::channel::<String>();
    let gateway = BgGateway {
        manga_dir: "Manga One".into(),
        pages: 2,
        delay_ms: 40, // keep the batch in flight while we leave the manga
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
    let chapters: Vec<ChapterEntry> = (1..=12)
        .map(|i| ChapterEntry {
            id: format!("c{i}"),
            num: Some(i as f32),
            title: None,
            lang: Some("en".into()),
        })
        .collect();
    app.stack.push(Screen::ChapterList {
        source: source.clone(),
        manga: manga.clone(),
        chapters: chapters.clone(),
        page: 0,
        sort: ChapterSort::default(),
    });

    // ⋮ on c1 → "Download from here…" (row 1) → "Download 5 chapters" (row 0).
    let (kx, ky) = tap_row_kebab(0);
    app.activate(0, kx, ky).unwrap();
    let UiEvent::Tap { x, y } = tap_row(1) else {
        unreachable!()
    };
    app.activate(1, x, y).unwrap();
    assert!(matches!(app.screen(), Screen::DownloadAheadMenu { .. }));
    let UiEvent::Tap { x, y } = tap_row(0) else {
        unreachable!()
    };
    app.activate(0, x, y).unwrap();
    assert!(
        matches!(app.screen(), Screen::Message { .. }),
        "a confirmation is shown after queueing"
    );

    // The worker has begun the batch; now leave the manga entirely.
    started_rx
        .recv_timeout(std::time::Duration::from_secs(2))
        .unwrap();
    app.pop().unwrap(); // leave the confirmation
    app.pop().unwrap(); // leave the chapter list → would cancel a look-ahead

    // All five queued chapters still land; the sixth was never requested.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        if (1..=5).all(|i| {
            app.downloaded_chapter_path(&source, &manga, &format!("c{i}"))
                .is_some()
        }) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    for i in 1..=5 {
        assert!(
            app.downloaded_chapter_path(&source, &manga, &format!("c{i}"))
                .is_some(),
            "c{i} downloads despite leaving the manga (persistent batch)"
        );
    }
    assert!(
        app.downloaded_chapter_path(&source, &manga, "c6").is_none(),
        "only the requested five chapters were queued"
    );
}

/// A gateway that fails each chapter's FIRST download attempt and succeeds on
/// any later one — a stand-in for a flaky device (transient source/network
/// error). Shared attempt-count state so the retry is observable across the
/// background clone the worker runs on.
#[derive(Clone)]
struct FailFirstGateway {
    manga_dir: String,
    attempts: std::sync::Arc<std::sync::Mutex<std::collections::HashMap<String, usize>>>,
    started: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

impl FailFirstGateway {
    fn new(manga_dir: &str) -> Self {
        Self {
            manga_dir: manga_dir.into(),
            attempts: std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
            started: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }
    }
}

impl SourceGateway for FailFirstGateway {
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
        self.started
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let n = {
            let mut a = self.attempts.lock().unwrap();
            let e = a.entry(chapter_id.to_string()).or_insert(0);
            *e += 1;
            *e
        };
        if n == 1 {
            anyhow::bail!("transient failure on the first attempt");
        }
        let path = library
            .join(&self.manga_dir)
            .join(format!("{chapter_id}.cbz"));
        make_cbz(&path, 2);
        progress(2, 2);
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
fn explicit_batch_retries_after_a_failed_attempt() {
    // The bug: "Download all remaining" says it's downloading but never does.
    // A batch that fails to land (a flaky first attempt on the device) leaves
    // its chapters stuck in the worker's dedup set, so re-requesting them the
    // same session silently enqueues nothing — they can never download until
    // you leave and come back. An explicit request must always re-attempt.
    let dir = tempfile::tempdir().unwrap();
    let lib = dir.path().join("Manga");
    std::fs::create_dir_all(&lib).unwrap();

    let gateway = FailFirstGateway::new("Manga One");
    let started = std::sync::Arc::clone(&gateway.started);
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
    let chapters: Vec<ChapterEntry> = (1..=3)
        .map(|i| ChapterEntry {
            id: format!("c{i}"),
            num: Some(i as f32),
            title: None,
            lang: Some("en".into()),
        })
        .collect();

    // First explicit batch: three chapters queued; every first attempt fails.
    let queued = app
        .queue_batch_download(&source, &manga, &chapters, 0, 3)
        .unwrap();
    assert_eq!(queued, 3, "all three were requested");

    // Wait until the worker has attempted (and failed) all three.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while started.load(std::sync::atomic::Ordering::SeqCst) < 3
        && std::time::Instant::now() < deadline
    {
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(
        (1..=3).all(|i| app
            .downloaded_chapter_path(&source, &manga, &format!("c{i}"))
            .is_none()),
        "first attempts failed, so nothing is on disk yet"
    );

    // Same session, no leaving: ask again. The chapters must re-attempt and,
    // this time, succeed — not be swallowed by the dedup set.
    let requeued = app
        .queue_batch_download(&source, &manga, &chapters, 0, 3)
        .unwrap();
    assert_eq!(
        requeued, 3,
        "the still-missing chapters are requested again"
    );

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        if (1..=3).all(|i| {
            app.downloaded_chapter_path(&source, &manga, &format!("c{i}"))
                .is_some()
        }) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    for i in 1..=3 {
        assert!(
            app.downloaded_chapter_path(&source, &manga, &format!("c{i}"))
                .is_some(),
            "c{i} re-downloaded on the retry instead of being silently skipped"
        );
    }
}

/// A source-linked series with two downloaded chapters, recorded in the index
/// so that — when online — "All chapters" would fetch the source list.
fn offline_fixture(lib: &Path) {
    make_cbz(&lib.join("Series/vol1.cbz"), 2);
    make_cbz(&lib.join("Series/vol2.cbz"), 2);
    let mut index = gideon_core::SeriesIndex::load(lib);
    index.record(
        "Series",
        gideon_core::SeriesRef {
            source_id: "src".into(),
            source_name: "Src".into(),
            manga_id: "m1".into(),
            manga_title: "Series".into(),
            ..Default::default()
        },
    );
    index.record_download("Series", "c1", "vol1.cbz");
    index.record_download("Series", "c2", "vol2.cbz");
    index.save(lib).unwrap();
}

/// A source-linked gateway whose chapter list, if ever consulted, is clearly
/// distinguishable from the on-disk files — and whose download is unconfigured,
/// so any download attempt errors. Lets a test prove the source was never
/// touched.
fn source_gateway() -> FakeGateway {
    FakeGateway {
        installed: RefCell::new(vec![SourceEntry {
            id: "src".into(),
            name: "Src".into(),
        }]),
        chapters: vec![ChapterEntry {
            id: "c99".into(),
            num: Some(99.0),
            title: Some("from the network".into()),
            lang: None,
        }],
        ..FakeGateway::default()
    }
}

/// End-to-end: with Wi-Fi down, opening a *source-linked* series swaps straight
/// to its downloaded chapters — no connect attempt, no source fetch, no
/// download — and reading one records progress. Driven through the real event
/// loop with an injected offline probe.
#[test]
fn offline_series_swaps_to_downloads_and_reads_without_network() {
    let dir = tempfile::tempdir().unwrap();
    let lib = dir.path().join("Manga");
    offline_fixture(&lib);

    let cell = tap_shelf_cell0();
    let UiEvent::Tap { x, y } = cell else {
        unreachable!()
    };
    // Offline, Home shows the "reconnect" row at row 0, so Library is row 1.
    let events = vec![
        tap_nav(0),                  // Library
        UiEvent::LongPress { x, y }, // card -> BookMenu
        tap_book_row(0),             // "All chapters" -> downloads (offline)
        tap_row(0),                  // first downloaded chapter -> reader
        reader_tap_next(),           // turn a page
        reader_tap_back(),           // back to the list
    ];
    let mut app = app(&lib, source_gateway(), events).with_online_probe(Box::new(|| false));
    app.run().unwrap();

    // We're on the LOCAL downloaded list (not the source ChapterList), and the
    // chapters came from disk, not the gateway's "from the network" sentinel.
    let Screen::DownloadedChapters { entries, .. } = app.screen() else {
        panic!("offline, the series opens straight to its downloaded chapters");
    };
    let rel: Vec<&str> = entries.iter().map(|e| e.relative_path.as_str()).collect();
    assert_eq!(rel, vec!["Series/vol1.cbz", "Series/vol2.cbz"]);

    // Reading a downloaded chapter offline recorded progress — no download path
    // was hit (the gateway has no download configured, so it would have errored).
    let store = ProgressStore::load(&progress_path(&lib)).unwrap();
    assert!(
        store.get("Series/vol1.cbz").is_some(),
        "reading a downloaded chapter offline recorded progress"
    );
}

/// The mirror of the above: when online, the same source-linked series opens
/// the source chapter list (so you can pull chapters you don't have yet).
/// Count non-white pixels inside content row `i` of a composed gray frame —
/// a proxy for "did any chapter text/icon actually draw on this row".
fn row_ink(page: &gideon_render::GrayPage, i: usize) -> usize {
    let l = layout();
    let top = l.row_top(i);
    let bottom = (top + l.row_h).min(page.height);
    let mut ink = 0;
    for y in top..bottom {
        for x in 0..page.width {
            if page.pixels[(y * page.width + x) as usize] != 0xFF {
                ink += 1;
            }
        }
    }
    ink
}

/// Guards the display half of "chapters not showing": the shared
/// `compose_chapter_list` is state-tested but never pixel-tested, so a blank
/// draw would slip through. This renders the real frame for both the offline
/// (downloaded) and online (source) lists and asserts the rows have ink.
#[test]
fn chapter_list_rows_render_not_blank() {
    let dir = tempfile::tempdir().unwrap();
    let lib = dir.path().join("Manga");
    // The user's real library shapes: spaces, parens, decimal chapters.
    make_cbz(&lib.join("Ibitsu/Chapter 6.5.cbz"), 3);
    make_cbz(&lib.join("Ibitsu/Chapter 7.cbz"), 3);

    // --- offline path: downloaded chapters ---
    let mut offline = app(&lib, FakeGateway::default(), vec![]);
    offline.open_series_chapters("Ibitsu").unwrap();
    let Screen::DownloadedChapters { entries, .. } = offline.screen() else {
        panic!("expected the downloaded-chapters list");
    };
    assert_eq!(entries.len(), 2, "both chapters present in state");
    let frame = offline.compose_current().unwrap();
    assert!(
        row_ink(&frame, 0) > 20 && row_ink(&frame, 1) > 20,
        "downloaded chapter rows rendered blank (row0={}, row1={})",
        row_ink(&frame, 0),
        row_ink(&frame, 1),
    );

    // --- online path: source chapter list ---
    let manga = MangaEntry {
        id: "m1".into(),
        title: "Ibitsu".into(),
        cover_url: None,
    };
    let source = SourceEntry {
        id: "src".into(),
        name: "Src".into(),
    };
    let mut online = app(&lib, source_gateway(), vec![]).with_online_probe(Box::new(|| true));
    online.open_chapter_list(&source, &manga).unwrap();
    let Screen::ChapterList { chapters, .. } = online.screen() else {
        panic!("expected the source chapter list");
    };
    assert_eq!(chapters.len(), 1, "source chapter present in state");
    let frame = online.compose_current().unwrap();
    assert!(
        row_ink(&frame, 0) > 20,
        "source chapter row rendered blank (row0={})",
        row_ink(&frame, 0),
    );
}

/// REPRO: online, a source-linked series whose source returns an EMPTY chapter
/// list (rate-limited, outdated source, delisted manga, or a next-SDK source
/// that yields no chapters) must not strand the reader on a blank screen — it
/// must fall back to the chapters already downloaded on the device.
#[test]
fn empty_source_chapter_list_falls_back_to_downloads() {
    let dir = tempfile::tempdir().unwrap();
    let lib = dir.path().join("Manga");
    offline_fixture(&lib); // Series/vol1.cbz, vol2.cbz + a "src" origin link

    // Source is reachable but returns no chapters (the systemic failure mode).
    let gateway = FakeGateway {
        chapters: vec![],
        ..source_gateway()
    };
    let mut app = app(&lib, gateway, vec![]).with_online_probe(Box::new(|| true));
    app.open_series_chapters("Series").unwrap();

    match app.screen() {
        Screen::DownloadedChapters { entries, .. } => {
            assert_eq!(entries.len(), 2, "fell back to the downloaded chapters");
        }
        Screen::ChapterList { chapters, .. } => panic!(
            "stranded on an empty source list ({} chapters) instead of showing downloads",
            chapters.len()
        ),
        other => panic!("unexpected screen: {other:?}"),
    }
}

#[test]
fn online_series_opens_the_source_chapter_list() {
    let dir = tempfile::tempdir().unwrap();
    let lib = dir.path().join("Manga");
    offline_fixture(&lib);

    let cell = tap_shelf_cell0();
    let UiEvent::Tap { x, y } = cell else {
        unreachable!()
    };
    // Online, Home has no reconnect row, so Library is row 0.
    let events = vec![
        tap_nav(0),                  // Home -> Library
        UiEvent::LongPress { x, y }, // card -> BookMenu
        tap_book_row(0),             // "All chapters" -> source list (online)
    ];
    let mut app = app(&lib, source_gateway(), events).with_online_probe(Box::new(|| true));
    app.run().unwrap();

    let Screen::ChapterList { chapters, .. } = app.screen() else {
        panic!("online, the series opens the source chapter list");
    };
    assert_eq!(
        chapters.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(),
        vec!["c99"],
        "the chapters came from the source, not local files"
    );
}

#[test]
fn chapter_menu_deletes_a_downloaded_chapter() {
    let dir = tempfile::tempdir().unwrap();
    let lib = dir.path().join("Manga");
    make_cbz(&lib.join("Series/vol1.cbz"), 2);
    make_cbz(&lib.join("Series/vol2.cbz"), 2);
    let mut app = app(&lib, FakeGateway::default(), vec![]);
    app.open_downloaded_chapters("Series").unwrap();

    // ⋮ on vol1 → menu rows are Mark read(0)/unread(1)/Delete download(2).
    let (kx, ky) = tap_row_kebab(0);
    app.activate(0, kx, ky).unwrap();
    let UiEvent::Tap { x, y } = tap_row(2) else {
        unreachable!()
    };
    app.activate(2, x, y).unwrap();

    assert!(
        !lib.join("Series/vol1.cbz").exists(),
        "the chapter file is deleted"
    );
    let Screen::DownloadedChapters { entries, .. } = app.screen() else {
        panic!("back on the downloaded list, rebuilt without the deleted file");
    };
    let rel: Vec<&str> = entries.iter().map(|e| e.relative_path.as_str()).collect();
    assert_eq!(rel, vec!["Series/vol2.cbz"], "the deleted chapter is gone");
}

#[test]
fn nav_first_last_jump_to_the_ends_of_a_listing() {
    let dir = tempfile::tempdir().unwrap();
    let lib = dir.path().join("Manga");
    std::fs::create_dir_all(&lib).unwrap();
    let mut app = app(&lib, FakeGateway::default(), vec![]);
    let chapters: Vec<ChapterEntry> = (1..=40)
        .map(|i| ChapterEntry {
            id: format!("c{i}"),
            num: Some(i as f32),
            title: None,
            lang: None,
        })
        .collect();
    app.stack.push(Screen::ChapterList {
        source: SourceEntry {
            id: "s".into(),
            name: "S".into(),
        },
        manga: MangaEntry {
            id: "m".into(),
            title: "M".into(),
            cover_url: None,
        },
        chapters,
        page: 0,
        sort: ChapterSort::default(),
    });

    let per = layout().rows_per_page();
    let last_page = 40usize.div_ceil(per) - 1;
    assert!(last_page >= 1, "this test needs a multi-page listing");

    // Last → jump straight to the final page (not one step).
    let UiEvent::Tap { x, y } = tap_nav_last() else {
        unreachable!()
    };
    app.handle_tap(x, y).unwrap();
    let Screen::ChapterList { page, .. } = app.screen() else {
        panic!("still on the chapter list")
    };
    assert_eq!(*page, last_page, "Last jumps to the final page");

    // First → straight back to the start.
    let UiEvent::Tap { x, y } = tap_nav_first() else {
        unreachable!()
    };
    app.handle_tap(x, y).unwrap();
    let Screen::ChapterList { page, .. } = app.screen() else {
        panic!("still on the chapter list")
    };
    assert_eq!(*page, 0, "First jumps back to the beginning");
}

#[test]
fn storage_limit_evicts_least_recently_read_downloads() {
    use filetime::{set_file_times, FileTime};

    let dir = tempfile::tempdir().unwrap();
    let lib = dir.path().join("Manga");
    // Three downloaded chapters, recorded in the index with distinct ages.
    for (i, name) in ["a", "b", "c"].iter().enumerate() {
        let path = lib.join("Series").join(format!("{name}.cbz"));
        make_cbz(&path, 6); // each comfortably over the tiny budget below
        let t = FileTime::from_unix_time(1000 + i as i64 * 100, 0); // a oldest
        set_file_times(&path, t, t).unwrap();
    }
    let mut index = gideon_core::SeriesIndex::load(&lib);
    index.record(
        "Series",
        gideon_core::SeriesRef {
            source_id: "src".into(),
            source_name: "Src".into(),
            manga_id: "m1".into(),
            manga_title: "Series".into(),
            ..Default::default()
        },
    );
    for name in ["a", "b", "c"] {
        index.record_download("Series", &format!("ch{name}"), &format!("{name}.cbz"));
    }
    index.save(&lib).unwrap();

    let app = app(&lib, FakeGateway::default(), vec![]);
    // Budget that fits two of the three chapters but not all three.
    let two = std::fs::metadata(lib.join("Series/a.cbz")).unwrap().len()
        + std::fs::metadata(lib.join("Series/b.cbz")).unwrap().len();
    let freed = evict_to_storage_limit(&lib, &app.index_guard, two);

    assert!(freed > 0, "something was evicted to get under budget");
    assert!(
        !lib.join("Series/a.cbz").exists(),
        "the least-recently-read chapter (a) is evicted first"
    );
    assert!(lib.join("Series/b.cbz").exists(), "newer chapters are kept");
    assert!(lib.join("Series/c.cbz").exists(), "newer chapters are kept");
    // The index no longer claims the evicted chapter is downloaded.
    let index = gideon_core::SeriesIndex::load(&lib);
    assert!(
        !index
            .get("Series")
            .unwrap()
            .downloaded
            .values()
            .any(|f| f == "a.cbz"),
        "the evicted chapter is forgotten from the index"
    );
}

#[test]
fn storage_screen_reports_usage_and_frees_space() {
    let dir = tempfile::tempdir().unwrap();
    let lib = dir.path().join("Manga");
    // Storage accounting uses only file sizes (it never opens the archive), so
    // plain files of a known size stand in for chapters. 2 MB each ⇒ 4 MB on
    // disk, comfortably over the 1 MB budget set below (the smallest limit that
    // round-trips through StorageSize's MB-granular settings serialization).
    std::fs::create_dir_all(lib.join("Series")).unwrap();
    let two_mb = vec![0u8; 2 * 1024 * 1024];
    std::fs::write(lib.join("Series/a.cbz"), &two_mb).unwrap();
    std::fs::write(lib.join("Series/b.cbz"), &two_mb).unwrap();
    let mut index = gideon_core::SeriesIndex::load(&lib);
    index.record(
        "Series",
        gideon_core::SeriesRef {
            source_id: "src".into(),
            source_name: "Src".into(),
            manga_id: "m1".into(),
            manga_title: "Series".into(),
            ..Default::default()
        },
    );
    index.record_download("Series", "cha", "a.cbz");
    index.record_download("Series", "chb", "b.cbz");
    index.save(&lib).unwrap();

    let settings_dir = dir.path().join("data");
    let mut app = app(&lib, FakeGateway::default(), vec![]).with_settings_dir(settings_dir.clone());
    let stats = app.storage_stats();
    assert_eq!(stats.chapters, 2, "both downloaded chapters are counted");
    assert_eq!(stats.series, 1);
    assert!(stats.used > 0);

    // A 1 MB budget — well under the 4 MB on disk — so the manual cleanup
    // evicts both chapters.
    let mut s = app.load_settings();
    s.storage_size_limit = gideon_core::StorageSize(1024 * 1024);
    app.save_settings(&s);

    // Open the storage screen and tap "Free up space now" — the last row of
    // content, sitting on the Back bar, wherever the series list ends.
    app.push(Screen::Storage).unwrap();
    let l = layout();
    app.handle_tap(l.width / 2, l.nav_top() - l.row_h / 2)
        .unwrap();
    assert_eq!(
        app.storage_stats().chapters,
        0,
        "the manual cleanup evicted everything over the (tiny) budget"
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
    // [Library, WifiList]; rows: Wi-Fi toggle(0), net(1), "Scan again"(2).
    app.stack.push(Screen::WifiList {
        networks: vec![wifi_net("X", true, false)],
    });
    app.activate(0, 10, 10).unwrap();
    assert!(
        matches!(app.screen(), Screen::Library { .. }),
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
        matches!(app.screen(), Screen::Library { .. }),
        "the Wi-Fi toggle closes the entire menu stack back to the library"
    );
}

#[test]
fn title_taps_off_the_power_icon_are_ignored() {
    let dir = tempfile::tempdir().unwrap();
    // Between the profile zone (left half) and the power zone (right
    // 2 × title_h): a dead-zone tap must do nothing.
    let l = layout();
    let x = (l.width / 2 + l.width.saturating_sub(l.title_h * 2)) / 2;
    let events = vec![tap_today(), UiEvent::Tap { x, y: 5 }];
    let mut app = app(dir.path(), FakeGateway::default(), events);
    app.run().unwrap();
    assert!(matches!(app.screen(), Screen::Stats));
}

// --- profiles ---

/// Tap the left half of the title bar (the profile name).
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
    let events = vec![nav_discover(), tap_title_left()];
    let mut app = app(dir.path(), FakeGateway::default(), events).with_settings_dir(settings_dir);
    app.run().unwrap();

    let Some(Sheet::Profiles { names: profiles }) = app.sheet() else {
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
        nav_discover(),
        tap_title_left(),            // profile menu
        tap_profile_row(1, 2, true), // switch to alex
        tap_nav(0),                  // Library
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

    let mut app = app(&lib, FakeGateway::default(), vec![tap_nav(0)]);
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
        nav_discover(),
        tap_title_left(),            // profile menu
        tap_profile_row(1, 2, true), // switch to alex
        tap_card(1),                 // Sources
        tap_row(0),                  // Listings
        tap_row(0),                  // Popular
        tap_row(0),                  // Manga One
        tap_row(0),                  // download + Reader
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
        nav_discover(),
        tap_title_left(),            // profile menu: [default, New profile…]
        tap_profile_row(1, 1, true), // New profile…
        tap_key(Key::Char('b')),
        tap_key(Key::Char('o')),
        tap_key(Key::Char('b')),
        tap_key(Key::Search), // create
        tap_nav(0),           // Library (of the new profile)
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
fn a_profile_with_a_library_is_listed_even_when_settings_forgot_it() {
    // settings.json can go missing or come back unparseable (a yanked cable
    // mid-write, a hand-edit) — and then it parses as the bare default. A
    // profile must not disappear from the picker just because of that: its
    // library directory on disk is enough to keep it listed.
    let dir = tempfile::tempdir().unwrap();
    let lib = dir.path().join("Manga");
    make_cbz(&lib.join("@alex/Alexs Series/vol1.cbz"), 2);
    make_cbz(&lib.join("@bo/Bos Series/vol1.cbz"), 2);
    let settings_dir = dir.path().join("data");
    std::fs::create_dir_all(&settings_dir).unwrap();
    std::fs::write(settings_dir.join("settings.json"), "{ truncated").unwrap();

    let events = vec![nav_discover(), tap_title_left()];
    let mut app = app(&lib, FakeGateway::default(), events).with_settings_dir(settings_dir);
    app.run().unwrap();

    let Some(Sheet::Profiles { names: profiles }) = app.sheet() else {
        panic!("expected the profile menu");
    };
    assert!(
        profiles.contains(&"alex".to_string()) && profiles.contains(&"bo".to_string()),
        "both on-disk profiles must be listed, got {profiles:?}"
    );
}

#[test]
fn switching_profiles_cannot_persist_a_list_that_read_back_short() {
    // The rescued profiles must survive the next settings write too, otherwise
    // the omission becomes permanent the moment the user switches.
    let dir = tempfile::tempdir().unwrap();
    let lib = dir.path().join("Manga");
    make_cbz(&lib.join("@alex/Alexs Series/vol1.cbz"), 2);
    make_cbz(&lib.join("@bo/Bos Series/vol1.cbz"), 2);
    let settings_dir = dir.path().join("data");
    std::fs::create_dir_all(&settings_dir).unwrap();
    std::fs::write(settings_dir.join("settings.json"), "{ truncated").unwrap();

    let events = vec![
        nav_discover(),
        tap_title_left(),
        tap_profile_row(1, 2, true), // switch to the first rescued profile
    ];
    let mut app = app(&lib, FakeGateway::default(), events).with_settings_dir(settings_dir.clone());
    app.run().unwrap();

    let settings = gideon_core::Settings::load(&settings_dir).unwrap();
    assert!(
        settings.profiles.contains(&"alex".to_string())
            && settings.profiles.contains(&"bo".to_string()),
        "the rewritten list must keep both on-disk profiles, got {:?}",
        settings.profiles
    );
}

#[test]
fn converting_the_default_profile_keeps_profiles_settings_forgot() {
    let dir = tempfile::tempdir().unwrap();
    let lib = dir.path().join("Manga");
    make_cbz(&lib.join("Shared/vol1.cbz"), 2);
    make_cbz(&lib.join("@alex/Alexs Series/vol1.cbz"), 2);
    let settings_dir = dir.path().join("data");
    std::fs::create_dir_all(&settings_dir).unwrap();
    std::fs::write(settings_dir.join("settings.json"), "{ truncated").unwrap();

    let events = vec![
        nav_discover(),
        tap_title_left(),
        tap_profile_row(3, 2, true), // [default, alex, New profile…, Name the default…]
        tap_key(Key::Char('m')),
        tap_key(Key::Char('e')),
        tap_key(Key::Search),
    ];
    let mut app = app(&lib, FakeGateway::default(), events).with_settings_dir(settings_dir.clone());
    app.run().unwrap();

    let settings = gideon_core::Settings::load(&settings_dir).unwrap();
    assert!(
        settings.profiles.contains(&"alex".to_string()),
        "alex has a library on disk and must stay listed, got {:?}",
        settings.profiles
    );
    assert!(lib.join("@alex/Alexs Series/vol1.cbz").is_file());
}

#[test]
fn naming_the_default_profile_converts_it_into_an_ordinary_one() {
    // The default profile's library used to BE the library root — a shape no
    // other profile has. Naming it moves the root's contents (books and the
    // .gideon bookkeeping alike) into "@name", so afterwards every profile is
    // just a directory and "default" is gone.
    let dir = tempfile::tempdir().unwrap();
    let lib = dir.path().join("Manga");
    make_cbz(&lib.join("Shared/vol1.cbz"), 2);
    make_cbz(&lib.join("@alex/Alexs Series/vol1.cbz"), 2);
    std::fs::create_dir_all(lib.join(".gideon")).unwrap();
    std::fs::write(lib.join(".gideon/progress.json"), "{}").unwrap();
    let settings_dir = profile_settings_dir(dir.path(), &["default", "alex"]);

    let events = vec![
        nav_discover(),
        tap_title_left(), // [default, alex, New profile…, Name the default…]
        tap_profile_row(3, 2, true), // Name the default profile…
        tap_key(Key::Char('m')),
        tap_key(Key::Char('e')),
        tap_key(Key::Search), // convert
        tap_nav(0),           // Library (now @me's)
    ];
    let mut app = app(&lib, FakeGateway::default(), events).with_settings_dir(settings_dir.clone());
    app.run().unwrap();

    // The books and the bookkeeping moved; other profiles stayed put.
    assert!(lib.join("@me/Shared/vol1.cbz").is_file());
    assert!(lib.join("@me/.gideon/progress.json").is_file());
    assert!(!lib.join("Shared").exists());
    assert!(lib.join("@alex/Alexs Series/vol1.cbz").is_file());

    // "default" is no longer a profile, and the app is running as the new one.
    let settings = gideon_core::Settings::load(&settings_dir).unwrap();
    assert_eq!(
        settings.profiles,
        vec!["me".to_string(), "alex".to_string()]
    );
    assert_eq!(settings.active_profile, "me");
    let Screen::Library { items, .. } = app.screen() else {
        panic!("expected the converted profile's library");
    };
    let titles: Vec<String> = items.iter().map(|c| c.title()).collect();
    assert_eq!(titles, vec!["Shared".to_string()]);
}

#[test]
fn naming_the_default_profile_a_taken_name_changes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let lib = dir.path().join("Manga");
    make_cbz(&lib.join("Shared/vol1.cbz"), 2);
    make_cbz(&lib.join("@alex/Alexs Series/vol1.cbz"), 2);
    let settings_dir = profile_settings_dir(dir.path(), &["default", "alex"]);

    let events = vec![
        nav_discover(),
        tap_title_left(),
        tap_profile_row(3, 2, true), // Name the default profile…
        tap_key(Key::Char('a')),
        tap_key(Key::Char('l')),
        tap_key(Key::Char('e')),
        tap_key(Key::Char('x')),
        tap_key(Key::Search), // alex already owns @alex
    ];
    let mut app = app(&lib, FakeGateway::default(), events).with_settings_dir(settings_dir.clone());
    app.run().unwrap();

    assert!(matches!(app.screen(), Screen::Message { .. }));
    assert!(lib.join("Shared/vol1.cbz").is_file(), "nothing moved");
    assert!(lib.join("@alex/Alexs Series/vol1.cbz").is_file());
    let settings = gideon_core::Settings::load(&settings_dir).unwrap();
    assert_eq!(
        settings.profiles,
        vec!["default".to_string(), "alex".to_string()]
    );
    assert_eq!(settings.active_profile, "default");
}

#[test]
fn the_profile_menu_stops_offering_conversion_once_default_is_gone() {
    let dir = tempfile::tempdir().unwrap();
    let lib = dir.path().join("Manga");
    let settings_dir = dir.path().join("data");
    gideon_core::Settings {
        profiles: vec!["me".to_string(), "alex".to_string()],
        active_profile: "me".to_string(),
        ..gideon_core::Settings::default()
    }
    .save(&settings_dir)
    .unwrap();

    let events = vec![nav_discover(), tap_title_left()];
    let mut app = app(&lib, FakeGateway::default(), events)
        .with_settings_dir(settings_dir)
        .with_profile("me");
    app.run().unwrap();

    // "Name the default profile…" is only worth offering while a default
    // profile still exists; with none, the sheet is the profiles, a way to
    // make another, and a way out.
    let (title, rows) = app.sheet_rows();
    assert_eq!(title, "Profiles");
    let labels: Vec<&str> = rows.iter().map(|(l, ..)| l.as_str()).collect();
    assert_eq!(labels, vec!["me", "alex", "New profile…", "Close"]);
}

#[test]
fn keyboard_shift_types_uppercase() {
    // The Shift key makes letters upper-case — needed for case-sensitive input
    // (passwords). Drive it through the new-profile keyboard: Shift, b -> "B".
    let dir = tempfile::tempdir().unwrap();
    let lib = dir.path().join("Manga");
    let settings_dir = profile_settings_dir(dir.path(), &["default"]);

    let events = vec![
        nav_discover(),
        tap_title_left(),
        tap_profile_row(1, 1, true), // New profile…
        tap_key(Key::Shift),         // caps on
        tap_key(Key::Char('b')),     // -> 'B'
        tap_key(Key::Shift),         // caps off
        tap_key(Key::Char('o')),
        tap_key(Key::Char('b')),
        tap_key(Key::Search), // create "Bob"
    ];
    let mut app = app(&lib, FakeGateway::default(), events).with_settings_dir(settings_dir.clone());
    app.run().unwrap();

    let settings = gideon_core::Settings::load(&settings_dir).unwrap();
    assert!(
        settings.profiles.contains(&"Bob".to_string()),
        "Shift produced an uppercase B: {:?}",
        settings.profiles
    );
}

#[test]
fn keyboard_shift_and_at_sign_editing() {
    // Unit-level: Shift types each key's upper register — upper-case letters
    // AND the symbols — and Shift/Search don't edit the buffer.
    assert_eq!(
        apply_key_edit("ab", Key::Char('c'), false).as_deref(),
        Some("abc")
    );
    assert_eq!(
        apply_key_edit("ab", Key::Char('c'), true).as_deref(),
        Some("abC")
    );
    // The lockout this fixes: a password with a '!' in it was untypeable,
    // because Shift left the digit row as digits.
    assert_eq!(
        apply_key_edit("pw", Key::Char('1'), true).as_deref(),
        Some("pw!")
    );
    assert_eq!(
        apply_key_edit("a", Key::Char('@'), false).as_deref(),
        Some("a@"),
        "unshifted, the email key still types @"
    );
    assert_eq!(apply_key_edit("a", Key::Shift, true), None);
    // The '@' key is actually on the keyboard — email addresses need it.
    assert!(
        layout()
            .keyboard_keys()
            .iter()
            .any(|(k, ..)| *k == Key::Char('@')),
        "keyboard has an @ key"
    );
}

#[test]
fn picking_the_active_profile_just_closes_the_menu() {
    let dir = tempfile::tempdir().unwrap();
    let settings_dir = profile_settings_dir(dir.path(), &["default", "alex"]);
    let events = vec![nav_discover(), tap_title_left(), tap_row(0)];
    let mut app =
        app(dir.path(), FakeGateway::default(), events).with_settings_dir(settings_dir.clone());
    app.run().unwrap();

    assert!(matches!(app.screen(), Screen::Home));
    let settings = gideon_core::Settings::load(&settings_dir).unwrap();
    assert_eq!(settings.active_profile, "default");
}

// --- removing sources ---

fn one_installed_source_gateway() -> FakeGateway {
    FakeGateway {
        installed: RefCell::new(vec![SourceEntry {
            id: "src".into(),
            name: "Src".into(),
        }]),
        available: Ok(Vec::new()),
        ..FakeGateway::default()
    }
}

#[test]
fn long_press_removes_an_installed_source_after_confirming() {
    let dir = tempfile::tempdir().unwrap();
    let events = vec![
        nav_discover(),
        tap_card(1),       // Home -> Sources
        long_press_row(0), // installed "Src" -> confirmation
        tap_row(0),        // "Remove source"
    ];
    let mut app = app(dir.path(), one_installed_source_gateway(), events);
    app.run().unwrap();

    assert_eq!(
        *app.gateway().uninstalled.borrow(),
        vec!["src".to_string()],
        "confirming must uninstall the source"
    );
    let Screen::Sources { rows, .. } = app.screen() else {
        panic!("expected to land back on the Sources screen");
    };
    assert!(
        !rows
            .iter()
            .any(|r| matches!(r, SourceRow::Installed(s) if s.id == "src")),
        "the removed source must be gone from the list"
    );
}

#[test]
fn cancelling_the_source_removal_keeps_it_installed() {
    let dir = tempfile::tempdir().unwrap();
    let events = vec![
        nav_discover(),
        tap_card(1),       // Home -> Sources
        long_press_row(0), // installed "Src" -> confirmation
        tap_row(1),        // Cancel
    ];
    let mut app = app(dir.path(), one_installed_source_gateway(), events);
    app.run().unwrap();

    assert!(
        app.gateway().uninstalled.borrow().is_empty(),
        "cancel must not uninstall anything"
    );
    assert!(matches!(app.screen(), Screen::Sources { .. }));
}

#[test]
fn long_press_on_an_available_source_is_just_a_tap() {
    // Only installed rows get the removal menu; long-pressing an available
    // source behaves like tapping it (installs it), same as everywhere else.
    let dir = tempfile::tempdir().unwrap();
    let gateway = FakeGateway {
        available: Ok(vec![SourceEntry {
            id: "new".into(),
            name: "New".into(),
        }]),
        ..FakeGateway::default()
    };
    // Row 0 is the "— available —" separator; the source sits on row 1.
    let events = vec![nav_discover(), tap_card(1), long_press_row(1)];
    let mut app = app(dir.path(), gateway, events);
    app.run().unwrap();

    assert!(
        app.gateway()
            .installed
            .borrow()
            .iter()
            .any(|s| s.id == "new"),
        "long press on an available source installs it, like a tap"
    );
    assert!(app.gateway().uninstalled.borrow().is_empty());
}

// --- title display cleanup ---

#[test]
fn tidy_title_collapses_sanitizer_underscores() {
    // "Frieren: Beyond Journey's End" was stored as
    // "Frieren_ Beyond Journey_s End" by the FAT32 sanitizer.
    assert_eq!(
        tidy_title("Frieren_ Beyond Journey_s End"),
        "Frieren Beyond Journey s End"
    );
    assert_eq!(tidy_title("One___Piece"), "One Piece");
    assert_eq!(tidy_title("_Judge_"), "Judge");
    // Unicode passes through untouched.
    assert_eq!(tidy_title("ジャッジ"), "ジャッジ");
    // All-underscore names keep their original form instead of vanishing.
    assert_eq!(tidy_title("___"), "___");
}

#[test]
fn entry_and_card_titles_are_tidied() {
    assert_eq!(
        entry_title("Dr. STONE_ reboot/vol_1.cbz"),
        "Dr. STONE reboot — vol 1"
    );
    let card = SeriesCard {
        series: Some("Kaguya-sama_ Love Is War".into()),
        chapters: vec![LibraryEntry {
            path: PathBuf::from("/lib/Kaguya-sama_ Love Is War/ch1.cbz"),
            relative_path: "Kaguya-sama_ Love Is War/ch1.cbz".into(),
        }],
    };
    assert_eq!(card.title(), "Kaguya-sama Love Is War");
}

// --- settings screen ---

#[test]
fn settings_rows_cycle_and_persist_to_disk() {
    let dir = tempfile::tempdir().unwrap();
    let settings_dir = dir.path().join("data");
    gideon_core::Settings::default()
        .save(&settings_dir)
        .unwrap();

    let mut events = open_settings();
    events.extend(tap_setting("Pre-download ahead")); // 2 -> 3
    events.extend(tap_setting("Pre-download ahead")); // 3 -> 5
    events.extend(tap_setting("Storage limit")); // 2 GB -> 5 GB
    events.extend(tap_setting("Check updates automatically")); // on -> off
    events.push(tap_nav(0));
    let mut app =
        app(dir.path(), FakeGateway::default(), events).with_settings_dir(settings_dir.clone());
    app.run().unwrap();

    assert!(matches!(app.screen(), Screen::Library { .. }));
    let settings = effective_settings(&settings_dir, dir.path());
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
fn idle_suspend_setting_cycles_and_persists() {
    let dir = tempfile::tempdir().unwrap();
    let settings_dir = dir.path().join("data");
    gideon_core::Settings::default()
        .save(&settings_dir)
        .unwrap();

    // Default 15 min -> 30 min (the Nickel/KOReader increments).
    let mut events = open_settings();
    events.extend(tap_setting("Sleep when idle"));
    let mut app =
        app(dir.path(), FakeGateway::default(), events).with_settings_dir(settings_dir.clone());
    app.run().unwrap();

    let settings = gideon_core::Settings::load(&settings_dir).unwrap();
    assert_eq!(settings.idle_suspend_minutes, 30, "15 min cycles to 30 min");
    assert_eq!(
        app.idle_suspend,
        std::time::Duration::from_secs(30 * 60),
        "the live event loops must pick the new timeout up immediately"
    );
}

#[test]
fn idle_suspend_cycles_past_an_hour_to_never() {
    let dir = tempfile::tempdir().unwrap();
    let settings_dir = dir.path().join("data");
    gideon_core::Settings {
        idle_suspend_minutes: 60,
        ..gideon_core::Settings::default()
    }
    .save(&settings_dir)
    .unwrap();

    let mut events = open_settings();
    events.extend(tap_setting("Sleep when idle"));
    let mut app =
        app(dir.path(), FakeGateway::default(), events).with_settings_dir(settings_dir.clone());
    app.run().unwrap();

    let settings = gideon_core::Settings::load(&settings_dir).unwrap();
    assert_eq!(settings.idle_suspend_minutes, 0, "60 min cycles to never");
}

#[test]
fn idle_suspend_never_stays_awake() {
    // "Sleep when idle: never": quiet polls must not suspend, no matter
    // how long the device sits.
    let dir = tempfile::tempdir().unwrap();
    let (count, sleeper) = counting_sleeper();
    let mut app = app(dir.path(), FakeGateway::default(), vec![])
        .with_sleeper(sleeper)
        .with_idle_suspend_minutes(0);
    app.input_mut().idle_timeouts = 5;
    app.run().unwrap();
    assert_eq!(count.get(), 0, "never means never");
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

    let mut events = open_settings();
    events.extend(tap_setting("Storage limit"));
    let mut app =
        app(dir.path(), FakeGateway::default(), events).with_settings_dir(settings_dir.clone());
    app.run().unwrap();

    let settings = effective_settings(&settings_dir, dir.path());
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
    // The cover shelf, so opening the book is one tap on a known cell.
    gideon_core::ProfileSettings {
        library_view: Some("shelf".into()),
        ..Default::default()
    }
    .save(&lib)
    .unwrap();
    let mut events = open_settings();
    events.extend(tap_setting("Reader fit")); // contain -> fit-width
    events.extend([
        tap_nav(0), // Library
        tap_shelf_cell0(),
        reader_tap_next(),
        reader_tap_back(),
    ]);
    let mut app = app(&lib, FakeGateway::default(), events).with_settings_dir(settings_dir.clone());
    app.run().unwrap();

    let settings = effective_settings(&settings_dir, &lib);
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
    let events = vec![tap_nav(0), tap_shelf_cell0(), slide, reader_tap_back()];
    let mut app = app(&lib, FakeGateway::default(), events).with_lights(lights);
    app.run().unwrap();

    assert_eq!(levels.borrow().0, 70, "20 + 50 = 70");
    assert_eq!(levels.borrow().1, 0, "warmth untouched");
}

#[test]
fn edge_slides_follow_reading_orientation_when_rotated() {
    // Regression: the brightness/warmth edge slides must follow the READING
    // orientation, not the physical panel. Upside down (180°), a slide up the
    // reader's RIGHT edge still RAISES brightness. In panel space that same
    // gesture lands on the left edge sliding down — so the old panel-coordinate
    // handling adjusted warmth downward instead (inverted + wrong edge).
    let dir = tempfile::tempdir().unwrap();
    let lib = dir.path().join("Manga");
    make_cbz(&lib.join("Sample/vol1.cbz"), 3);
    let (levels, lights) = lights();

    // Reading-right edge, sliding up by half the height at 180°. map_reader_tap
    // at 180° is (W-1-x, H-1-y), so these panel points map to reading
    // (597, 699) -> (597, 299): right edge, +50 up.
    let slide = UiEvent::Swipe {
        x0: 2,
        y0: 100,
        x1: 2,
        y1: 500,
    };
    let events = vec![
        tap_nav_rot(0, 180),
        tap_row_rot(0, 180),
        tap_shelf_cell0_rot(180),
        slide,
        UiEvent::Tap { x: W / 2, y: H / 2 }, // center is Back at any rotation
    ];
    let mut app = app(&lib, FakeGateway::default(), events)
        .with_lights(lights)
        .with_reader_settings(FitMode::Contain, 180);
    app.run().unwrap();

    assert_eq!(
        levels.borrow().0,
        70,
        "upside down, a right-edge up-slide still raises brightness (20 + 50)"
    );
    assert_eq!(
        levels.borrow().1,
        0,
        "warmth untouched — it's the reader's right edge"
    );
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
    let events = vec![tap_nav(0), tap_shelf_cell0(), slide_up, reader_tap_back()];
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
    let events = vec![tap_nav(0), tap_shelf_cell0(), slide, reader_tap_back()];
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
        tap_nav(0),
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
        tap_nav(0),
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
        tap_nav(0),
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
    let events = vec![tap_nav(0), tap_shelf_cell0(), panel_up, rotated_up];
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
        nav_discover(),
        tap_card(1),       // Sources
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
        tap_nav(0),
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
        tap_nav(0),
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
    let events = vec![tap_nav(0), tap_shelf_cell0(), reader_tap_next(), swipe_down];
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
    let events = vec![tap_nav(0), UiEvent::LongPress { x, y }, tap_book_row(0)];
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

// --- chapter-list sorting & page jumps ---

/// A library with one source-linked series, plus a gateway that serves `n`
/// numbered chapters for it. Opening the card's "All chapters" lands on the
/// source ChapterList.
fn linked_series(dir: &Path, n: usize) -> (PathBuf, FakeGateway) {
    let lib = dir.join("Manga");
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
    // Newest-first, the way real sources serve them.
    let chapters = (1..=n)
        .rev()
        .map(|i| ChapterEntry {
            id: format!("c{i}"),
            num: Some(i as f32),
            title: None,
            lang: None,
        })
        .collect();
    let gateway = FakeGateway {
        chapters,
        ..FakeGateway::default()
    };
    (lib, gateway)
}

/// Open the source chapter list for the single linked series.
fn open_chapters(cell: (u32, u32)) -> Vec<UiEvent> {
    let (x, y) = cell;
    vec![
        tap_nav(0),                  // Home -> Library
        UiEvent::LongPress { x, y }, // card -> book menu
        tap_book_row(0),             // "All chapters (from source)"
    ]
}

#[test]
fn pure_chapter_display_order_sorts_and_reverses() {
    // Source order is preserved as-is.
    let nums = [Some(3.0), Some(1.0), Some(2.0)];
    assert_eq!(
        chapter_display_order(&nums, ChapterSort::Source),
        vec![0, 1, 2]
    );
    // Ascending by number; Descending is its exact reverse.
    assert_eq!(
        chapter_display_order(&nums, ChapterSort::Ascending),
        vec![1, 2, 0]
    );
    assert_eq!(
        chapter_display_order(&nums, ChapterSort::Descending),
        vec![0, 2, 1]
    );
    // Unnumbered chapters keep their order at the end (ascending) and still
    // flip under descending.
    let mixed = [Some(2.0), None, Some(1.0), None];
    assert_eq!(
        chapter_display_order(&mixed, ChapterSort::Ascending),
        vec![2, 0, 1, 3]
    );
    assert_eq!(
        chapter_display_order(&mixed, ChapterSort::Descending),
        vec![3, 1, 0, 2]
    );
}

#[test]
fn pure_label_chapter_num_parses_markers() {
    assert_eq!(label_chapter_num("Ch 12 — Title"), Some(12.0));
    assert_eq!(label_chapter_num("Vol.01 Ch.012.5"), Some(12.5));
    assert_eq!(label_chapter_num("Chapter 7"), Some(7.0));
    assert_eq!(label_chapter_num("#42"), Some(42.0));
    assert_eq!(label_chapter_num("009 - intro"), Some(9.0));
    assert_eq!(label_chapter_num("prologue"), None);
}

#[test]
fn last_button_jumps_to_the_final_page() {
    let dir = tempfile::tempdir().unwrap();
    let (lib, gateway) = linked_series(dir.path(), 30);
    let per_page = layout().rows_per_page();
    let cell = match tap_shelf_cell0() {
        UiEvent::Tap { x, y } => (x, y),
        _ => unreachable!(),
    };
    let mut events = open_chapters(cell);
    events.push(tap_nav_last());
    let mut app = app(&lib, gateway, events);
    app.run().unwrap();

    let Screen::ChapterList { chapters, page, .. } = app.screen() else {
        panic!("expected the chapter list");
    };
    let last = chapters.len().div_ceil(per_page) - 1;
    assert!(last > 0, "30 chapters must span several pages");
    assert_eq!(*page, last, "Last jumps straight to the final page");
}

#[test]
fn first_button_returns_to_the_beginning() {
    let dir = tempfile::tempdir().unwrap();
    let (lib, gateway) = linked_series(dir.path(), 30);
    let cell = match tap_shelf_cell0() {
        UiEvent::Tap { x, y } => (x, y),
        _ => unreachable!(),
    };
    let mut events = open_chapters(cell);
    // Go to the end, then one tap back to the very beginning.
    events.push(tap_nav_last());
    events.push(tap_nav_first());
    let mut app = app(&lib, gateway, events);
    app.run().unwrap();

    let Screen::ChapterList { page, .. } = app.screen() else {
        panic!("expected the chapter list");
    };
    assert_eq!(*page, 0, "First returns to page 0 in one tap");
}

#[test]
fn sort_button_cycles_order_and_resets_page() {
    let dir = tempfile::tempdir().unwrap();
    let (lib, gateway) = linked_series(dir.path(), 30);
    let cell = match tap_shelf_cell0() {
        UiEvent::Tap { x, y } => (x, y),
        _ => unreachable!(),
    };
    let mut events = open_chapters(cell);
    // Move off page 0, then sort: the rows all move, so it snaps back to page 0.
    events.push(tap_nav_last());
    events.push(tap_sort());
    let mut app = app(&lib, gateway, events);
    app.run().unwrap();

    let Screen::ChapterList { sort, page, .. } = app.screen() else {
        panic!("expected the chapter list");
    };
    assert_eq!(
        *sort,
        ChapterSort::Ascending,
        "Source -> Ascending on first tap"
    );
    assert_eq!(*page, 0, "changing the sort returns to the first page");
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
    let events = vec![tap_nav(0), UiEvent::LongPress { x, y }];
    let mut app = app(&lib, FakeGateway::default(), events);
    app.run().unwrap();

    let Some(Sheet::Book { series_dir, .. }) = app.sheet() else {
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
    let events = vec![tap_nav(0), UiEvent::LongPress { x, y }, tap_book_row(0)];
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
    // Row 2 is "Delete this chapter" (row 1 is "Mark as unread"); the delete now
    // routes through a confirmation whose row 0 confirms.
    let events = vec![
        tap_nav(0),
        UiEvent::LongPress { x, y },
        tap_book_row(2),    // Delete this chapter -> confirm screen
        tap_confirm_row(0), // confirm
    ];
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
fn deleting_a_chapter_keeps_its_reading_record() {
    // Removing a downloaded file must not throw away the reader's history/stats
    // for it: the progress row survives the delete, so a per-user reading record
    // stays intact (and resumes if the chapter is re-downloaded).
    let dir = tempfile::tempdir().unwrap();
    let lib = dir.path().join("Manga");
    make_cbz(&lib.join("Series/vol1.cbz"), 2);
    make_cbz(&lib.join("Series/vol2.cbz"), 2);
    let progress_file = progress_path(&lib);
    std::fs::create_dir_all(progress_file.parent().unwrap()).unwrap();
    std::fs::write(
        &progress_file,
        r#"{"progress":{
            "Series/vol1.cbz":{"current_page":1,"total_pages":2,"last_read_at":200}
        },"last_opened":{"Series":"Series/vol1.cbz"}}"#,
    )
    .unwrap();

    let cell = tap_shelf_cell0();
    let UiEvent::Tap { x, y } = cell else {
        unreachable!()
    };
    let events = vec![
        tap_nav(0),
        UiEvent::LongPress { x, y },
        tap_book_row(2),    // Delete this chapter -> confirm screen
        tap_confirm_row(0), // confirm
    ];
    let mut app = app(&lib, FakeGateway::default(), events);
    app.run().unwrap();

    assert!(
        !lib.join("Series/vol1.cbz").exists(),
        "the chapter file is gone"
    );
    let store = ProgressStore::load(&progress_path(&lib)).unwrap();
    assert!(
        store.get("Series/vol1.cbz").is_some(),
        "the reading record for the deleted chapter is preserved"
    );
}

#[test]
fn deleting_a_whole_series_keeps_its_reading_records() {
    // "Keep my shelf clean": deleting the entire series removes its files but
    // must keep every chapter's reading record, so stats persist regardless of
    // library status. progress.json lives at the library root, not inside the
    // series dir, so the recursive dir removal leaves it alone.
    let dir = tempfile::tempdir().unwrap();
    let lib = dir.path().join("Manga");
    make_cbz(&lib.join("Series/vol1.cbz"), 2);
    make_cbz(&lib.join("Series/vol2.cbz"), 2);
    let progress_file = progress_path(&lib);
    std::fs::create_dir_all(progress_file.parent().unwrap()).unwrap();
    std::fs::write(
        &progress_file,
        r#"{"progress":{
            "Series/vol1.cbz":{"current_page":1,"total_pages":2,"last_read_at":200},
            "Series/vol2.cbz":{"current_page":0,"total_pages":2,"last_read_at":100}
        },"last_opened":{"Series":"Series/vol1.cbz"}}"#,
    )
    .unwrap();

    let cell = tap_shelf_cell0();
    let UiEvent::Tap { x, y } = cell else {
        unreachable!()
    };
    let events = vec![
        tap_nav(0),
        UiEvent::LongPress { x, y },
        tap_book_row(3),    // Delete whole series -> confirm screen
        tap_confirm_row(0), // confirm
    ];
    let mut app = app(&lib, FakeGateway::default(), events);
    app.run().unwrap();

    assert!(!lib.join("Series").exists(), "the series files are gone");
    let store = ProgressStore::load(&progress_path(&lib)).unwrap();
    assert!(
        store.get("Series/vol1.cbz").is_some() && store.get("Series/vol2.cbz").is_some(),
        "every chapter's reading record survives deleting the series"
    );
}

#[test]
fn book_menu_delete_asks_for_confirmation_first() {
    // A long-hold that lands on "Delete this chapter" must NOT delete outright —
    // it opens a confirmation, and cancelling leaves every file in place.
    let dir = tempfile::tempdir().unwrap();
    let lib = dir.path().join("Manga");
    make_cbz(&lib.join("Series/vol1.cbz"), 2);
    make_cbz(&lib.join("Series/vol2.cbz"), 2);

    let cell = tap_shelf_cell0();
    let UiEvent::Tap { x, y } = cell else {
        unreachable!()
    };

    // Tapping "Delete this chapter" opens the confirmation without touching disk.
    let events = vec![tap_nav(0), UiEvent::LongPress { x, y }, tap_book_row(2)];
    let mut opened = app(&lib, FakeGateway::default(), events);
    opened.run().unwrap();
    assert!(
        matches!(opened.sheet(), Some(Sheet::ConfirmDelete { .. })),
        "delete asks before removing anything"
    );
    assert!(
        lib.join("Series/vol1.cbz").exists(),
        "nothing is deleted until the user confirms"
    );

    // Now cancel it (confirmation row 1) and confirm the file survives.
    let events = vec![
        tap_nav(0),
        UiEvent::LongPress { x, y },
        tap_book_row(2),    // -> confirm screen
        tap_confirm_row(1), // Cancel
    ];
    let mut cancelled = app(&lib, FakeGateway::default(), events);
    cancelled.run().unwrap();
    assert!(
        cancelled.sheet().is_none(),
        "cancelling dismisses the sheet without touching disk"
    );
    assert!(
        lib.join("Series/vol1.cbz").exists(),
        "cancel keeps the chapter"
    );
    assert!(lib.join("Series/vol2.cbz").exists());
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
    // Row 3 is "Delete whole series" (shifted by the new "Mark as unread" row);
    // confirm on the follow-up screen (row 0).
    let events = vec![
        tap_nav(0),
        UiEvent::LongPress { x, y },
        tap_book_row(3),    // Delete whole series -> confirm screen
        tap_confirm_row(0), // confirm
    ];
    let mut app = app(&lib, FakeGateway::default(), events);
    app.run().unwrap();

    let Screen::Library { items, .. } = app.screen() else {
        panic!("expected refreshed library");
    };
    assert!(items.is_empty(), "whole series gone");
    assert!(!lib.join("Series").exists());
}

#[test]
fn account_menu_signed_out_leads_to_email_sign_in() {
    // With no session on this profile, the account menu offers email sign-in,
    // and tapping it opens the email keyboard. (No network: we stop at the
    // keyboard, before a code is requested.)
    let dir = tempfile::tempdir().unwrap();
    let lib = dir.path().join("Manga");
    std::fs::create_dir_all(&lib).unwrap();
    let mut app = app(&lib, FakeGateway::default(), vec![]);

    assert!(
        app.account_email().is_none(),
        "a fresh profile has no signed-in account"
    );

    app.stack.push(Screen::AccountMenu);
    let UiEvent::Tap { x, y } = tap_row(0) else {
        unreachable!()
    };
    app.activate(0, x, y).unwrap();
    assert!(
        matches!(app.screen(), Screen::AccountEmail { .. }),
        "signed out, the account menu leads to email sign-in"
    );
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
    let events = vec![tap_nav(0), UiEvent::LongPress { x, y }, tap_book_row(1)];
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
        nav_discover(),
        tap_card(1), // Sources
        tap_row(0),  // Listings
        tap_row(0),  // Popular
        tap_row(0),  // Manga One
        tap_row(0),  // download + Reader
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
        nav_discover(),
        tap_card(1),       // Sources
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
        nav_discover(),
        tap_card(1),
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
    let mut app = app(
        dir.path(),
        gateway,
        vec![nav_discover(), tap_card(3), tap_row(0)],
    );
    assert_eq!(app.run().unwrap(), Exit::Restart);
    assert_eq!(
        app.gateway().installs.get(),
        1,
        "tap on prompt should install"
    );
}

// --- popular manga (MyAnimeList tab) ---

#[test]
fn home_popular_lists_titles_and_tap_searches_installed_sources() {
    let dir = tempfile::tempdir().unwrap();
    let gateway = FakeGateway {
        installed: RefCell::new(vec![SourceEntry {
            id: "src1".into(),
            name: "First".into(),
        }]),
        popular: Ok(vec![
            MangaEntry {
                id: "Berserk".into(),
                title: "Berserk".into(),
                cover_url: None,
            },
            MangaEntry {
                id: "Vagabond".into(),
                title: "Vagabond".into(),
                cover_url: None,
            },
        ]),
        search_results: Ok(vec![MangaEntry {
            id: "m1".into(),
            title: "Berserk".into(),
            cover_url: None,
        }]),
        ..FakeGateway::default()
    };
    let events = vec![
        nav_discover(),
        tap_card(2), // Home -> Popular manga
        tap_row(0),  // first popular title -> global search for it
    ];
    let mut app = app(dir.path(), gateway, events);
    app.run().unwrap();

    // Tapping a MyAnimeList title runs a global search for it across the
    // installed sources, so it can be found and downloaded.
    assert_eq!(
        *app.gateway().searches.borrow(),
        vec!["Berserk".to_string()],
        "the tapped popular title drives a source search"
    );
    let Screen::SearchResults { query, results, .. } = app.screen() else {
        panic!("expected search results from a popular title");
    };
    assert_eq!(query, "Berserk");
    assert_eq!(results.len(), 1);
}

#[test]
fn home_popular_renders_the_titles() {
    let dir = tempfile::tempdir().unwrap();
    let gateway = FakeGateway {
        popular: Ok(vec![MangaEntry {
            id: "Berserk".into(),
            title: "Berserk".into(),
            cover_url: None,
        }]),
        ..FakeGateway::default()
    };
    let mut app = app(dir.path(), gateway, vec![nav_discover(), tap_card(2)]);
    app.run().unwrap();

    let Screen::Popular { mangas, .. } = app.screen() else {
        panic!("expected the Popular manga tab");
    };
    assert_eq!(mangas.len(), 1);
    assert!(
        app.display().buffer.iter().any(|&p| p < 0x80),
        "popular tab is blank"
    );
}

#[test]
fn home_popular_empty_explains_instead_of_a_blank_tab() {
    // No popular titles came back (offline, or MyAnimeList hiccup): the user
    // gets a message, not an empty list.
    let dir = tempfile::tempdir().unwrap();
    let mut app = app(
        dir.path(),
        FakeGateway::default(),
        vec![nav_discover(), tap_card(2)],
    );
    app.run().unwrap();

    let Screen::Message { title, .. } = app.screen() else {
        panic!("expected a message for empty popular results");
    };
    assert_eq!(title, "Popular manga");
}

#[test]
fn home_popular_outage_explains_instead_of_an_error_screen() {
    // MyAnimeList down (its API 504s): the user gets a message naming the
    // likely cause, not a raw error screen.
    let dir = tempfile::tempdir().unwrap();
    let gateway = FakeGateway {
        popular: Err("Jikan 504".into()),
        ..FakeGateway::default()
    };
    let mut app = app(dir.path(), gateway, vec![nav_discover(), tap_card(2)]);
    app.run().unwrap();

    let Screen::Message { title, body } = app.screen() else {
        panic!("expected a message for a popular-manga outage");
    };
    assert_eq!(title, "Popular manga");
    assert!(body.contains("MyAnimeList"), "the cause is named: {body}");
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
    let mut app = app(
        dir.path(),
        search_gateway(),
        vec![nav_discover(), tap_card(0)],
    );
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
    let mut app = app(
        dir.path(),
        FakeGateway::default(),
        vec![nav_discover(), tap_card(0)],
    );
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
        nav_discover(),
        tap_card(0), // Home -> global search keyboard
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
        nav_discover(),
        tap_card(0),
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
fn global_search_retries_with_title_variants_on_a_miss() {
    // The source lists the manga under its Japanese title only; the user
    // typed the English one. The search misses, is retried with the
    // MyAnimeList name variants, and finds it under "ジャッジ".
    let dir = tempfile::tempdir().unwrap();
    let mut gateway = search_gateway();
    gateway.variants = vec!["ジャッジ".into()];
    gateway.hit_query = Some("ジャッジ".into());
    let events = vec![
        nav_discover(),
        tap_card(0),
        tap_key(Key::Char('j')),
        tap_key(Key::Search),
    ];
    let mut app = app(dir.path(), gateway, events);
    app.run().unwrap();

    assert_eq!(
        *app.gateway().searches.borrow(),
        vec!["j".to_string(), "ジャッジ".to_string()],
        "the miss must be retried with the variant"
    );
    let Screen::SearchResults { results, .. } = app.screen() else {
        panic!("expected the results screen");
    };
    assert_eq!(results.len(), 1, "the variant's hit is the result");
    assert_eq!(results[0].1.title, "Naruto");
}

#[test]
fn global_search_with_a_hit_never_looks_up_variants() {
    // The raw query matched, so no variant retries: every extra search is
    // another network round-trip per source on the device.
    let dir = tempfile::tempdir().unwrap();
    let mut gateway = search_gateway();
    gateway.variants = vec!["Some Other Name".into()];
    let events = vec![
        nav_discover(),
        tap_card(0),
        tap_key(Key::Char('n')),
        tap_key(Key::Search),
    ];
    let mut app = app(dir.path(), gateway, events);
    app.run().unwrap();

    assert_eq!(
        *app.gateway().searches.borrow(),
        vec!["n".to_string()],
        "a hit must not trigger variant searches"
    );
}

#[test]
fn global_search_with_a_failing_source_still_opens_results() {
    // A source that errors is skipped (logged to stderr), never fatal. Even
    // with no hits the results screen opens, so its "Search more sources"
    // row is there to widen the search.
    let dir = tempfile::tempdir().unwrap();
    let mut gateway = search_gateway();
    gateway.search_results = Err("cloudflare tantrum".into());
    let events = vec![
        nav_discover(),
        tap_card(0),
        tap_key(Key::Char('a')),
        tap_key(Key::Search),
    ];
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
    let events = vec![nav_discover(), tap_card(1), tap_row(0), tap_row(2)];
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
        nav_discover(),
        tap_card(1),
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
    let mut events = vec![nav_discover(), tap_card(1), tap_row(0), tap_row(2)];
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
        nav_discover(),
        tap_card(1),
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
        nav_discover(),
        tap_card(1),
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
        nav_discover(),
        tap_card(1),
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
        nav_discover(),
        tap_card(1),
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
    let events = vec![
        nav_discover(),
        tap_card(1),
        tap_row(0),
        tap_row(2),
        tap_key(Key::Search),
    ];
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
        nav_discover(),
        tap_card(1),
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
        nav_discover(),
        tap_card(1),
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
    let events = vec![
        nav_discover(),
        tap_card(1),
        tap_row(0),
        tap_row(2),
        tap_back(),
    ];
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
        nav_discover(),
        tap_card(0), // Home -> global search keyboard (no history)
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
        nav_discover(),
        tap_card(0),
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
        nav_discover(),
        tap_card(0),
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
        nav_discover(),
        tap_card(0), // Home -> keyboard (no history yet)
        tap_key(Key::Char('n')),
        tap_key(Key::Search), // -> SearchResults, remembers "n"
        tap_back(),           // -> keyboard
        tap_back(),           // -> Discover
        tap_card(0),          // -> RecentSearches (history exists now)
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
        nav_discover(),
        tap_card(0),
        tap_key(Key::Char('n')),
        tap_key(Key::Search), // remembers "n"
        tap_back(),
        tap_back(),
        tap_card(0), // -> RecentSearches
        tap_row(0),  // "New search…" -> keyboard
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
        tap_nav(0), // Library
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
        tap_nav(0),
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
    let events = vec![
        tap_nav(0),
        UiEvent::PageForward,
        UiEvent::PageBack,
        tap_row(0),
    ];
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
    assert!(matches!(app.screen(), Screen::Stats));
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
    assert!(matches!(app.screen(), Screen::Stats));
    // Initial paint, "Sleeping…", the "staying awake" notice, the restore.
    assert_eq!(app.display().flushes.len(), 4);
    assert!(app
        .display()
        .flushes
        .iter()
        .all(|m| *m == RefreshMode::Full));
}

#[test]
fn charging_sleep_finishes_once_unplugged() {
    // Cover closed while charging: the suspend is refused (MTK kernels hang
    // otherwise), but the sleep request must not be forgotten — the moment
    // the charger reads unplugged, the suspend runs. Without this, a device
    // closed in its cover and unplugged later stayed awake until the
    // battery died.
    let dir = tempfile::tempdir().unwrap();
    let count = std::rc::Rc::new(std::cell::Cell::new(0usize));
    let c = count.clone();
    let sleeper: SleepFn = Box::new(move || {
        c.set(c.get() + 1);
        Ok(if c.get() == 1 {
            SleepResult::Skipped // charger was in during the first attempt
        } else {
            SleepResult::Slept
        })
    });
    // …and out again by the time the wait loop probes.
    let charger = Box::new(|| false);
    let mut app = app(dir.path(), FakeGateway::default(), vec![UiEvent::Sleep])
        .with_sleeper(sleeper)
        .with_charger(charger);
    app.run().unwrap();

    assert_eq!(count.get(), 2, "the refused suspend must be retried");
    assert!(matches!(app.screen(), Screen::Stats));
}

#[test]
fn charging_wait_aborts_when_the_user_is_using_the_device() {
    // Still plugged in, and the user taps: they're using it — drop the
    // pending sleep instead of suspending under their fingers later.
    let dir = tempfile::tempdir().unwrap();
    let count = std::rc::Rc::new(std::cell::Cell::new(0usize));
    let c = count.clone();
    let sleeper: SleepFn = Box::new(move || {
        c.set(c.get() + 1);
        Ok(SleepResult::Skipped)
    });
    let charger = Box::new(|| true);
    let events = vec![UiEvent::Sleep, UiEvent::Tap { x: 1, y: 1 }];
    let mut app = app(dir.path(), FakeGateway::default(), events)
        .with_sleeper(sleeper)
        .with_charger(charger);
    app.run().unwrap();

    assert_eq!(count.get(), 1, "a tap must abort the wait, not re-suspend");
    assert!(matches!(app.screen(), Screen::Stats));
}

#[test]
fn idle_menus_auto_suspend_after_the_timeout() {
    // No input past the idle window: suspend as if the cover closed — a
    // user who walks away without a sleep cover otherwise leaves the CPU
    // and Wi-Fi burning all night. A zero threshold makes the first quiet
    // poll cross the wall-clock window immediately.
    let dir = tempfile::tempdir().unwrap();
    let (count, sleeper) = counting_sleeper();
    let mut app = app(dir.path(), FakeGateway::default(), vec![]).with_sleeper(sleeper);
    app.idle_suspend = std::time::Duration::ZERO;
    app.input_mut().idle_timeouts = 1;
    app.run().unwrap();
    assert_eq!(count.get(), 1, "idle past the window must suspend");
}

#[test]
fn quiet_polls_within_the_idle_window_do_not_suspend() {
    // Idle is wall-clock time, not a count of empty polls: on hardware a
    // poll can return "no event" long before its timeout (mid-gesture
    // touch traffic, gyro chatter), and a burst of those must not read as
    // 15 minutes of inactivity — that suspended mid-page-drag.
    let dir = tempfile::tempdir().unwrap();
    let (count, sleeper) = counting_sleeper();
    let mut app = app(dir.path(), FakeGateway::default(), vec![]).with_sleeper(sleeper);
    // Default (15 min) threshold; 100 instant empty polls stay far under it.
    app.input_mut().idle_timeouts = 100;
    app.run().unwrap();
    assert_eq!(count.get(), 0, "empty-poll bursts must not count as idle");
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
        nav_discover(),
        tap_card(1),       // Sources
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
    let events = vec![tap_nav(0), UiEvent::Sleep, tap_row(0)];
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
        tap_nav(0),        // Library
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
fn update_error_body_names_github_not_wifi_when_connected() {
    // A transport failure that reaches the update check *after* ensure_online
    // means Wi-Fi is up but GitHub is unreachable — the message must say so and
    // note the release may not be out yet, never "check that Wi-Fi is on".
    let offline = anyhow::Error::new(gideon_sources::Error::Offline);
    let body = update_error_body(&offline);
    assert!(
        body.contains("Couldn't reach GitHub"),
        "should name GitHub as unreachable: {body}"
    );
    assert!(
        body.contains("isn't published yet") || body.contains("Try again later"),
        "should note the release may not be out / to retry: {body}"
    );
    assert!(
        !body.to_ascii_lowercase().contains("wi-fi is on"),
        "must not tell the user to check Wi-Fi when it's connected: {body}"
    );

    // Non-network failures keep their detail rather than being masked.
    let other = anyhow::anyhow!("downloaded binary is not a valid ELF executable");
    assert!(update_error_body(&other).contains("not a valid ELF"));
}

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
fn today_shows_a_bluetooth_glyph_only_when_a_remote_is_connected() {
    let dir = tempfile::tempdir().unwrap();
    let l = layout();
    // The box just left of the power icon where draw_bluetooth_icon paints,
    // excluding the title-bar separator row (title_h - 1) which is always drawn.
    let power_cx = l.width.saturating_sub(l.title_h / 2 + l.pad);
    let cx = power_cx.saturating_sub(l.title_h);
    let half = l.title_h / 4;
    let (x0, x1) = (cx.saturating_sub(half), (cx + half).min(l.width));
    let (y0, y1) = (1u32, l.title_h.saturating_sub(2).min(l.height));
    let ink = |buf: &[u8]| -> usize {
        let mut n = 0;
        for y in y0..y1 {
            for x in x0..x1 {
                if buf[(y * l.width + x) as usize] < 0x80 {
                    n += 1;
                }
            }
        }
        n
    };
    let render = |connected: bool| {
        let mut app = UiApp::new(
            MemoryDisplay::new(W, H),
            FakeInput::new(vec![]).with_bluetooth(connected),
            FakeGateway::default(),
            dir.path().to_path_buf(),
        );
        app.goto_root(Screen::Stats).unwrap();
        ink(&app.display().buffer)
    };

    assert_eq!(render(false), 0, "no glyph when no remote is connected");
    assert!(
        render(true) > 0,
        "the Bluetooth glyph shows when a remote is connected"
    );
}

/// Prime the local "send to Kobo" cache the sync sweep would normally write, so
/// the bell/list can be exercised without a network round-trip.
fn seed_sends(library_dir: &Path, items: &[(&str, &str)]) {
    let sends: Vec<gideon_sync::supabase::SendItem> = items
        .iter()
        .map(|(id, title)| gideon_sync::supabase::SendItem {
            id: (*id).to_string(),
            title: (*title).to_string(),
        })
        .collect();
    let dir = library_dir.join(".gideon");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("sends.json"), serde_json::to_vec(&sends).unwrap()).unwrap();
}

#[test]
fn today_shows_a_notification_bell_only_when_the_web_has_queued_sends() {
    let l = layout();
    // Slot 1 sits one title-bar height left of the power symbol — where the
    // bell paints — minus the always-drawn title separator row.
    let power_cx = l.width.saturating_sub(l.title_h / 2 + l.pad);
    let cx = power_cx.saturating_sub(l.title_h);
    let half = l.title_h / 4;
    let (x0, x1) = (cx.saturating_sub(half), (cx + half).min(l.width));
    let (y0, y1) = (1u32, l.title_h.saturating_sub(2).min(l.height));
    let ink = |buf: &[u8]| -> usize {
        let mut n = 0;
        for y in y0..y1 {
            for x in x0..x1 {
                if buf[(y * l.width + x) as usize] < 0x80 {
                    n += 1;
                }
            }
        }
        n
    };
    let render = |seeded: bool| {
        let dir = tempfile::tempdir().unwrap();
        if seeded {
            seed_sends(dir.path(), &[("s1", "Berserk")]);
        }
        let mut app = app(dir.path(), FakeGateway::default(), vec![]);
        app.goto_root(Screen::Stats).unwrap();
        ink(&app.display().buffer)
    };

    assert_eq!(render(false), 0, "no bell when nothing is queued");
    assert!(render(true) > 0, "the bell shows when a send is waiting");
}

#[test]
fn tapping_the_bell_opens_the_sent_list() {
    let dir = tempfile::tempdir().unwrap();
    seed_sends(dir.path(), &[("s1", "Berserk")]);
    let l = layout();
    // The bell lives in slot 1: between the far-right power zone (width - th)
    // and one more title-bar height to the left.
    let bell = UiEvent::Tap {
        x: l.width - l.title_h - l.title_h / 2,
        y: 1,
    };
    let mut app = app(dir.path(), FakeGateway::default(), vec![tap_today(), bell]);
    app.run().unwrap();
    assert!(
        matches!(app.screen(), Screen::SentList { items } if items.len() == 1),
        "tapping the bell opens the list of queued sends"
    );
}

#[test]
fn opening_a_sent_item_searches_for_it_and_clears_the_badge() {
    let dir = tempfile::tempdir().unwrap();
    seed_sends(dir.path(), &[("s1", "Berserk")]);
    let gateway = FakeGateway {
        installed: RefCell::new(vec![SourceEntry {
            id: "src".into(),
            name: "Src".into(),
        }]),
        search_results: Ok(vec![MangaEntry {
            id: "m1".into(),
            title: "Berserk".into(),
            cover_url: None,
        }]),
        ..FakeGateway::default()
    };
    let l = layout();
    let bell = UiEvent::Tap {
        x: l.width - l.title_h - l.title_h / 2,
        y: 1,
    };
    let mut app = app(dir.path(), gateway, vec![tap_today(), bell, tap_row(0)])
        .with_online_probe(Box::new(|| true));
    app.run().unwrap();

    let Screen::SearchResults { query, results, .. } = app.screen() else {
        panic!("opening a sent item should land on its search results");
    };
    assert_eq!(query, "Berserk", "the queued title drives the search");
    assert_eq!(results.len(), 1, "the source's hit is shown to pick from");
    assert!(
        crate::sync::cached_sends(dir.path()).is_empty(),
        "opening a send clears it from the local badge cache"
    );
}

#[test]
fn battery_probe_feeds_today_and_sleep_without_breaking_either() {
    let dir = tempfile::tempdir().unwrap();
    let (count, sleeper) = counting_sleeper();
    let reads = std::rc::Rc::new(std::cell::Cell::new(0usize));
    let probe = reads.clone();
    let mut app = app(
        dir.path(),
        FakeGateway::default(),
        vec![tap_today(), UiEvent::Sleep],
    )
    .with_sleeper(sleeper)
    .with_battery(Box::new(move || {
        probe.set(probe.get() + 1);
        Some(47)
    }));
    app.run().unwrap();

    assert_eq!(count.get(), 1, "sleep still suspends with a battery probe");
    assert!(
        reads.get() >= 2,
        "both the Today title and the sleep notice must read the battery"
    );
    assert!(matches!(app.screen(), Screen::Stats));
    assert!(
        app.display().buffer.iter().any(|&p| p < 0x80),
        "today screen is blank"
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
    let mut app = app(
        dir.path(),
        gateway,
        vec![nav_discover(), tap_card(3), tap_back()],
    );
    app.run().unwrap();
    assert_eq!(app.gateway().installs.get(), 0, "back should not install");
}

/// A source-linked series with only its first chapter on disk — the state the
/// reader ends up in when the look-ahead didn't manage to stock the next one.
fn one_chapter_fixture(lib: &Path) {
    make_cbz(&lib.join("Series/vol1.cbz"), 2);
    let mut index = gideon_core::SeriesIndex::load(lib);
    index.record(
        "Series",
        gideon_core::SeriesRef {
            source_id: "src".into(),
            source_name: "Src".into(),
            manga_id: "m1".into(),
            manga_title: "Series".into(),
            ..Default::default()
        },
    );
    index.record_download("Series", "c1", "vol1.cbz");
    index.save(lib).unwrap();
}

#[test]
fn library_reading_downloads_the_next_chapter_when_it_runs_out() {
    // The bug: reading a series from the library shelf only ever chained
    // through chapters already on disk. Turning past the last page of the last
    // downloaded chapter did nothing, and the only way on was backing out to
    // the chapter list and tapping the next chapter by hand. Now the turn
    // itself fetches it from the source and keeps reading.
    let dir = tempfile::tempdir().unwrap();
    let lib = dir.path().join("Manga");
    one_chapter_fixture(&lib);

    let gateway = FakeGateway {
        installed: RefCell::new(vec![SourceEntry {
            id: "src".into(),
            name: "Src".into(),
        }]),
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
        download: Some(Box::new(|library, progress| {
            let path = library.join("Series/vol2.cbz");
            make_cbz(&path, 2);
            progress(2, 2);
            Ok(path)
        })),
        ..FakeGateway::default()
    };

    let events = vec![
        tap_nav(0),        // Home -> Library
        tap_shelf_cell0(), // the card -> vol1 (the only download)
        reader_tap_next(), // vol1 page 2 (last)
        reader_tap_next(), // past the end -> fetch and open c2
        reader_tap_back(),
    ];
    let mut app = app(&lib, gateway, events);
    app.run().unwrap();

    assert!(
        lib.join("Series/vol2.cbz").exists(),
        "the next chapter was downloaded from the source"
    );
    let store = ProgressStore::load(&progress_path(&lib)).unwrap();
    assert!(
        store.get("Series/vol2.cbz").is_some(),
        "reading continued into the freshly downloaded chapter"
    );
}

#[test]
fn library_reading_says_so_when_the_series_is_finished() {
    // Same path, nothing left to fetch: the turn must explain itself rather
    // than silently doing nothing.
    let dir = tempfile::tempdir().unwrap();
    let lib = dir.path().join("Manga");
    one_chapter_fixture(&lib);

    let gateway = FakeGateway {
        installed: RefCell::new(vec![SourceEntry {
            id: "src".into(),
            name: "Src".into(),
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
        tap_nav(0),
        tap_shelf_cell0(),
        reader_tap_next(),
        reader_tap_next(), // past the end -> nothing newer at the source
    ];
    let mut app = app(&lib, gateway, events);
    app.run().unwrap();

    let Screen::Message { title, body } = app.screen() else {
        panic!("expected a message about the next chapter");
    };
    assert_eq!(title, "Next chapter");
    assert!(body.contains("up to date"), "body was {body:?}");
}

#[test]
fn a_failed_lookahead_is_retried_by_the_next_kick() {
    // The wake case: the look-ahead fired while the radio was still down after
    // a suspend and failed, so the next chapter never landed — and, because the
    // worker had already recorded it as "queued", nothing could ever ask for it
    // again in that session. Waking re-fires the look-ahead
    // (`rekick_lookahead`), and this proves the re-fire actually reaches the
    // worker instead of being swallowed by the dedup set.
    let dir = tempfile::tempdir().unwrap();
    let lib = dir.path().join("Manga");
    std::fs::create_dir_all(&lib).unwrap();

    let gateway = FailFirstGateway::new("Manga One");
    let started = std::sync::Arc::clone(&gateway.started);
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
    let chapters: Vec<ChapterEntry> = (1..=2)
        .map(|i| ChapterEntry {
            id: format!("c{i}"),
            num: Some(i as f32),
            title: None,
            lang: Some("en".into()),
        })
        .collect();

    // Read c1: the look-ahead queues c2, whose first attempt fails.
    app.predownload_ahead(&source, &manga, &chapters, "c1");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while started.load(std::sync::atomic::Ordering::SeqCst) < 1
        && std::time::Instant::now() < deadline
    {
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(
        app.downloaded_chapter_path(&source, &manga, "c2").is_none(),
        "the first attempt failed, so nothing is on disk yet"
    );

    // Waking re-fires the same look-ahead — and this time it lands.
    app.rekick_lookahead();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        if app.downloaded_chapter_path(&source, &manga, "c2").is_some() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    assert!(
        app.downloaded_chapter_path(&source, &manga, "c2").is_some(),
        "the re-kick after wake retried the chapter that failed while offline"
    );
}

/// Write a progress store directly, so a stats test can describe a reading
/// history without having to simulate weeks of page turns.
fn write_progress(lib: &Path, entries: &[(&str, usize, usize, u64)]) {
    let dir = lib.join(".gideon");
    std::fs::create_dir_all(&dir).unwrap();
    let body: Vec<String> = entries
        .iter()
        .map(|(key, page, total, at)| {
            format!(
                "\"{key}\":{{\"current_page\":{page},\"total_pages\":{total},\"last_read_at\":{at}}}"
            )
        })
        .collect();
    std::fs::write(
        dir.join("progress.json"),
        format!("{{\"progress\":{{{}}}}}", body.join(",")),
    )
    .unwrap();
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

#[test]
fn home_opens_reading_stats_and_draws_it_in_color() {
    // The stats screen exists to carry the heatmap, and the heatmap is a
    // colour ramp — so it must take the RGB blit path, not fall back to gray.
    let dir = tempfile::tempdir().unwrap();
    let lib = dir.path().join("Manga");
    make_cbz(&lib.join("Berserk/vol1.cbz"), 4);
    write_progress(&lib, &[("Berserk/vol1.cbz", 3, 4, now_unix())]);

    let events = vec![tap_nav(1)];
    let mut app = app(&lib, FakeGateway::default(), events);
    app.run().unwrap();

    assert!(
        matches!(app.screen(), Screen::Stats),
        "row 6 should open Reading stats"
    );
    assert_eq!(
        app.display().blits.last(),
        Some(&true),
        "the stats screen must blit as colour"
    );
}

#[test]
fn the_heatmap_darkens_a_day_that_was_read() {
    // A day with reading has to be visibly darker than an untouched one,
    // otherwise the whole widget is decoration.
    let dir = tempfile::tempdir().unwrap();
    let lib = dir.path().join("Manga");
    make_cbz(&lib.join("Berserk/vol1.cbz"), 40);
    write_progress(&lib, &[("Berserk/vol1.cbz", 39, 40, now_unix())]);

    let mut app = app(&lib, FakeGateway::default(), vec![tap_nav(1)]);
    app.run().unwrap();

    // The empty-day step is 0xEE; a read day is darker than that everywhere
    // in every ramp. Somewhere below the tiles there must be such a pixel.
    let l = layout();
    let dark =
        (l.content_top()..l.height).any(|y| (0..l.width).any(|x| app.display().pixel(x, y) < 0xE0));
    assert!(dark, "no heatmap ink drawn for a day that was read");
}

#[test]
fn stats_survive_an_empty_library() {
    // A fresh device has no progress at all; the screen must still compose
    // rather than dividing by a zero maximum somewhere in the ramp.
    let dir = tempfile::tempdir().unwrap();
    let lib = dir.path().join("Manga");
    std::fs::create_dir_all(&lib).unwrap();

    let mut app = app(&lib, FakeGateway::default(), vec![tap_nav(1)]);
    app.run().unwrap();
    assert!(matches!(app.screen(), Screen::Stats));
}

#[test]
#[ignore]
fn dump_stats_screen_png() {
    // Not part of CI: renders the stats screen at real Libra Colour size, in
    // colour, for eyeballing. Set GIDEON_PROFILE to try another palette.
    // `cargo test -p gideon-app dump_stats_screen_png -- --ignored`
    let dir = tempfile::tempdir().unwrap();
    let lib = dir.path().join("Manga");
    make_cbz(&lib.join("Vinland Saga/vol1.cbz"), 40);
    let now = now_unix();
    // A plausible, irregular history so the ramp actually has range.
    let pattern = [
        0usize, 14, 3, 0, 41, 22, 0, 0, 7, 33, 18, 0, 9, 27, 0, 44, 2, 0, 11, 36,
    ];
    let mut entries: Vec<(String, usize, usize, u64)> = Vec::new();
    for d in 0..126u64 {
        let pages = pattern[(d as usize * 7 + d as usize / 5) % pattern.len()];
        if pages == 0 {
            continue;
        }
        entries.push((
            format!("Vinland Saga/ch{d}.cbz"),
            pages - 1,
            pages,
            now - d * 86_400,
        ));
    }
    let refs: Vec<(&str, usize, usize, u64)> = entries
        .iter()
        .map(|(k, p, t, a)| (k.as_str(), *p, *t, *a))
        .collect();
    write_progress(&lib, &refs);

    let (w, h) = (1264, 1680);
    let settings_dir = dir.path().join("data");
    if let Ok(profile) = std::env::var("GIDEON_PROFILE") {
        gideon_core::Settings {
            color_profile: profile,
            ..gideon_core::Settings::default()
        }
        .save(&settings_dir)
        .unwrap();
    }
    let mut app = UiApp::new(
        MemoryDisplay::new(w, h),
        FakeInput::new(vec![]),
        FakeGateway::default(),
        lib.clone(),
    )
    .with_settings_dir(settings_dir);
    app.push(Screen::Stats).unwrap();

    // compose_stats is private, but this module is a child of `ui`.
    let page = app.compose_stats();
    let mut img = image::RgbImage::new(page.width, page.height);
    for y in 0..page.height {
        for x in 0..page.width {
            img.put_pixel(x, y, image::Rgb(page.pixel(x, y)));
        }
    }
    let out = std::env::var("GIDEON_DUMP").unwrap_or_else(|_| "stats.png".into());
    img.save(&out).unwrap();
    eprintln!("wrote {out}");
}

#[test]
fn the_library_title_bar_toggles_between_shelf_and_list() {
    // Two views of one library, and the choice has to survive a restart —
    // it is stored in settings, not in the screen.
    let dir = tempfile::tempdir().unwrap();
    let lib = dir.path().join("Manga");
    make_cbz(&lib.join("Berserk/vol1.cbz"), 3);
    let settings_dir = dir.path().join("data");
    gideon_core::Settings::default()
        .save(&settings_dir)
        .unwrap();

    let l = layout();
    let title_tap = UiEvent::Tap {
        x: l.width / 2,
        y: l.title_h / 2,
    };
    let mut app = app(&lib, FakeGateway::default(), vec![tap_nav(0), title_tap])
        .with_settings_dir(settings_dir.clone());
    app.run().unwrap();

    assert_eq!(
        effective_settings(&settings_dir, &lib).library_view,
        "shelf",
        "the list is the default, so a title-bar tap swaps to the cover shelf"
    );
    assert!(
        matches!(app.screen(), Screen::Library { .. }),
        "still the Library"
    );
}

#[test]
fn the_list_view_draws_in_colour_and_the_shelf_does_not() {
    // The list carries score chips and a progress bar in the theme colour,
    // so it must take the RGB path; a coverless shelf has no colour to show.
    let dir = tempfile::tempdir().unwrap();
    let lib = dir.path().join("Manga");
    make_cbz(&lib.join("Berserk/vol1.cbz"), 3);
    let settings_dir = dir.path().join("data");

    for (view, want_colour) in [("shelf", false), ("list", true)] {
        // The view is a per-profile setting, so it has to be written to the
        // profile's own file — a device-file value would be overlaid away.
        gideon_core::ProfileSettings {
            library_view: Some(view.into()),
            ..Default::default()
        }
        .save(&lib)
        .unwrap();
        let mut app = app(&lib, FakeGateway::default(), vec![tap_nav(0)])
            .with_settings_dir(settings_dir.clone());
        app.run().unwrap();
        assert_eq!(
            app.compose_color_current().unwrap().is_some(),
            want_colour,
            "{view} view colour path"
        );
    }
}

#[test]
fn discover_offers_only_what_the_tabs_do_not_and_keeps_its_nav_bar() {
    // Discover used to be the whole menu, and still listed Library, Settings
    // and Reading stats after those became nav destinations — a tab bar drawn
    // over a menu that duplicated it. It also carried a copy of Today's
    // reading band, so two tabs showed the same thing. What is left is the
    // one job no tab does: finding something new to read.
    let dir = tempfile::tempdir().unwrap();
    let lib = dir.path().join("Manga");
    make_cbz(&lib.join("Berserk/vol1.cbz"), 4);
    write_progress(&lib, &[("Berserk/vol1.cbz", 3, 4, now_unix())]);

    let mut app = app(&lib, FakeGateway::default(), vec![nav_discover()]);
    app.run().unwrap();
    assert!(matches!(app.screen(), Screen::Home));

    for tab in ["Library", "Settings", "Reading stats"] {
        assert!(
            !super::HOME_ROWS.contains(&tab),
            "{tab} is a nav destination; Discover must not list it too"
        );
    }

    // Discover takes the colour path unconditionally — the nav bar is drawn
    // in RGB, and without it the screen shows no route anywhere else.
    let page = app
        .compose_color_current()
        .unwrap()
        .expect("Discover composes in colour so it can draw the nav bar");
    let l = layout();
    let strip = l.nav_top() + l.nav_h / 2;
    let inked = (0..l.width)
        .filter(|&x| page.pixel(x, strip) != [0xFF; 3])
        .count();
    assert!(inked > 0, "the nav bar must actually be drawn on Discover");
}

/// Cover tints for the dump fixtures — muted the way real cover art reads
/// once Kaleido has taken its cut, so the screenshot is not misleading.
const COVER_TINTS: [[u8; 3]; 6] = [
    [0x6f, 0x7d, 0x86],
    [0x7a, 0x5c, 0x52],
    [0x5f, 0x6f, 0x78],
    [0x5a, 0x5a, 0x5a],
    [0x6d, 0x62, 0x55],
    [0x6b, 0x64, 0x70],
];

#[test]
#[ignore]
fn dump_library_list_png() {
    // Not part of CI: renders the dense Library list at real Libra Colour
    // size, with metadata, for eyeballing.
    let dir = tempfile::tempdir().unwrap();
    let lib = dir.path().join("Manga");
    let now = now_unix();
    let series = [
        (
            "Vinland Saga",
            8.74f32,
            "Publishing",
            vec!["Action", "Drama", "Historical"],
            213u32,
            18usize,
            54usize,
            2u64,
        ),
        (
            "Chainsaw Man",
            8.61,
            "Publishing",
            vec!["Action", "Supernatural"],
            97,
            9,
            76,
            4,
        ),
        (
            "Frieren",
            9.02,
            "Publishing",
            vec!["Adventure", "Fantasy"],
            126,
            12,
            52,
            6,
        ),
        (
            "Blue Period",
            8.33,
            "Publishing",
            vec!["Drama", "Slice of Life"],
            74,
            4,
            9,
            14,
        ),
        (
            "Oyasumi Punpun",
            9.01,
            "Finished",
            vec!["Drama", "Psychological"],
            147,
            3,
            147,
            30,
        ),
        (
            "Dandadan",
            8.55,
            "Publishing",
            vec!["Action", "Comedy", "Sci-Fi"],
            188,
            5,
            31,
            45,
        ),
    ];
    let mut index = gideon_core::SeriesIndex::default();
    let mut progress = Vec::new();
    for (title, score, status, genres, total, downloaded, read, days) in &series {
        for i in 0..*downloaded {
            make_cbz(&lib.join(format!("{title}/ch{i}.cbz")), 20);
        }
        // A 2:3 cover in a distinct tint, so the dump shows what the row
        // actually looks like rather than the fixture's black 8x8 page.
        let tint = COVER_TINTS
            [series.iter().position(|(t, ..)| t == title).unwrap_or(0) % COVER_TINTS.len()];
        let cover = image::RgbImage::from_pixel(200, 300, image::Rgb(tint));
        image::DynamicImage::ImageRgb8(cover)
            .save(lib.join(title).join(".cover.jpg"))
            .unwrap();
        index.record(
            title,
            gideon_core::SeriesRef {
                source_id: "s".into(),
                source_name: "src".into(),
                manga_id: "m".into(),
                manga_title: (*title).into(),
                meta: Some(gideon_core::SeriesMeta {
                    score: Some(*score),
                    status: Some((*status).into()),
                    genres: genres.iter().map(|g| (*g).to_string()).collect(),
                    rank: None,
                    total_chapters: Some(*total),
                    fetched_at: None,
                }),
                ..Default::default()
            },
        );
        for i in 0..(*read).min(downloaded.saturating_sub(*days as usize % 5)) {
            progress.push((
                format!("{title}/ch{i}.cbz"),
                19usize,
                20usize,
                now - days * 86_400,
            ));
        }
    }
    index.save(&lib).unwrap();
    let refs: Vec<(&str, usize, usize, u64)> = progress
        .iter()
        .map(|(k, p, t, a)| (k.as_str(), *p, *t, *a))
        .collect();
    write_progress(&lib, &refs);

    let settings_dir = dir.path().join("data");
    gideon_core::Settings {
        library_view: "list".into(),
        color_profile: std::env::var("GIDEON_PROFILE").unwrap_or_else(|_| "ink-rust".into()),
        ..gideon_core::Settings::default()
    }
    .save(&settings_dir)
    .unwrap();

    let mut app = UiApp::new(
        MemoryDisplay::new(1264, 1680),
        FakeInput::new(vec![]),
        FakeGateway::default(),
        lib.clone(),
    )
    .with_settings_dir(settings_dir);
    app.open_library().unwrap();

    let page = app
        .compose_color_current()
        .unwrap()
        .expect("list view is colour");
    let mut img = image::RgbImage::new(page.width, page.height);
    for y in 0..page.height {
        for x in 0..page.width {
            img.put_pixel(x, y, image::Rgb(page.pixel(x, y)));
        }
    }
    let out = std::env::var("GIDEON_DUMP").unwrap_or_else(|_| "library.png".into());
    img.save(&out).unwrap();
    eprintln!("wrote {out}");
}

#[test]
fn two_profiles_keep_their_own_view_and_share_the_device_settings() {
    // The point of the split: personal taste follows the reader, hardware
    // follows the device. One Kobo, two people, one frontlight.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("Manga");
    let settings_dir = profile_settings_dir(dir.path(), &["default", "alex"]);
    make_cbz(&root.join("Shared/vol1.cbz"), 2);
    make_cbz(&root.join("@alex/Alexs/vol1.cbz"), 2);

    // Default flips to the dense list; alex is untouched.
    let l = layout();
    let title_tap = UiEvent::Tap {
        x: l.width / 2,
        y: l.title_h / 2,
    };
    let mut a = app(&root, FakeGateway::default(), vec![tap_nav(0), title_tap])
        .with_settings_dir(settings_dir.clone());
    a.run().unwrap();

    let alex_lib = root.join("@alex");
    assert_eq!(
        effective_settings(&settings_dir, &root).library_view,
        "shelf",
        "the profile that toggled should have left the default list view"
    );
    assert_eq!(
        effective_settings(&settings_dir, &alex_lib).library_view,
        "list",
        "the other profile keeps the default, and must not inherit the toggle"
    );

    // Device-global settings stay shared: a storage-limit change made by one
    // profile is the same limit the other sees, because there is one disk.
    let before = effective_settings(&settings_dir, &alex_lib).storage_size_limit;
    let mut storage_events = open_settings();
    storage_events.extend(tap_setting("Storage limit"));
    let mut b =
        app(&root, FakeGateway::default(), storage_events).with_settings_dir(settings_dir.clone());
    b.run().unwrap();
    let after_self = effective_settings(&settings_dir, &root).storage_size_limit;
    let after_other = effective_settings(&settings_dir, &alex_lib).storage_size_limit;
    assert_ne!(before, after_self, "the storage limit should have cycled");
    assert_eq!(
        after_self, after_other,
        "one disk, one limit: both profiles must see the same value"
    );
}

#[test]
#[ignore]
fn dump_today_png() {
    // Not part of CI: renders Home at real Libra Colour size for eyeballing.
    let dir = tempfile::tempdir().unwrap();
    let lib = dir.path().join("Manga");
    make_cbz(&lib.join("Vinland Saga/vol1.cbz"), 40);
    let now = now_unix();
    let pattern = [
        0usize, 14, 3, 0, 41, 22, 0, 0, 7, 33, 18, 0, 9, 27, 0, 44, 2, 0, 11, 36,
    ];
    let mut entries: Vec<(String, usize, usize, u64)> = Vec::new();
    for d in 0..126u64 {
        let pages = pattern[(d as usize * 7 + d as usize / 5) % pattern.len()];
        if pages == 0 {
            continue;
        }
        entries.push((
            format!("Vinland Saga/ch{d}.cbz"),
            pages - 1,
            pages,
            now - d * 86_400,
        ));
    }
    let refs: Vec<(&str, usize, usize, u64)> = entries
        .iter()
        .map(|(k, p, t, a)| (k.as_str(), *p, *t, *a))
        .collect();
    write_progress(&lib, &refs);

    let settings_dir = dir.path().join("data");
    gideon_core::Settings {
        color_profile: std::env::var("GIDEON_PROFILE").unwrap_or_else(|_| "ink-rust".into()),
        ..gideon_core::Settings::default()
    }
    .save(&settings_dir)
    .unwrap();

    let mut app = UiApp::new(
        MemoryDisplay::new(1264, 1680),
        FakeInput::new(vec![]),
        FakeGateway::default(),
        lib.clone(),
    )
    .with_settings_dir(settings_dir);
    app.push(Screen::Stats).unwrap();

    let page = app
        .compose_color_current()
        .unwrap()
        .expect("Home draws its band in colour");
    let mut img = image::RgbImage::new(page.width, page.height);
    for y in 0..page.height {
        for x in 0..page.width {
            img.put_pixel(x, y, image::Rgb(page.pixel(x, y)));
        }
    }
    let out = std::env::var("GIDEON_DUMP").unwrap_or_else(|_| "home.png".into());
    img.save(&out).unwrap();
    eprintln!("wrote {out}");
}

/// Render any screen to a PNG for the 1.0 demo. Not part of CI.
/// `GIDEON_SCREEN` picks the screen, `GIDEON_PROFILE` the colour profile.
/// `cargo test -p gideon-app dump_demo -- --ignored`
#[test]
#[ignore]
fn dump_demo() {
    let dir = tempfile::tempdir().unwrap();
    let lib = dir.path().join("Manga");
    let now = now_unix();
    let series = [
        (
            "Vinland Saga",
            8.74f32,
            "Publishing",
            vec!["Action", "Drama", "Historical"],
            213u32,
            18usize,
            16usize,
            2u64,
        ),
        (
            "Chainsaw Man",
            8.61,
            "Publishing",
            vec!["Action", "Supernatural"],
            97,
            9,
            5,
            4,
        ),
        (
            "Frieren",
            9.02,
            "Publishing",
            vec!["Adventure", "Fantasy"],
            126,
            12,
            11,
            6,
        ),
        (
            "Oyasumi Punpun",
            9.01,
            "Finished",
            vec!["Drama", "Psychological"],
            147,
            3,
            3,
            30,
        ),
        (
            "Dandadan",
            8.55,
            "Publishing",
            vec!["Action", "Comedy", "Sci-Fi"],
            188,
            5,
            5,
            45,
        ),
        (
            "Blue Period",
            8.33,
            "Publishing",
            vec!["Drama", "Slice of Life"],
            74,
            4,
            0,
            14,
        ),
    ];
    let mut index = gideon_core::SeriesIndex::default();
    let mut progress: Vec<(String, usize, usize, u64)> = Vec::new();
    for (i, (title, score, status, genres, total, downloaded, read, days)) in
        series.iter().enumerate()
    {
        for c in 0..*downloaded {
            make_cbz(&lib.join(format!("{title}/ch{c}.cbz")), 20);
        }
        let cover =
            image::RgbImage::from_pixel(200, 300, image::Rgb(COVER_TINTS[i % COVER_TINTS.len()]));
        image::DynamicImage::ImageRgb8(cover)
            .save(lib.join(title).join(".cover.jpg"))
            .unwrap();
        index.record(
            title,
            gideon_core::SeriesRef {
                source_id: "s".into(),
                source_name: "src".into(),
                manga_id: "m".into(),
                manga_title: (*title).into(),
                meta: Some(gideon_core::SeriesMeta {
                    score: Some(*score),
                    status: Some((*status).into()),
                    genres: genres.iter().map(|g| (*g).to_string()).collect(),
                    rank: None,
                    total_chapters: Some(*total),
                    fetched_at: None,
                }),
                ..Default::default()
            },
        );
        for c in 0..*read {
            progress.push((format!("{title}/ch{c}.cbz"), 19, 20, now - days * 86_400));
        }
        // Record the files as downloads too, so the storage screen has a
        // breakdown to draw.
        for i in 0..*downloaded {
            index.record_download(title, &format!("c{i}"), &format!("ch{i}.cbz"));
        }
    }
    // A partly-read chapter so Continue has somewhere to point.
    progress.push((
        "Vinland Saga/ch16.cbz".to_string(),
        11,
        44,
        now - 2 * 86_400,
    ));
    // Longer history so the heatmap has range.
    let pattern = [
        0usize, 14, 3, 0, 41, 22, 0, 0, 7, 33, 18, 0, 9, 27, 0, 44, 2, 0, 11, 36,
    ];
    for d in 20..126u64 {
        let pages = pattern[(d as usize * 7 + d as usize / 5) % pattern.len()];
        if pages > 0 {
            progress.push((
                format!("Frieren/h{d}.cbz"),
                pages - 1,
                pages,
                now - d * 86_400,
            ));
        }
    }
    index.save(&lib).unwrap();
    let refs: Vec<(&str, usize, usize, u64)> = progress
        .iter()
        .map(|(k, p, t, a)| (k.as_str(), *p, *t, *a))
        .collect();
    write_progress(&lib, &refs);

    // Extra series so a dump of the library shows what a real one does:
    // more than one page, and therefore the pager strip.
    for i in 0..6 {
        make_cbz(&lib.join(format!("Filler {i:02}/ch0.cbz")), 8);
    }
    let screen = std::env::var("GIDEON_SCREEN").unwrap_or_else(|_| "today".into());
    let settings_dir = dir.path().join("data");
    gideon_core::Settings {
        color_profile: std::env::var("GIDEON_PROFILE").unwrap_or_else(|_| "ink-rust".into()),
        library_view: if screen == "shelf" {
            "shelf".into()
        } else {
            "list".into()
        },
        ..gideon_core::Settings::default()
    }
    .save(&settings_dir)
    .unwrap();

    let mut app = UiApp::new(
        MemoryDisplay::new(1264, 1680),
        FakeInput::new(vec![]),
        FakeGateway::default(),
        lib.clone(),
    )
    .with_settings_dir(settings_dir)
    .with_lights(Box::new(FakeLights {
        levels: std::rc::Rc::new(std::cell::RefCell::new((42, 15))),
    }));

    // Top-level destinations REPLACE the root screen the way the nav bar
    // does; pushing them would make them look like pushed screens (Back
    // instead of the nav bar) and misrepresent the design.
    app.run().unwrap();
    match screen.as_str() {
        "library" | "shelf" => {}
        "today" => app.goto_root(Screen::Stats).unwrap(),
        "discover" => app.goto_root(Screen::Home).unwrap(),
        "settings" => app.goto_root(Screen::Settings).unwrap(),
        "storage" => app.push(Screen::Storage).unwrap(),
        // The quick-settings sheet, layered over the library the way a real
        // nav tap produces it.
        "quick" => {
            app.open_quick_settings().unwrap();
        }
        // The profiles sheet, over Today.
        "profiles" => {
            app.open_profile_menu().unwrap();
        }
        // The long-press book sheet, over the library it was raised from.
        "book" => {
            let l = UiLayout::new(1264, 1680);
            app.handle_long_press(l.width / 2, l.content_top() + l.row_h)
                .unwrap();
        }
        other => panic!("unknown screen {other}"),
    }

    let page = app.compose_final().unwrap();
    let mut img = image::RgbImage::new(page.width, page.height);
    for y in 0..page.height {
        for x in 0..page.width {
            img.put_pixel(x, y, image::Rgb(page.pixel(x, y)));
        }
    }
    let out = std::env::var("GIDEON_DUMP").unwrap_or_else(|_| format!("{screen}.png"));
    img.save(&out).unwrap();
    eprintln!("wrote {out}");
}

#[test]
fn quick_settings_opens_and_cycles_without_flashing() {
    // The panel has a non-flashing partial waveform (GLR16, GLRC16 for
    // colour). Sliding a sheet up changes only the strip it covers, and
    // cycling one tile changes one value — flashing the whole screen for
    // either is what makes an e-ink UI feel cheap. Closing DOES flash,
    // because it restores a region the panel has been holding stale.
    let dir = tempfile::tempdir().unwrap();
    let lib = dir.path().join("Manga");
    make_cbz(&lib.join("Berserk/vol1.cbz"), 3);
    let settings_dir = dir.path().join("data");
    gideon_core::Settings::default()
        .save(&settings_dir)
        .unwrap();

    let mut app = app(&lib, FakeGateway::default(), vec![]).with_settings_dir(settings_dir.clone());
    app.run().unwrap();
    let before = app.display().flushes.len();

    app.open_quick_settings().unwrap();
    assert_eq!(
        app.display().flushes[before..],
        [RefreshMode::Partial],
        "opening the sheet must not flash"
    );

    // The tiles the sheet offers, and where they are: by label, because the
    // contents are a design decision and will move again.
    let tiles = app.quick_tiles();
    assert_eq!(tiles.len(), QUICK_TILES, "the helpers know the tile count");
    let colour = tiles
        .iter()
        .position(|(label, _)| *label == "Colour profile")
        .expect("the sheet offers the colour profile");

    let UiEvent::Tap { x, y } = tap_quick_tile(colour) else {
        unreachable!()
    };
    app.tap_sheet(x, y).unwrap();
    assert_eq!(
        app.display().flushes.last(),
        Some(&RefreshMode::Partial),
        "cycling a tile repaints one value; it must not flash"
    );
    assert_eq!(
        effective_settings(&settings_dir, &lib).color_profile,
        "indigo",
        "the tap should have advanced the colour profile"
    );

    // The gutter between the columns is not a tile: a tap there changes
    // nothing rather than cycling whichever neighbour rounding lands on.
    let (_, grid, _) = quick_sheet();
    let (cx, cy, cw, ch) = grid.cell(0);
    let flushes = app.display().flushes.len();
    app.tap_sheet(cx + cw + 1, cy + ch / 2).unwrap();
    assert_eq!(
        app.display().flushes.len(),
        flushes,
        "a tap in the gutter repaints nothing"
    );

    // A tap above the sheet dismisses it, and that one flashes.
    let (top, ..) = quick_sheet();
    app.tap_sheet(W / 2, top.saturating_sub(10)).unwrap();
    assert!(app.sheet().is_none(), "tapping outside dismisses");
    assert_eq!(
        app.display().flushes.last(),
        Some(&RefreshMode::Full),
        "closing restores what the sheet covered, so it flashes"
    );
}

#[test]
fn every_series_is_reachable_by_paging_in_either_library_view() {
    // The two views hold different counts per page — the shelf packs a cover
    // grid, the list gives each series a double-height row. Paging that asks
    // the wrong one strands everything past the last reachable page, which
    // with the list as the default meant half a library was simply gone.
    let dir = tempfile::tempdir().unwrap();
    let lib = dir.path().join("Manga");
    for i in 0..14 {
        make_cbz(&lib.join(format!("Series {i:02}/vol1.cbz")), 2);
    }
    let settings_dir = dir.path().join("data");

    for view in ["list", "shelf"] {
        gideon_core::ProfileSettings {
            library_view: Some(view.into()),
            ..Default::default()
        }
        .save(&lib)
        .unwrap();
        let mut app =
            app(&lib, FakeGateway::default(), vec![]).with_settings_dir(settings_dir.clone());
        app.run().unwrap();
        // Today is the landing screen; this test is about the library.
        app.open_library().unwrap();

        let per_page = app.library_page_capacity();
        let Screen::Library { items, .. } = app.screen() else {
            panic!("the landing screen is the library");
        };
        let total = items.len();
        assert_eq!(total, 14, "{view}: every series should be listed");

        // Jump to the last page and confirm it really covers the tail.
        let pages = total.div_ceil(per_page);
        app.move_page(PageMove::Last).unwrap();
        let Screen::Library { page, .. } = app.screen() else {
            unreachable!()
        };
        let page = *page;
        assert_eq!(
            page,
            pages - 1,
            "{view}: Last should reach the final page, not stop short"
        );
        assert!(
            (page + 1) * per_page >= total,
            "{view}: the last page must cover the final series ({} of {total} covered)",
            (page + 1) * per_page
        );
    }
}

#[test]
fn every_settings_row_changes_the_setting_its_label_names() {
    // The screen drew from a grouped list while the tap dispatch indexed a
    // different flat one, so every row fired someone else's setting — "Library
    // view" opened the Wi-Fi scanner. Nothing caught it, because the only test
    // exercised the model rather than a tap at the y a row is drawn at.
    // This taps the drawn centre of each row and asserts THAT row's field moved.
    let dir = tempfile::tempdir().unwrap();
    let lib = dir.path().join("Manga");
    make_cbz(&lib.join("Berserk/vol1.cbz"), 2);
    let settings_dir = dir.path().join("data");
    gideon_core::Settings::default()
        .save(&settings_dir)
        .unwrap();

    let mut app = app(&lib, FakeGateway::default(), vec![]).with_settings_dir(settings_dir.clone());
    app.run().unwrap();
    app.goto_root(Screen::Settings).unwrap();

    let mut seen = Vec::new();
    let pages = app.settings_page_count();
    assert!(pages >= 1);
    for page in 0..pages {
        app.set_settings_page(page);
        let map = app.settings_hit_map();
        assert!(!map.is_empty(), "page {page} laid out no rows");
        for (cx, cy, cw, ch, action) in map {
            seen.push(action);
            let before = effective_settings(&settings_dir, &lib);
            app.tap_setting_at(cx + cw / 2, cy + ch / 2).unwrap();
            let after = effective_settings(&settings_dir, &lib);

            // The three navigation rows move the screen instead of a value.
            let changed = match action {
                SettingAction::ReaderFit => before.reader_fit != after.reader_fit,
                SettingAction::FullRefresh => {
                    before.reader_full_refresh_interval != after.reader_full_refresh_interval
                }
                SettingAction::RotateSpreads => {
                    before.auto_rotate_spreads != after.auto_rotate_spreads
                }
                SettingAction::ColorProfile => before.color_profile != after.color_profile,
                SettingAction::LibraryView => before.library_view != after.library_view,
                SettingAction::Predownload => {
                    before.predownload_unread_chapters != after.predownload_unread_chapters
                }
                SettingAction::CleanupHours => {
                    before.finished_cleanup_hours != after.finished_cleanup_hours
                }
                SettingAction::StorageLimit => {
                    before.storage_size_limit != after.storage_size_limit
                }
                SettingAction::IdleSuspend => {
                    before.idle_suspend_minutes != after.idle_suspend_minutes
                }
                SettingAction::WifiAutoConnect => {
                    before.wifi_auto_connect != after.wifi_auto_connect
                }
                SettingAction::ColorBoost => before.color_post_process != after.color_post_process,
                SettingAction::AutoUpdate => before.auto_check_updates != after.auto_check_updates,
                SettingAction::OpenWifi
                | SettingAction::OpenStorage
                | SettingAction::OpenAccount => {
                    // A navigation row leaves every value alone.
                    assert_eq!(before, after, "{action:?} must not change a setting");
                    app.goto_root(Screen::Settings).unwrap();
                    app.set_settings_page(page);
                    continue;
                }
            };
            assert!(changed, "tapping the {action:?} row changed nothing");
        }
    }

    // Paging must not lose a row: every setting the model defines has to be
    // reachable on some page. Dropping the tail off the fold is exactly the
    // bug this screen shipped with.
    for (_, rows) in super::settings_groups(&effective_settings(&settings_dir, &lib)) {
        for (label, _, _, action) in rows {
            assert!(
                seen.contains(&action),
                "{label:?} is defined but on no page"
            );
        }
    }
}

#[test]
fn every_setting_fits_on_one_screen_as_a_grid() {
    // Fifteen settings stacked as rows needed two panels, and the version
    // before that simply drew the last group past the fold — visible
    // nowhere, tappable nowhere. Two columns of tiles fit the lot on one
    // screen, so there is no pager to get lost in.
    let dir = tempfile::tempdir().unwrap();
    let lib = dir.path().join("Manga");
    make_cbz(&lib.join("Berserk/vol1.cbz"), 2);
    let settings_dir = dir.path().join("data");
    gideon_core::Settings::default()
        .save(&settings_dir)
        .unwrap();

    let mut app = app(&lib, FakeGateway::default(), vec![]).with_settings_dir(settings_dir.clone());
    app.run().unwrap();
    app.goto_root(Screen::Settings).unwrap();

    assert_eq!(app.settings_page_count(), 1, "everything fits on one page");
    let map = app.settings_hit_map();
    let defined: usize = super::settings_groups(&effective_settings(&settings_dir, &lib))
        .iter()
        .map(|(_, rows)| rows.len())
        .sum();
    assert_eq!(map.len(), defined, "every setting has a tile to tap");

    // Tiles must not overlap each other or spill past the nav bar — the two
    // ways a grid silently eats a tap.
    let nav_top = layout().nav_top();
    for (i, (ax, ay, aw, ah, _)) in map.iter().enumerate() {
        assert!(
            ay + ah <= nav_top,
            "tile {i} runs under the nav bar ({ay}+{ah} > {nav_top})"
        );
        for (bx, by, bw, bh, _) in map.iter().skip(i + 1) {
            let overlaps = ax < &(bx + bw) && bx < &(ax + aw) && ay < &(by + bh) && by < &(ay + ah);
            assert!(!overlaps, "tiles overlap: {ax},{ay} and {bx},{by}");
        }
    }
}

#[test]
fn the_settings_pager_still_works_when_a_panel_is_too_small_for_the_grid() {
    // The packing stays even though nothing pages today: a smaller panel, a
    // larger font or one more group of settings all overflow it, and the
    // failure mode without paging is a tile drawn past the fold.
    let groups = super::settings_groups(&gideon_core::Settings::default());
    let total: usize = groups.iter().map(|(_, rows)| rows.len()).sum();

    // Room for two tile-rows per page, against three groups of settings.
    let pages = super::paginate_settings(groups, 20, 100, 8, 2, 20 + 100 + 8 + 100);
    assert!(pages.len() > 1, "a short panel has to page");
    let placed: usize = pages
        .iter()
        .flat_map(|p| p.iter().map(|(_, rows)| rows.len()))
        .sum();
    assert_eq!(placed, total, "paging must not drop a setting");
    for page in &pages {
        assert!(!page.is_empty(), "no page is drawn empty");
        for (_, rows) in page {
            assert!(!rows.is_empty(), "a heading never sits alone on a page");
        }
    }
}

#[test]
fn todays_continue_card_opens_the_chapter_it_names() {
    // The card was drawn with a title, a "page N of M" and a progress bar,
    // and tapping it did nothing at all: Today's whole tap dispatch returned
    // early. A card that names your place has to take you there.
    let dir = tempfile::tempdir().unwrap();
    let lib = dir.path().join("Manga");
    make_cbz(&lib.join("Berserk/vol1.cbz"), 6);
    make_cbz(&lib.join("Vinland Saga/vol1.cbz"), 6);
    let now = now_unix();
    // Vinland Saga is the more recent read, so it is what Continue names.
    write_progress(
        &lib,
        &[
            ("Berserk/vol1.cbz", 1, 6, now - 90_000),
            ("Vinland Saga/vol1.cbz", 2, 6, now - 600),
        ],
    );

    let l = layout();
    let mut probe =
        app(&lib, FakeGateway::default(), vec![]).with_settings_dir(dir.path().join("d"));
    probe.run().unwrap();
    probe.goto_root(Screen::Stats).unwrap();

    let (title, _, _) = probe.continue_card().expect("something has been read");
    assert_eq!(title, "Vinland Saga");
    let top = probe.continue_card_top();
    assert!(
        probe.continue_card_hit(top + l.row_h / 2),
        "the drawn card must be inside its own hit box"
    );
    assert!(
        !probe.continue_card_hit(top.saturating_sub(1)),
        "the heatmap above it is not a Continue tap"
    );

    // Tapping it opens the reader on that chapter and reading advances it.
    let events = vec![
        tap_today(),
        UiEvent::Tap {
            x: l.width / 2,
            y: top + l.row_h / 2,
        },
        reader_tap_next(),
        reader_tap_back(),
    ];
    let mut app = app(&lib, FakeGateway::default(), events).with_settings_dir(dir.path().join("d"));
    app.run().unwrap();

    let store = ProgressStore::load(&progress_path(&lib)).unwrap();
    assert_eq!(
        store.get("Vinland Saga/vol1.cbz").unwrap().current_page,
        3,
        "the tap resumed page 3 of Vinland Saga and turned to page 4"
    );
    assert_eq!(
        store.get("Berserk/vol1.cbz").unwrap().current_page,
        1,
        "the series the card did not name is untouched"
    );
}

#[test]
fn a_long_press_opens_a_modal_over_the_book_without_flashing() {
    // Long-press options are a sheet over the library, not a screen of their
    // own: the card you pressed stays visible behind it (which is the point
    // of pressing THAT one), and only the strip it covers repaints.
    let dir = tempfile::tempdir().unwrap();
    let lib = dir.path().join("Manga");
    make_cbz(&lib.join("Berserk/vol1.cbz"), 2);
    make_cbz(&lib.join("Berserk/vol2.cbz"), 2);

    let mut app = app(&lib, FakeGateway::default(), vec![]).with_settings_dir(dir.path().join("d"));
    app.run().unwrap();
    // Today is the landing screen; this test is about the library.
    app.open_library().unwrap();
    let before = app.display().flushes.len();

    let cell = tap_shelf_cell0();
    let UiEvent::Tap { x, y } = cell else {
        unreachable!()
    };
    app.handle_long_press(x, y).unwrap();

    assert!(
        matches!(app.sheet(), Some(Sheet::Book { .. })),
        "the long press opens the book sheet"
    );
    assert!(
        matches!(app.screen(), Screen::Library { .. }),
        "and leaves you on the library, not on a menu screen"
    );
    assert_eq!(
        app.display().flushes[before..],
        [RefreshMode::Partial],
        "raising a sheet must not flash the panel"
    );

    // Delete swaps one sheet for another — still no flash, still no delete.
    let before = app.display().flushes.len();
    app.tap_sheet(W / 2, tap_y(tap_book_row(2))).unwrap();
    assert!(matches!(app.sheet(), Some(Sheet::ConfirmDelete { .. })));
    assert_eq!(app.display().flushes[before..], [RefreshMode::Partial]);
    assert!(lib.join("Berserk/vol1.cbz").exists());

    // Dismissing by tapping above the sheet restores what it covered, which
    // is the one interaction here that legitimately flashes.
    let before = app.display().flushes.len();
    app.tap_sheet(W / 2, 1).unwrap();
    assert!(app.sheet().is_none());
    assert_eq!(app.display().flushes[before..], [RefreshMode::Full]);
    assert!(lib.join("Berserk/vol1.cbz").exists());
}

/// The y of a scripted tap event.
fn tap_y(event: UiEvent) -> u32 {
    match event {
        UiEvent::Tap { y, .. } => y,
        other => panic!("not a tap: {other:?}"),
    }
}

#[test]
fn todays_waiting_list_shows_unread_series_and_opens_them() {
    // Today ended at the Continue card with a third of the panel blank — on a
    // screen whose job is "what should I read next", the one question it
    // declined to answer. The waiting list is what is downloaded and unread,
    // most recently read first, and each row opens its series.
    let dir = tempfile::tempdir().unwrap();
    let lib = dir.path().join("Manga");
    for (series, chapters) in [("Berserk", 3), ("Frieren", 2), ("Punpun", 1)] {
        for c in 1..=chapters {
            make_cbz(&lib.join(format!("{series}/vol{c}.cbz")), 4);
        }
    }
    let now = now_unix();
    write_progress(
        &lib,
        &[
            // Berserk: vol1 finished, vol2 part-read → 2 unread. Read today.
            ("Berserk/vol1.cbz", 3, 4, now - 300),
            ("Berserk/vol2.cbz", 1, 4, now - 200),
            // Frieren: nothing finished → 2 unread, read a week ago.
            ("Frieren/vol1.cbz", 0, 4, now - 7 * 86_400),
            // Punpun: its only chapter finished → nothing waiting.
            ("Punpun/vol1.cbz", 3, 4, now - 86_400),
        ],
    );

    let mut probe =
        app(&lib, FakeGateway::default(), vec![]).with_settings_dir(dir.path().join("d"));
    probe.run().unwrap();
    probe.goto_root(Screen::Stats).unwrap();

    let rows = probe.waiting_rows();
    let listed: Vec<(String, usize)> = rows.iter().map(|(c, n)| (c.title(), *n)).collect();
    assert_eq!(
        listed,
        vec![("Berserk".to_string(), 2), ("Frieren".to_string(), 2)],
        "finished series drop off; the most recently read leads"
    );

    // Each row's drawn y opens that row's series, not its neighbour's.
    let l = layout();
    let head_h = l.row_h * 2 / 3;
    let top = probe.waiting_top() + head_h + l.pad;
    for (i, (card, _)) in rows.iter().enumerate() {
        let y = top + i as u32 * l.row_h + l.row_h / 2;
        assert_eq!(
            probe.waiting_row_at(y).map(|c| c.title()),
            Some(card.title()),
            "row {i} must hit the series drawn there"
        );
    }
    assert!(
        probe.waiting_row_at(top.saturating_sub(l.row_h)).is_none(),
        "the header above the list is not a row"
    );

    // And a tap on the second row reads it.
    let events = vec![
        tap_today(),
        UiEvent::Tap {
            x: l.width / 2,
            y: top + l.row_h + l.row_h / 2,
        },
        reader_tap_next(),
        reader_tap_back(),
    ];
    let mut app = app(&lib, FakeGateway::default(), events).with_settings_dir(dir.path().join("d"));
    app.run().unwrap();
    let store = ProgressStore::load(&progress_path(&lib)).unwrap();
    assert_eq!(
        store.get("Frieren/vol1.cbz").unwrap().current_page,
        1,
        "tapping the Frieren row resumed and turned a page in Frieren"
    );
}

#[test]
fn storage_breaks_down_by_series_and_reads_sizes_in_human_units() {
    // The screen used to be four sentences: "Used: 0 MB of 2048 MB" and a
    // promise that auto-cleanup takes the least-recently-read first. Neither
    // told you what was actually taking the space.
    let dir = tempfile::tempdir().unwrap();
    let lib = dir.path().join("Manga");
    let mut index = gideon_core::SeriesIndex::default();
    for (series, chapters, pages) in [("Big Series", 3, 12), ("Small Series", 1, 2)] {
        index.record(
            series,
            gideon_core::SeriesRef {
                source_id: "s".into(),
                source_name: "src".into(),
                manga_id: series.into(),
                manga_title: series.into(),
                ..Default::default()
            },
        );
        for c in 0..chapters {
            let file = format!("ch{c}.cbz");
            make_cbz(&lib.join(series).join(&file), pages);
            index.record_download(series, &format!("c{c}"), &file);
        }
    }
    index.save(&lib).unwrap();

    let mut app = app(&lib, FakeGateway::default(), vec![]).with_settings_dir(dir.path().join("d"));
    app.run().unwrap();

    let rows = app.storage_by_series();
    let names: Vec<&str> = rows.iter().map(|(t, ..)| t.as_str()).collect();
    assert_eq!(names, vec!["Big Series", "Small Series"], "biggest first");
    assert_eq!(rows[0].2, 3, "chapter counts come from the download index");
    assert!(rows[0].1 > rows[1].1);
    assert_eq!(
        rows.iter().map(|(_, b, _)| b).sum::<u64>(),
        app.storage_stats().used,
        "the breakdown accounts for every byte the total claims"
    );

    // `StorageSize`'s Display floors to whole MB because it has to round-trip
    // through the settings parser, which renders a 700 KB chapter as "0 MB".
    assert_eq!(super::human_size(700 * 1024), "700 KB");
    assert_eq!(super::human_size(3 * 1024 * 1024 + 512 * 1024), "3.5 MB");
    assert_eq!(super::human_size(2 * 1024 * 1024 * 1024), "2.00 GB");
}

#[test]
fn discover_cards_are_a_grid_and_each_one_opens_its_own_destination() {
    // Four rows of a list left two thirds of the panel white. Two columns of
    // cards fill it — but a grid resolves taps by x AND y, and the old row
    // dispatch would have sent every card in the right-hand column to its
    // left-hand neighbour.
    let dir = tempfile::tempdir().unwrap();
    let mut probe = app(dir.path(), source_gateway(), vec![]);
    probe.run().unwrap();
    probe.goto_root(Screen::Home).unwrap();

    let cards = probe.discover_cards();
    assert_eq!(cards.len(), 4, "online, the four destinations");
    let grid = probe.discover_grid(cards.len());
    assert!(grid.cell(1).0 > grid.cell(0).0, "two columns");
    assert!(grid.cell(2).1 > grid.cell(0).1, "two rows");

    // The gutter between the columns is not a card: a tap there does nothing
    // rather than activating whichever neighbour rounding lands on.
    let l = layout();
    let (cx, cy, cw, ch) = grid.cell(0);
    assert_eq!(grid.hit(cx + cw / 2, cy + ch / 2, cards.len()), Some(0));
    assert_eq!(grid.hit(cx + cw + 1, cy + ch / 2, cards.len()), None);
    assert!(cy + ch <= l.nav_top(), "cards stay clear of the nav bar");

    // Each card lands on its own screen.
    for (i, want) in [(0usize, "search"), (1, "sources"), (2, "popular")] {
        let mut card_app = app(
            dir.path(),
            source_gateway(),
            vec![nav_discover(), tap_card(i)],
        );
        card_app.run().unwrap();
        let landed = match card_app.screen() {
            Screen::Search { .. } | Screen::RecentSearches { .. } => "search",
            Screen::Sources { .. } => "sources",
            Screen::MangaList { .. } | Screen::Message { .. } => "popular",
            other => panic!("card {i} landed on {other:?}"),
        };
        assert_eq!(landed, want, "card {i}");
    }
}

#[test]
fn discovers_installed_sources_open_their_listings() {
    // The strip under the cards is the one part of Discover that still says
    // something with the radio off. Listing what you have installed and then
    // ignoring a tap on it is the kind of dead text that teaches people not
    // to try, so each line opens that source.
    let dir = tempfile::tempdir().unwrap();
    let mut app = app(dir.path(), source_gateway(), vec![]);
    app.run().unwrap();
    app.goto_root(Screen::Home).unwrap();

    let sources = app.gateway().installed_sources().unwrap();
    assert!(!sources.is_empty(), "the fixture installs a source");

    let l = layout();
    let head_h = l.row_h * 2 / 3;
    let top = app.discover_sources_top() + head_h + l.pad / 2;
    let line_h = app.discover_source_line_h();

    // The header above the strip is not a row.
    assert!(app.discover_source_at(top.saturating_sub(1)).is_none());
    for (i, source) in sources.iter().enumerate() {
        let y = top + i as u32 * line_h + line_h / 2;
        assert_eq!(
            app.discover_source_at(y).map(|s| s.id.clone()),
            Some(source.id.clone()),
            "line {i} must hit the source drawn there"
        );
    }

    // And a tap actually goes there, without disturbing the cards above it.
    let y = top + line_h / 2;
    app.handle_tap(l.width / 2, y).unwrap();
    assert!(
        matches!(app.screen(), Screen::Listings { source } if source.id == sources[0].id),
        "the tap opened the source's listings"
    );
}

#[test]
fn a_paginated_library_does_not_draw_paging_buttons_under_the_nav_bar() {
    // Shipped in 1.0 and caught on hardware: with more than one page of
    // series, the Library drew First/Prev/Next/Last into the bottom strip
    // AND the four nav tabs on top of them, so the bar read
    // "Library First TodayPrev DisCoveNr Sesttings".
    let dir = tempfile::tempdir().unwrap();
    let lib = dir.path().join("Manga");
    for view in ["list", "shelf"] {
        let profile_lib = lib.join(view);
        for i in 0..12 {
            make_cbz(&profile_lib.join(format!("Series {i:02}/vol1.cbz")), 1);
        }
        gideon_core::ProfileSettings {
            library_view: Some(view.into()),
            ..Default::default()
        }
        .save(&profile_lib)
        .unwrap();

        let mut paged = app(&profile_lib, FakeGateway::default(), vec![]);
        paged.run().unwrap();
        // Today is the landing screen; this test is about the library.
        paged.open_library().unwrap();
        assert!(paged.current_page_count() > 1, "{view}: needs to paginate");

        let page = paged.compose_final().unwrap();
        let l = layout();
        let strip = l.nav_top() + l.nav_h / 2;
        // The nav bar draws four labels; the paging buttons would add ink in
        // the gaps between them. Compare against the same screen on a
        // single page, where only the tabs are drawn.
        let one = dir.path().join(format!("{view}-one"));
        make_cbz(&one.join("Only/vol1.cbz"), 1);
        gideon_core::ProfileSettings {
            library_view: Some(view.into()),
            ..Default::default()
        }
        .save(&one)
        .unwrap();
        let mut single = app(&one, FakeGateway::default(), vec![]);
        single.run().unwrap();
        single.open_library().unwrap();
        assert_eq!(single.current_page_count(), 1);
        let plain = single.compose_final().unwrap();

        let ink = |p: &gideon_render::RgbPage| -> usize {
            (0..l.width)
                .filter(|&x| p.pixel(x, strip) != [0xFF; 3])
                .count()
        };
        assert_eq!(
            ink(&page),
            ink(&plain),
            "{view}: the paged nav strip must hold the tabs and nothing else"
        );
    }
}

#[test]
fn quick_settings_sliders_set_the_lamp_where_you_tap() {
    // Reaching for the light is the most common reason to open this sheet
    // mid-chapter. A tap on the track sets the level to where you tapped —
    // stepping across it a level at a time would be worse than the edge
    // slide that already exists.
    let dir = tempfile::tempdir().unwrap();
    let lib = dir.path().join("Manga");
    make_cbz(&lib.join("Berserk/vol1.cbz"), 2);
    let (levels, lights) = lights();
    let mut app = app(&lib, FakeGateway::default(), vec![])
        .with_lights(lights)
        .with_settings_dir(dir.path().join("d"));
    app.run().unwrap();
    app.open_quick_settings().unwrap();

    let sliders = app.quick_sliders();
    assert_eq!(
        sliders.iter().map(|(l, _)| *l).collect::<Vec<_>>(),
        vec!["Brightness", "Night warmth"],
        "a device with a lamp offers both"
    );

    let l = layout();
    let sheet_top = app.sheet_bounds().expect("a sheet is open").0;
    let title_h = (l.text_px * 1.6) as u32;
    let slider_row = |i: u32| {
        sheet_top
            + title_h
            + super::SETTINGS_GAP
            + i * (l.row_h + super::SETTINGS_GAP)
            + l.row_h / 2
    };

    // Three quarters along the brightness track.
    let before = app.display().flushes.len();
    app.tap_sheet(l.pad + (l.width - l.pad * 2) * 3 / 4, slider_row(0))
        .unwrap();
    assert_eq!(levels.borrow().0, 75, "brightness follows the tap");
    assert_eq!(
        app.display().flushes[before..],
        [RefreshMode::Partial],
        "one slider's fill changed; it must not flash"
    );

    // And the warmth row is its own control, not the same one twice.
    app.tap_sheet(l.pad + (l.width - l.pad * 2) / 4, slider_row(1))
        .unwrap();
    assert_eq!(levels.borrow().1, 25, "warmth follows its own tap");
    assert_eq!(levels.borrow().0, 75, "and leaves brightness alone");
}

#[test]
fn a_device_without_a_lamp_gets_no_sliders() {
    // Drawing a control that cannot do anything is worse than not offering
    // it: the desktop build and the test harness have no frontlight.
    let dir = tempfile::tempdir().unwrap();
    let lib = dir.path().join("Manga");
    make_cbz(&lib.join("Berserk/vol1.cbz"), 2);
    let mut app = app(&lib, FakeGateway::default(), vec![]);
    app.run().unwrap();
    app.open_quick_settings().unwrap();
    assert!(app.quick_sliders().is_empty());
}
