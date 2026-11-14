//! Browse-UI state machine tests: MemoryDisplay + FakeInput + FakeGateway,
//! no network and no WASM runtime.

use std::cell::RefCell;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Result};

use gideon_core::ProgressStore;
use gideon_device::{FakeInput, MemoryDisplay, RefreshMode, UiEvent};

use super::gateway::{BookEntry, BookFeed, BookOpened, SearchLink};
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
    /// OPDS catalogues the Books card shows.
    catalogs: Vec<gideon_core::BookCatalog>,
    /// Canned HTTP bodies the book methods run the REAL OPDS client over —
    /// feeds, OpenSearch descriptions and EPUB bytes by exact URL — so a UI
    /// test exercises parse → open → download end to end, offline.
    web: gideon_sources::FakeFetcher,
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
            catalogs: Vec::new(),
            web: gideon_sources::FakeFetcher::new(),
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

    fn book_catalogs(&self) -> Vec<gideon_core::BookCatalog> {
        self.catalogs.clone()
    }

    fn book_feed(&self, url: &url::Url) -> Result<BookFeed> {
        Ok(gideon_sources::catalog::fetch_feed(&self.web, url)?)
    }

    fn book_next_page(&self, feed: &BookFeed) -> Result<Option<BookFeed>> {
        Ok(gideon_sources::catalog::fetch_next(&self.web, feed)?)
    }

    fn book_search(&self, search: &SearchLink, query: &str) -> Result<Option<BookFeed>> {
        Ok(gideon_sources::catalog::search(&self.web, search, query)?)
    }

    fn book_open(&self, entry: &BookEntry) -> Result<BookOpened> {
        Ok(gideon_sources::catalog::open_entry(&self.web, entry)?)
    }

    fn book_download(
        &self,
        epub: &url::Url,
        books_dir: &Path,
        author: Option<&str>,
        title: &str,
    ) -> Result<(PathBuf, bool)> {
        Ok(gideon_sources::opds::download_epub(
            &self.web, epub, books_dir, author, title,
        )?)
    }

    fn fetch(&self, url: &url::Url) -> Result<Vec<u8>> {
        Ok(gideon_sources::Fetcher::get(&self.web, url)?)
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
    // Beside the library would be the tempdir's parent — /tmp — so books go
    // under the test's own directory instead.
    .with_books_dir(library.join("Books"))
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
    UiEvent::Tap {
        x: W / 2,
        y: tap_modal_row_y(rows, i),
    }
}

/// The y of row `i` of a `rows`-row sheet.
fn tap_modal_row_y(rows: u32, i: usize) -> u32 {
    let l = layout();
    let top = H - (rows + 1) * l.row_h;
    top + l.row_h + i as u32 * l.row_h + l.row_h / 2
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
/// under them ("All settings" — the way out is the × in the title bar).
const QUICK_TILES: usize = 6;
const QUICK_ACTIONS: usize = 1;

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
    tap_card_at(W, H, i)
}

/// [`tap_card`] on a panel of any size.
fn tap_card_at(w: u32, h: u32, i: usize) -> UiEvent {
    let l = UiLayout::new(w, h);
    let gap = l.pad / 2;
    let top = l.content_top() + gap;
    let avail = l.nav_top().saturating_sub(top + gap);
    // Mirrors `discover_grid`: cards are laid out for the count on screen —
    // HOME_ROWS, plus one when the offline reconnect card is showing (which
    // lands in the same row count).
    let rows = (HOME_ROWS.len() as u32).div_ceil(2);
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

/// Tap the title bar's pager: "‹ n/m ›" at its right end. `pages` sizes the
/// label, which is what the zones are measured from.
fn tap_page(forward: bool, pages: usize) -> UiEvent {
    let l = layout();
    let (prev, next, _) = super::title_pager_zones(&l, 0, pages, 0);
    let zone = if forward { next } else { prev };
    UiEvent::Tap {
        x: (zone.0 + zone.1) / 2,
        y: l.title_h / 2,
    }
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

    let events = vec![
        tap_nav(0),
        tap_page(true, 2),
        tap_page(true, 2), // clamped at the last page
        tap_page(false, 2),
    ];
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

    let events = vec![tap_nav(0), tap_page(true, 2)];
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
    let mut app = app(dir.path(), gateway, vec![nav_discover(), tap_card(4)]);
    app.run().unwrap();

    let Screen::Message { title, body } = app.screen() else {
        panic!("expected updates message screen");
    };
    assert_eq!(title, "Updates");
    assert!(body.contains("up to date"));
}

#[test]
fn a_root_screen_has_no_back_and_cannot_be_popped_out_of() {
    // The bottom strip of a top-level destination is the nav bar, so the
    // place Back used to be is now the Library tab. Nothing can pop the root
    // out from under the app — quitting goes through the power menu — and
    // the taps after it still work.
    let dir = tempfile::tempdir().unwrap();
    let mut rooted = app(
        dir.path(),
        FakeGateway::default(),
        vec![nav_discover(), tap_back(), tap_back()],
    );
    rooted.run().unwrap();
    assert!(
        matches!(rooted.screen(), Screen::Library { .. }),
        "the bottom-left of a root screen is the Library tab"
    );

    // And the tap after them still works: Discover, then its first card.
    let mut again = app(
        dir.path(),
        FakeGateway::default(),
        vec![
            nav_discover(),
            tap_back(),
            tap_back(),
            nav_discover(),
            tap_card(0),
        ],
    );
    again.run().unwrap();
    assert!(
        matches!(again.screen(), Screen::Message { title, .. } if title == "Search"),
        "the tap after them still works"
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
    let _ = wait_for(|| {
        app.downloaded_chapter_path(&source, &manga, "c2").is_some()
            && app.downloaded_chapter_path(&source, &manga, "c3").is_some()
    });

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
    assert_eq!(started_rx.recv_timeout(WORKER_WAIT).unwrap(), "c2");
    app.pop().unwrap(); // pops the chapter list → cancels the queued rest

    // c2 (already in flight) finishes; c3/c4 are dropped. Wait for c2 to land,
    // then give the worker ample time to (not) fetch the cancelled ones.
    let _ = wait_for(|| app.downloaded_chapter_path(&source, &manga, "c2").is_some());
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

    let _ = wait_for(|| app.downloaded_chapter_path(&source, &manga, "c1").is_some());

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
    started_rx.recv_timeout(WORKER_WAIT).unwrap();
    app.pop().unwrap(); // leave the confirmation
    app.pop().unwrap(); // leave the chapter list → would cancel a look-ahead

    // All five queued chapters still land; the sixth was never requested.
    let _ = wait_for(|| {
        (1..=5).all(|i| {
            app.downloaded_chapter_path(&source, &manga, &format!("c{i}"))
                .is_some()
        })
    });
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

/// How long a test may wait for the background download worker.
///
/// The worker runs at IDLE cpu/io priority on purpose
/// (`lower_current_thread_to_idle`, so pre-fetching never makes the reader
/// stutter). On a CI box running the whole suite in parallel that thread can be
/// starved for seconds at a stretch — which is exactly what a few seconds of
/// headroom here buys back. This is a synchronisation ceiling, not a
/// performance assertion: [`wait_for`] returns the instant its condition holds,
/// so a generous ceiling costs a fast machine nothing and costs a loaded one a
/// green run.
const WORKER_WAIT: std::time::Duration = std::time::Duration::from_secs(60);

/// Poll until `done`, up to [`WORKER_WAIT`]. Returns whether it happened.
///
/// `#[must_use]` on purpose: a wait that quietly falls through on timeout turns
/// every assertion after it into a statement about something else. The tests
/// that wait for a *failed* attempt are the ones this bites — they then assert
/// "nothing is on disk yet", which a worker that never ran also satisfies.
#[must_use]
fn wait_for(mut done: impl FnMut() -> bool) -> bool {
    let deadline = std::time::Instant::now() + WORKER_WAIT;
    while std::time::Instant::now() < deadline {
        if done() {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    done()
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
    assert!(
        wait_for(|| started.load(std::sync::atomic::Ordering::SeqCst) >= 3),
        "the worker never attempted all three, so what follows would be \
         measuring the wait rather than the retry"
    );
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

    let _ = wait_for(|| {
        (1..=3).all(|i| {
            app.downloaded_chapter_path(&source, &manga, &format!("c{i}"))
                .is_some()
        })
    });
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

/// Scriptable light control recording every set — the final levels alone
/// cannot tell a control that followed a finger from one that jumped at the
/// end, and that difference is the whole point of a scrub.
#[derive(Default)]
struct FakeLights {
    levels: SharedLevels,
    log: SharedLog,
}

impl LightControl for FakeLights {
    fn brightness(&self) -> u8 {
        self.levels.borrow().0
    }
    fn set_brightness(&mut self, p: u8) {
        self.levels.borrow_mut().0 = p;
        self.log.borrow_mut().push(Lamp::Brightness(p));
    }
    fn warmth(&self) -> u8 {
        self.levels.borrow().1
    }
    fn set_warmth(&mut self, p: u8) {
        self.levels.borrow_mut().1 = p;
        self.log.borrow_mut().push(Lamp::Warmth(p));
    }
}

/// One recorded change, so a test can read the shape of a scrub.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Lamp {
    Brightness(u8),
    Warmth(u8),
}

type SharedLevels = std::rc::Rc<RefCell<(u8, u8)>>;
type SharedLog = std::rc::Rc<RefCell<Vec<Lamp>>>;

fn lights() -> (SharedLevels, Box<dyn LightControl>) {
    let (levels, _, control) = lights_recording();
    (levels, control)
}

fn lights_recording() -> (SharedLevels, SharedLog, Box<dyn LightControl>) {
    let levels = std::rc::Rc::new(RefCell::new((20u8, 0u8)));
    let log = SharedLog::default();
    (
        levels.clone(),
        log.clone(),
        Box::new(FakeLights { levels, log }) as Box<dyn LightControl>,
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
fn the_reader_lamp_follows_the_finger_and_the_release_does_not_double_it() {
    // You are adjusting a light you are looking at: waiting for the lift means
    // aiming in the dark and correcting afterwards. So the in-flight reports
    // move the lamp...
    //
    // ...and the trap they open is the release. Every report of one drag
    // carries the SAME start point, so the delta is the same each time; adding
    // it to the running level would take the lamp to 20 + 50 + 50 + 50. Both
    // the scrub and the landing apply it to the level the drag began at.
    let dir = tempfile::tempdir().unwrap();
    let lib = dir.path().join("Manga");
    make_cbz(&lib.join("Sample/vol1.cbz"), 3);
    let (levels, log, lights) = lights_recording();

    let drag_to = |y1: u32| UiEvent::Drag {
        x0: W - 5,
        y0: H - 100,
        x1: W - 5,
        y1,
    };
    // Up the right edge in three reports, then lift at the same place.
    let quarter = drag_to(H - 100 - H / 4);
    let half = drag_to(H - 100 - H / 2);
    let release = UiEvent::Swipe {
        x0: W - 5,
        y0: H - 100,
        x1: W - 5,
        y1: H - 100 - H / 2,
    };
    let events = vec![
        tap_nav(0),
        tap_shelf_cell0(),
        quarter,
        half,
        release,
        reader_tap_back(),
    ];
    let mut app = app(&lib, FakeGateway::default(), events).with_lights(lights);
    app.run().unwrap();

    // The two in-flight reports moved it on the way — a quarter of the screen
    // is +25, half is +50 — and the release named the level it was already
    // showing, so it changed nothing further.
    assert_eq!(
        *log.borrow(),
        vec![Lamp::Brightness(45), Lamp::Brightness(70)],
        "the lamp did not follow the finger"
    );
    assert_eq!(
        levels.borrow().0,
        70,
        "20 + 50 — the release must land on the drag, not on top of it"
    );
    assert_eq!(levels.borrow().1, 0, "warmth untouched");
}

#[test]
fn a_drag_across_the_middle_of_a_page_decides_nothing_until_it_is_released() {
    // Leaving the manga, rotating it and raising the controls sheet are
    // DECISIONS. Only the edges scrub; a decision must never fire halfway
    // through the gesture that makes it, or a swipe that is still being aimed
    // throws you out of the chapter.
    let dir = tempfile::tempdir().unwrap();
    let lib = dir.path().join("Manga");
    make_cbz(&lib.join("Sample/vol1.cbz"), 3);

    // A downward drag across the middle, reported in flight and never released.
    let mid_drag = UiEvent::Drag {
        x0: W / 2,
        y0: H / 4,
        x1: W / 2,
        y1: H - 10,
    };
    let events = vec![
        tap_nav(0),
        tap_shelf_cell0(),
        mid_drag,
        mid_drag,
        reader_tap_back(),
    ];
    let mut app = app(&lib, FakeGateway::default(), events);
    // If the in-flight reports had been taken as the exit gesture, the reader
    // would have quit early and `reader_tap_back` would have landed on the
    // library instead — leaving the run with events unconsumed.
    app.run().unwrap();
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
    // Home card 5 = "Check for updates" -> prompt; content tap installs,
    // and a successful install restarts the app in place so the new
    // binary is live immediately.
    let mut app = app(
        dir.path(),
        gateway,
        vec![nav_discover(), tap_card(4), tap_row(0)],
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
        .map(|(id, title)| gideon_sync::supabase::SendItem::title(id, title))
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
        vec![nav_discover(), tap_card(4), tap_back()],
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
    assert!(
        wait_for(|| started.load(std::sync::atomic::Ordering::SeqCst) >= 1),
        "the look-ahead never attempted c2, so the re-kick below would be \
         testing a first attempt rather than a retry"
    );
    assert!(
        app.downloaded_chapter_path(&source, &manga, "c2").is_none(),
        "the first attempt failed, so nothing is on disk yet"
    );

    // Waking re-fires the same look-ahead — and this time it lands.
    app.rekick_lookahead();
    let _ = wait_for(|| app.downloaded_chapter_path(&source, &manga, "c2").is_some());
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
fn both_library_views_take_the_colour_path_as_a_destination() {
    // The list carries score chips and a progress bar in the theme colour;
    // the shelf carries covers. Either way, as a top-level destination the
    // Library draws the nav bar, which is RGB — a screen that fell back to
    // the grayscale path would lose every route off itself.
    let dir = tempfile::tempdir().unwrap();
    let lib = dir.path().join("Manga");
    make_cbz(&lib.join("Berserk/vol1.cbz"), 3);
    let settings_dir = dir.path().join("data");

    for view in ["shelf", "list"] {
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
        assert!(
            app.compose_color_current().unwrap().is_some(),
            "{view} view must compose in colour so it can draw the nav bar"
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
    // Consecutive-day runs, which is what the calendar exists to show: four
    // evenings of one series, three of another overlapping it, and a couple
    // of single days.
    for (title, start, run) in [
        ("Frieren", 2u64, 4u64),
        ("Chainsaw Man", 4, 3),
        ("Pluto", 9, 2),
    ] {
        for d in 0..run {
            progress.push((
                format!("{title}/cal{d}.cbz"),
                19,
                20,
                now - (start + d) * 86_400,
            ));
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
        log: SharedLog::default(),
    }));

    // Top-level destinations REPLACE the root screen the way the nav bar
    // does; pushing them would make them look like pushed screens (Back
    // instead of the nav bar) and misrepresent the design.
    app.run().unwrap();
    match screen.as_str() {
        // Today is the landing screen now, so a library dump has to walk
        // there the way a person does.
        "library" | "shelf" => app.open_library().unwrap(),
        "today" => app.goto_root(Screen::Stats).unwrap(),
        // Today with the month calendar instead of the heatmap.
        "calendar" => {
            let mut settings = app.load_settings();
            settings.stats_view = "calendar".into();
            app.save_settings(&settings);
            app.goto_root(Screen::Stats).unwrap();
        }
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
                SettingAction::StatsView => before.stats_view != after.stats_view,
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
    let listed: Vec<(String, usize)> = rows
        .iter()
        .map(|r| match r {
            WaitingRow::Manga(c, n) => (c.title(), *n),
            WaitingRow::Book(b) => (b.title.clone(), 0),
        })
        .collect();
    assert_eq!(
        listed,
        vec![("Berserk".to_string(), 2), ("Frieren".to_string(), 2)],
        "finished series drop off; the most recently read leads"
    );

    // Each row's drawn y opens that row's series, not its neighbour's.
    let l = layout();
    let head_h = l.row_h * 2 / 3;
    let top = probe.waiting_top() + head_h + l.pad;
    for (i, row) in rows.iter().enumerate() {
        let y = top + i as u32 * l.row_h + l.row_h / 2;
        assert_eq!(
            probe.waiting_row_at(y).map(|c| c.title()),
            Some(row.title()),
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
    assert_eq!(cards.len(), 5, "online, the five destinations");
    let grid = probe.discover_grid(cards.len());
    assert!(grid.cell(1).0 > grid.cell(0).0, "two columns");
    assert!(grid.cell(2).1 > grid.cell(0).1, "stacked rows");
    assert!(
        grid.cell(4).1 > grid.cell(2).1,
        "the odd fifth card gets its own row"
    );

    // The gutter between the columns is not a card: a tap there does nothing
    // rather than activating whichever neighbour rounding lands on.
    let l = layout();
    let (cx, cy, cw, ch) = grid.cell(0);
    assert_eq!(grid.hit(cx + cw / 2, cy + ch / 2, cards.len()), Some(0));
    assert_eq!(grid.hit(cx + cw + 1, cy + ch / 2, cards.len()), None);
    assert!(cy + ch <= l.nav_top(), "cards stay clear of the nav bar");

    // Each card lands on its own screen. With no catalogues and no update
    // the last two land on explanatory messages, told apart by title.
    for (i, want) in [
        (0usize, "search"),
        (1, "sources"),
        (2, "popular"),
        (3, "books"),
        (4, "updates"),
    ] {
        let mut card_app = app(
            dir.path(),
            source_gateway(),
            vec![nav_discover(), tap_card(i)],
        );
        card_app.run().unwrap();
        let landed = match card_app.screen() {
            Screen::Search { .. } | Screen::RecentSearches { .. } => "search",
            Screen::Sources { .. } => "sources",
            Screen::MangaList { .. } => "popular",
            Screen::Message { title, .. } if title == "Books" => "books",
            Screen::Message { title, .. } if title == "Updates" => "updates",
            Screen::Message { .. } => "popular",
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
fn every_top_level_destination_owns_its_nav_strip() {
    // The invariant three separate bugs violated, each found on hardware or
    // in a screenshot rather than here:
    //
    //   * a paginated Library drew First/Prev/Next/Last INTO the nav strip,
    //     under the tabs, so the bar read "Library First TodayPrev
    //     DisCoveNr Sesttings";
    //   * the Library tab pushed instead of replacing the root, so it came
    //     up with a Back button and no nav bar at all;
    //   * Discover composed in grayscale, so its nav bar was not drawn.
    //
    // All three are the same statement: on a top-level destination, the
    // bottom strip belongs to the nav bar and to nothing else. Checked by
    // rendering the bar on its own and requiring the screen's strip to match
    // it pixel for pixel — which no amount of "looks fine" can fake.
    let dir = tempfile::tempdir().unwrap();
    let lib = dir.path().join("Manga");
    // Enough series to paginate in either view, so the paging chrome is in
    // play on every pass.
    for i in 0..12 {
        make_cbz(&lib.join(format!("Series {i:02}/vol1.cbz")), 2);
        make_red_cover(&lib.join(format!("Series {i:02}")));
    }
    write_progress(&lib, &[("Series 00/vol1.cbz", 0, 2, now_unix())]);
    let settings_dir = dir.path().join("data");

    for view in ["list", "shelf"] {
        gideon_core::ProfileSettings {
            library_view: Some(view.into()),
            ..Default::default()
        }
        .save(&lib)
        .unwrap();

        for zone in 0..4u32 {
            let mut app = app(&lib, FakeGateway::default(), vec![tap_nav(zone)])
                .with_settings_dir(settings_dir.clone());
            app.run().unwrap();
            // The Settings tab raises the quick sheet; this is about the
            // screen under it, so dismiss it the way a tap outside would.
            if app.sheet().is_some() {
                app.tap_sheet(1, 1).unwrap();
            }
            let what = format!("{view} view, tab {zone}, {:?}", app.screen());

            // A destination replaces the root rather than stacking on it.
            assert_eq!(app.stack_depth(), 1, "{what}: not a root screen");

            let page = app.compose_final().unwrap();
            let l = layout();

            // The strip, rendered from nothing but the nav bar.
            let mut reference = gideon_render::RgbPage::new_white(l.width, l.height);
            gideon_render::widgets::draw_nav_bar(
                &mut reference,
                0,
                l.nav_top(),
                l.width,
                l.nav_h,
                &app.nav_items(),
                l.text_px,
                &gideon_render::widgets::Theme::from_setting(
                    &effective_settings(&settings_dir, &lib).color_profile,
                ),
            );

            let mut drawn = 0usize;
            for y in l.nav_top()..l.height {
                for x in 0..l.width {
                    assert_eq!(
                        page.pixel(x, y),
                        reference.pixel(x, y),
                        "{what}: the nav strip has something else in it at {x},{y}"
                    );
                    if reference.pixel(x, y) != [0xFF; 3] {
                        drawn += 1;
                    }
                }
            }
            assert!(drawn > 0, "{what}: the nav bar was never drawn");
        }
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
fn a_slider_scrubs_from_wherever_the_finger_lands_and_goes_both_ways() {
    // The track has no knob to catch: put a finger down anywhere on it and
    // move, and the level follows the finger — up OR back down — live, before
    // the finger lifts. A control you can only tap is not a slider.
    let dir = tempfile::tempdir().unwrap();
    let lib = dir.path().join("Manga");
    make_cbz(&lib.join("Berserk/vol1.cbz"), 2);
    let (levels, lights) = lights();
    let mut app = app(&lib, FakeGateway::default(), vec![])
        .with_lights(lights)
        .with_settings_dir(dir.path().join("d"));
    app.run().unwrap();
    app.open_quick_settings().unwrap();

    let l = layout();
    let sheet_top = app.sheet_bounds().expect("a sheet is open").0;
    let title_h = (l.text_px * 1.6) as u32;
    let track = l.width - l.pad * 2;
    let slider_row = |i: u32| {
        sheet_top
            + title_h
            + super::SETTINGS_GAP
            + i * (l.row_h + super::SETTINGS_GAP)
            + l.row_h / 2
    };
    let at = |fraction: u32| l.pad + track * fraction / 100;

    // Land at a tenth of the way along and drag up to nine tenths, reporting
    // in flight. Every report moves the lamp; nothing waits for the lift.
    let start = (at(10), slider_row(0));
    for stop in [30, 60, 90] {
        app.drag_menu(start.0, start.1, at(stop), start.1, false)
            .unwrap();
        assert_eq!(
            levels.borrow().0,
            stop as u8,
            "the lamp follows the finger mid-drag"
        );
    }

    // Now scrub back the other way without lifting — the same drag, reversing.
    let before = app.display().flushes.len();
    app.drag_menu(start.0, start.1, at(40), start.1, false)
        .unwrap();
    assert_eq!(levels.borrow().0, 40, "dragging back down lowers it");
    assert_eq!(
        app.display().flushes[before..],
        [RefreshMode::Partial],
        "one slider's fill changed; scrubbing must never flash"
    );

    // A report that names the level it is already showing costs nothing: the
    // panel reports motion far faster than e-ink can repaint, and a scrub that
    // spent a refresh per report would lag behind the finger.
    let before = app.display().flushes.len();
    app.drag_menu(start.0, start.1, at(40), start.1, false)
        .unwrap();
    assert_eq!(app.display().flushes[before..], [], "no change, no repaint");

    // Vertical drift off the row keeps scrubbing the slider it started on —
    // it must not hand the drag to the row below.
    app.drag_menu(start.0, start.1, at(80), slider_row(1), false)
        .unwrap();
    assert_eq!(levels.borrow().0, 80, "brightness kept the drag");
    assert_eq!(levels.borrow().1, 0, "warmth was never touched");
}

#[test]
fn a_drag_that_starts_off_the_sliders_leaves_the_sheet_alone() {
    // Dismissal is a TAP outside the sheet. A drag that begins on a tile, on
    // the title, or off the sheet entirely must not set a level and must not
    // close anything — otherwise a swipe aimed past the sheet moves the lamp
    // on its way out.
    let dir = tempfile::tempdir().unwrap();
    let lib = dir.path().join("Manga");
    make_cbz(&lib.join("Berserk/vol1.cbz"), 2);
    let (levels, lights) = lights();
    let mut app = app(&lib, FakeGateway::default(), vec![])
        .with_lights(lights)
        .with_settings_dir(dir.path().join("d"));
    app.run().unwrap();
    app.open_quick_settings().unwrap();

    let l = layout();
    let sheet = app.quick_sheet_layout();
    let before = app.display().flushes.len();
    let untouched = *levels.borrow();
    for (y0, what) in [
        (sheet.top / 2, "above the sheet"),
        (sheet.top + 2, "on the sheet's title"),
        (sheet.grid.y + sheet.grid.cell_h / 2, "on a tile"),
        (sheet.actions_top + l.row_h / 2, "on an action row"),
    ] {
        app.drag_menu(l.pad, y0, l.width - l.pad, y0, false)
            .unwrap();
        assert_eq!(*levels.borrow(), untouched, "the lamp moved from {what}");
        assert!(app.sheet().is_some(), "the sheet closed from {what}");
    }
    assert_eq!(
        app.display().flushes[before..],
        [],
        "a drag with nothing to scrub repaints nothing at all"
    );
}

#[test]
fn an_open_sheet_owns_every_tap_even_on_a_paginated_screen() {
    // The bug this pins: a paginated screen puts invisible First/Prev/Next/Last
    // targets in the bottom strip, and the quick sheet's action rows sit in
    // exactly that strip. Falling through to the screen behind meant the sheet's
    // own bottom row silently paged the library instead — so "Close" did nothing
    // on any library big enough to paginate, and only on those.
    let dir = tempfile::tempdir().unwrap();
    let lib = dir.path().join("Manga");
    for i in 0..12 {
        make_cbz(&lib.join(format!("Series {i:02}/vol1.cbz")), 1);
    }
    let mut app = app(&lib, FakeGateway::default(), vec![]).with_settings_dir(dir.path().join("d"));
    app.run().unwrap();
    app.open_library().unwrap();
    assert!(app.current_page_count() > 1, "needs to paginate");
    let page_before = app.current_page();

    app.open_quick_settings().unwrap();
    let sheet = app.quick_sheet_layout();
    let l = layout();

    // Sweep the whole sheet, both edges and the middle. Nothing behind it may
    // move: not the page, not the stack.
    let depth = app.stack_depth();
    let mut y = sheet.top;
    while y < l.height {
        for x in [l.pad, l.width / 2, l.width - l.pad - 1] {
            // The tiles and the action row DO act; what must never happen is
            // the screen behind reacting.
            app.handle_tap(x, y).unwrap();
            if app.sheet().is_none() {
                // A dismiss (the ×, or a miss below the last row) — reopen and
                // carry on sweeping.
                assert_eq!(app.stack_depth(), depth, "the sheet popped a screen");
                app.open_quick_settings().unwrap();
            }
            assert_eq!(
                app.current_page(),
                page_before,
                "a tap at ({x}, {y}) paged the library behind the sheet"
            );
        }
        y += l.row_h / 3;
    }

    // And the row that DOES navigate still navigates, from the same strip.
    app.handle_tap(l.width / 2, sheet.actions_top + l.row_h / 2)
        .unwrap();
    assert!(
        matches!(app.screen(), Screen::Settings),
        "\"All settings\" is unreachable on a paginated screen: {:?}",
        app.screen()
    );
}

#[test]
fn every_sheet_carries_an_x_that_closes_it() {
    // There is no "Close" row any more — it cost a row to say what the ×, and
    // the whole screen above the sheet, already say. Both of those have to work
    // for every sheet, or a sheet becomes a trap.
    let dir = tempfile::tempdir().unwrap();
    let lib = dir.path().join("Manga");
    make_cbz(&lib.join("Berserk/vol1.cbz"), 2);
    let l = layout();

    for open in ["quick", "profiles", "book"] {
        let mut app =
            app(&lib, FakeGateway::default(), vec![]).with_settings_dir(dir.path().join(open));
        app.run().unwrap();
        match open {
            "quick" => app.open_quick_settings().unwrap(),
            "profiles" => app.open_profile_menu().unwrap(),
            _ => {
                app.open_library().unwrap();
                let (cx, cy) = app.shelf_layout().cell_origin(0);
                app.handle_long_press(cx + 4, l.content_top() + cy + 4)
                    .unwrap();
            }
        }
        assert!(app.sheet().is_some(), "{open}: nothing opened");
        let top = app.sheet_bounds().unwrap().0;
        let (bx, by, bw, bh) = gideon_render::widgets::panel_close_box(0, top, l.width, l.text_px);

        // Just inside the × box.
        app.handle_tap(bx + bw / 2, by + bh / 2).unwrap();
        assert!(app.sheet().is_none(), "{open}: the × did not close it");

        // And a tap above the sheet still closes it too.
        let depth = app.stack_depth();
        match open {
            "quick" => app.open_quick_settings().unwrap(),
            "profiles" => app.open_profile_menu().unwrap(),
            _ => {
                let (cx, cy) = app.shelf_layout().cell_origin(0);
                app.handle_long_press(cx + 4, l.content_top() + cy + 4)
                    .map(|_| ())
                    .unwrap()
            }
        }
        let top = app.sheet_bounds().unwrap().0;
        app.handle_tap(l.width / 2, top.saturating_sub(l.row_h / 2))
            .unwrap();
        assert!(
            app.sheet().is_none(),
            "{open}: tapping outside did not close it"
        );
        assert_eq!(app.stack_depth(), depth, "{open}: dismissing navigated");
    }
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

#[test]
fn the_title_bar_pages_and_does_not_swallow_the_view_toggle() {
    // The page indicator is the pager: "‹ 2/3 ›" at the right end of the
    // title bar. A separate Previous/Next row above the nav bar said the
    // same thing twice and cost a row of content to say it.
    let dir = tempfile::tempdir().unwrap();
    let lib = dir.path().join("Manga");
    for i in 0..12 {
        make_cbz(&lib.join(format!("Series {i:02}/vol1.cbz")), 1);
    }

    let mut app = app(&lib, FakeGateway::default(), vec![]);
    app.run().unwrap();
    app.open_library().unwrap();
    let pages = app.current_page_count();
    assert!(pages > 1, "needs to paginate");

    let l = layout();
    let (prev, next, _) = super::title_pager_zones(&l, 0, pages, 0);
    let title_y = l.title_h / 2;
    let page_of = |app: &UiApp<MemoryDisplay, FakeInput, FakeGateway>| match app.screen() {
        Screen::Library { page, .. } => *page,
        other => panic!("expected the library, got {other:?}"),
    };

    // Forward, then back, from the title bar.
    app.handle_tap((next.0 + next.1) / 2, title_y).unwrap();
    assert_eq!(page_of(&app), 1, "the › chevron pages forward");
    app.handle_tap((prev.0 + prev.1) / 2, title_y).unwrap();
    assert_eq!(page_of(&app), 0, "the ‹ chevron pages back");

    // The rest of the title bar still toggles the library view — the two
    // must not fight over the same strip. "The rest" starts past the
    // Manga | Books tab strip at the left end.
    let before = effective_settings(&dir.path().join("data"), &lib).library_view;
    let [_, books_tab] = super::library_tab_zones(&l);
    app.handle_tap(books_tab.1 + l.pad, title_y).unwrap();
    let after = effective_settings(&dir.path().join("data"), &lib).library_view;
    assert_ne!(before, after, "a tap on the title still toggles the view");
    assert_eq!(page_of(&app), 0, "and does not page");

    // The chevrons are finger-sized, not glyph-sized.
    assert!(next.1 - next.0 >= l.title_h.min(88), "next zone too small");
    assert!(prev.1 - prev.0 >= l.title_h.min(88), "prev zone too small");
}

#[test]
fn the_calendar_is_a_per_profile_choice_that_replaces_the_heatmap() {
    // Two views of the same history answering different questions — "how
    // much, lately" against "what was I on, and for how many days running".
    // Which one you want is taste, so it lives with the profile.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("Manga");
    let alex = root.join("@alex");
    make_cbz(&root.join("Berserk/vol1.cbz"), 4);
    make_cbz(&alex.join("Pluto/vol1.cbz"), 4);
    let now = now_unix();
    write_progress(&root, &[("Berserk/vol1.cbz", 1, 4, now - 3600)]);
    write_progress(&alex, &[("Pluto/vol1.cbz", 1, 4, now - 3600)]);
    let settings_dir = profile_settings_dir(dir.path(), &["default", "alex"]);

    // The default profile takes the calendar; alex is untouched.
    gideon_core::ProfileSettings {
        stats_view: Some("calendar".into()),
        ..Default::default()
    }
    .save(&root)
    .unwrap();

    let mut chosen =
        app(&root, FakeGateway::default(), vec![]).with_settings_dir(settings_dir.clone());
    chosen.run().unwrap();
    assert!(
        chosen.stats_is_calendar(),
        "the profile that chose it gets it"
    );
    assert_eq!(
        effective_settings(&settings_dir, &alex).stats_view,
        "heatmap",
        "and the other profile keeps the default"
    );

    // The calendar takes the room the waiting list would use, so Today shows
    // the chart and the Continue card and nothing else.
    assert!(
        chosen.waiting_rows().is_empty() || chosen.stats_is_calendar(),
        "the calendar view drops the waiting list"
    );

    // Both views still leave the Continue card somewhere it can be tapped.
    for view in ["calendar", "heatmap"] {
        gideon_core::ProfileSettings {
            stats_view: Some(view.into()),
            ..Default::default()
        }
        .save(&root)
        .unwrap();
        let mut view_app =
            app(&root, FakeGateway::default(), vec![]).with_settings_dir(settings_dir.clone());
        view_app.run().unwrap();
        let top = view_app.continue_card_top();
        assert!(
            view_app.continue_card_hit(top + 1),
            "{view}: the Continue card must still be reachable"
        );
        assert!(
            top + view_app.continue_card_height() <= layout().nav_top(),
            "{view}: the Continue card runs under the nav bar"
        );
    }
}

#[test]
fn the_calendar_draws_a_run_of_days_as_one_bar() {
    // The point of the view, end to end: four evenings of one series is one
    // bar four days wide on the screen, not four marks.
    let dir = tempfile::tempdir().unwrap();
    let lib = dir.path().join("Manga");
    make_cbz(&lib.join("Berserk/vol1.cbz"), 4);
    gideon_core::ProfileSettings {
        stats_view: Some("calendar".into()),
        ..Default::default()
    }
    .save(&lib)
    .unwrap();

    let now = now_unix();
    let entries: Vec<(String, usize, usize, u64)> = (0..4)
        .map(|d| (format!("Berserk/ch{d}.cbz"), 3, 4, now - (d + 1) * 86_400))
        .collect();
    let refs: Vec<(&str, usize, usize, u64)> = entries
        .iter()
        .map(|(k, p, t, a)| (k.as_str(), *p, *t, *a))
        .collect();
    write_progress(&lib, &refs);

    let mut app =
        app(&lib, FakeGateway::default(), vec![]).with_settings_dir(dir.path().join("data"));
    app.run().unwrap();

    let stats = app.reading_stats();
    let spans = stats.spans(stats.today - 40, stats.today);
    let berserk: Vec<&gideon_core::ReadingSpan> =
        spans.iter().filter(|s| s.series == "Berserk").collect();
    assert_eq!(berserk.len(), 1, "four consecutive evenings are ONE run");
    assert_eq!(berserk[0].days(), 4);
}

#[test]
fn slow_handled_tap_drops_the_taps_queued_behind_it() {
    // Regression: taps made while a slow handler (render, e-ink refresh wait,
    // IO) ran used to fire afterwards against a screen that had moved on —
    // the "queued taps fire late and select the wrong thing" bug.
    let dir = tempfile::tempdir().unwrap();
    let lib = dir.path().join("Manga");
    make_cbz(&lib.join("Berserk/vol1.cbz"), 3);

    let tap = UiEvent::Tap { x: 5, y: 5 };
    let mut slow = app(&lib, FakeGateway::default(), vec![tap])
        .with_stale_tap_after(std::time::Duration::ZERO);
    slow.run().unwrap();
    assert!(
        slow.input().discard_taps_calls >= 1,
        "a handler that took 'too long' must flush the stale queue"
    );

    // A handler that finishes quickly must not eat the user's next tap.
    let mut fast = app(&lib, FakeGateway::default(), vec![tap])
        .with_stale_tap_after(std::time::Duration::from_secs(3600));
    fast.run().unwrap();
    assert_eq!(
        fast.input().discard_taps_calls,
        0,
        "fast handling must never drop input"
    );
}

/// Headless latency harness (not part of the normal run):
/// `cargo test --release -p gideon-app tap_latency_bench -- --ignored --nocapture`.
/// Reports host-side CPU cost per handled tap and panel flushes per tap. It
/// excludes the e-ink refresh wait, which only exists on the device.
#[test]
#[ignore]
fn tap_latency_bench() {
    let dir = tempfile::tempdir().unwrap();
    let lib = dir.path().join("Manga");
    for i in 0..40 {
        make_cbz(&lib.join(format!("Series{i:02}/vol1.cbz")), 3);
    }
    let taps: Vec<UiEvent> = (0..60)
        .map(|i| UiEvent::Tap {
            x: 60 + (i % 5) * 100,
            y: 150 + (i % 7) * 90,
        })
        .collect();
    let n = taps.len();
    let mut app = app(&lib, FakeGateway::default(), taps);
    let start = std::time::Instant::now();
    app.run().unwrap();
    let total = start.elapsed();
    let flushes = app.display().flushes.len();
    let full = app
        .display()
        .flushes
        .iter()
        .filter(|m| **m == RefreshMode::Full)
        .count();
    println!(
        "BENCH taps={n} total={total:?} per_tap={:?} flushes={flushes} full={full}",
        total / n as u32
    );
}

// --- Books (OPDS) -----------------------------------------------------------
//
// The gateway runs the real OPDS client over captured Gutenberg feeds and a
// hand-written Calibre one, so these cover the whole path from the Home card
// to an EPUB on disk — the only fake is the network.

mod books {
    use super::*;

    const ROOT: &[u8] =
        include_bytes!("../../../gideon-sources/tests/fixtures/opds/gutenberg-root.xml");
    const SEARCH: &[u8] =
        include_bytes!("../../../gideon-sources/tests/fixtures/opds/gutenberg-search.xml");
    const POPULAR: &[u8] =
        include_bytes!("../../../gideon-sources/tests/fixtures/opds/gutenberg-popular.xml");
    const BOOK84: &[u8] =
        include_bytes!("../../../gideon-sources/tests/fixtures/opds/gutenberg-book84.xml");
    const OSD: &[u8] =
        include_bytes!("../../../gideon-sources/tests/fixtures/opds/gutenberg-osd.xml");
    const CALIBRE_ROOT: &[u8] =
        include_bytes!("../../../gideon-sources/tests/fixtures/opds/calibre-root.xml");
    const CALIBRE_NEWEST: &[u8] =
        include_bytes!("../../../gideon-sources/tests/fixtures/opds/calibre-newest.xml");

    const GUTENBERG: &str = "https://www.gutenberg.org/ebooks.opds/";
    const EPUB: &[u8] = b"PK\x03\x04 a tiny epub";

    fn gutenberg_only() -> FakeGateway {
        FakeGateway {
            catalogs: vec![gideon_core::BookCatalog {
                name: "Project Gutenberg".into(),
                url: GUTENBERG.into(),
            }],
            web: gideon_sources::FakeFetcher::new()
                .with(GUTENBERG, ROOT)
                .with("https://www.gutenberg.org/ebooks/search.opds/?sort_order=downloads", POPULAR)
                .with("https://www.gutenberg.org/ebooks/search.opds/?sort_order=downloads&start_index=26", POPULAR)
                .with("https://www.gutenberg.org/ebooks/84.opds", BOOK84)
                .with("https://www.gutenberg.org/ebooks/84.epub3.images", EPUB)
                .with("https://www.gutenberg.org/catalog/osd-books.xml", OSD)
                .with("http://m.gutenberg.org/ebooks/search.opds/?query=frankenstein", SEARCH),
            ..FakeGateway::default()
        }
    }

    /// Home -> Books. Books is card 3 (fourth card).
    fn tap_books() -> UiEvent {
        tap_card(3)
    }

    /// The list rows of the current BookList, as the painter labels them.
    fn book_list(app: &UiApp<MemoryDisplay, FakeInput, FakeGateway>) -> (String, Vec<String>) {
        let Screen::BookList { title, feed, .. } = app.screen() else {
            panic!("expected a book list, got {:?}", app.screen());
        };
        let rows = book_rows(feed)
            .into_iter()
            .map(|r| match r {
                BookRow::Search => "Search this catalogue".to_string(),
                BookRow::Entry(i) => feed.entries[i].label(),
                BookRow::More => "More…".to_string(),
            })
            .collect();
        (title.clone(), rows)
    }

    #[test]
    fn one_catalogue_opens_straight_from_the_card() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = app(
            dir.path(),
            gutenberg_only(),
            vec![nav_discover(), tap_books()],
        );
        app.run().unwrap();
        let (title, rows) = book_list(&app);
        assert_eq!(title, "Project Gutenberg");
        // Gutenberg offers search, so the first row is the search row, then
        // its sections with their one-line blurbs; no "More" because the
        // root isn't paged.
        assert_eq!(
            rows,
            [
                "Search this catalogue",
                "Popular — Our most popular books.",
                "Latest — Our latest releases.",
                "Random — Random books.",
            ]
        );
    }

    #[test]
    fn no_catalogues_explains_how_to_add_one_instead_of_an_empty_list() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = app(
            dir.path(),
            FakeGateway::default(),
            vec![nav_discover(), tap_books()],
        );
        app.run().unwrap();
        let Screen::Message { title, body } = app.screen() else {
            panic!("expected a hint, got {:?}", app.screen());
        };
        assert_eq!(title, "Books");
        assert!(body.contains("book_catalogs"), "{body}");
        assert!(body.contains("settings.json"), "{body}");
    }

    #[test]
    fn catalogues_typed_on_the_phone_join_the_list_without_duplicates() {
        let dir = tempfile::tempdir().unwrap();
        // The sync sweep's cache: the user's Calibre, plus Gutenberg again
        // (they added it on the phone too) which must not show twice.
        let cache = dir.path().join(".gideon");
        std::fs::create_dir_all(&cache).unwrap();
        std::fs::write(
            cache.join("catalogs.json"),
            r#"[{"name":"Home","url":"http://calibre.local:8080/opds"},{"name":"PG","url":"https://www.gutenberg.org/ebooks.opds/"}]"#,
        )
        .unwrap();
        let mut app = app(
            dir.path(),
            gutenberg_only(),
            vec![nav_discover(), tap_books()],
        );
        app.run().unwrap();
        let Screen::Books { catalogs } = app.screen() else {
            panic!("expected the catalogue list, got {:?}", app.screen());
        };
        let labels: Vec<String> = catalogs.iter().map(|c| c.label()).collect();
        assert_eq!(labels, ["Project Gutenberg", "Home"]);
    }

    #[test]
    fn a_file_send_waiting_on_a_retry_is_listed_but_not_a_search() {
        let dir = tempfile::tempdir().unwrap();
        let sends = vec![
            gideon_sync::supabase::SendItem::title("t1", "Berserk"),
            gideon_sync::supabase::SendItem {
                id: "f1".into(),
                title: "Slaughterhouse-Five.epub".into(),
                kind: gideon_sync::supabase::SendKind::File,
                file_path: Some("u/r/Slaughterhouse-Five.epub".into()),
                file_size: Some(512000),
            },
        ];
        let cache = dir.path().join(".gideon");
        std::fs::create_dir_all(&cache).unwrap();
        std::fs::write(
            cache.join("sends.json"),
            serde_json::to_vec(&sends).unwrap(),
        )
        .unwrap();
        // Tapping the file row must not start a source search for an EPUB.
        // (The bell sits one title-bar height left of the power zone.)
        let l = layout();
        let bell = UiEvent::Tap {
            x: l.width - l.title_h - l.title_h / 2,
            y: 1,
        };
        let mut app = app(
            dir.path(),
            search_gateway(),
            vec![tap_today(), bell, tap_row(1)],
        );
        app.run().unwrap();
        assert!(
            matches!(app.screen(), Screen::SentList { .. }),
            "stayed on the list, got {:?}",
            app.screen()
        );
        assert!(app.gateway().searches.borrow().is_empty(), "no search ran");
    }

    #[test]
    fn several_catalogues_list_first() {
        let dir = tempfile::tempdir().unwrap();
        let mut gateway = gutenberg_only();
        gateway.catalogs.push(gideon_core::BookCatalog {
            name: String::new(),
            url: "http://calibre.local:8080/opds".into(),
        });
        gateway.web = gateway
            .web
            .with("http://calibre.local:8080/opds", CALIBRE_ROOT);
        let mut app = app(
            dir.path(),
            gateway,
            vec![nav_discover(), tap_books(), tap_row(1)],
        );
        app.run().unwrap();
        // Row 1 was the unnamed Calibre server, labelled by its host.
        let (title, rows) = book_list(&app);
        assert_eq!(title, "calibre.local");
        assert!(rows.iter().any(|r| r.starts_with("Newest — ")), "{rows:?}");
    }

    #[test]
    fn a_gutenberg_stub_downloads_the_illustrated_epub_into_the_kobo_library() {
        let dir = tempfile::tempdir().unwrap();
        // Books -> "Popular" (row 1, after the search row) -> first book.
        let mut app = app(
            dir.path(),
            gutenberg_only(),
            vec![nav_discover(), tap_books(), tap_row(1), tap_row(2)],
        );
        app.run().unwrap();

        // The popular list's row 2 is its second entry (row 0 = search,
        // row 1 = first entry): Frankenstein, a stub that needed a second
        // fetch to find its EPUB links.
        let Screen::Message { title, body } = app.screen() else {
            panic!("expected the saved message, got {:?}", app.screen());
        };
        assert_eq!(title, "Books");
        assert!(body.starts_with("Saved Frankenstein"), "{body}");
        // The one line that matters: when they will actually see it.
        assert!(body.contains("after you exit gideon"), "{body}");

        // On disk, beside the manga library as Author/Title.epub, NOT inside
        // the manga library where gideon's own scan would ignore it.
        let books = dir.path().join("Books");
        let saved: Vec<PathBuf> = walk(&books);
        assert_eq!(saved.len(), 1, "{saved:?}");
        let path = &saved[0];
        assert_eq!(path.extension().and_then(|e| e.to_str()), Some("epub"));
        assert!(
            path.to_string_lossy()
                .contains("Mary Wollstonecraft Shelley"),
            "{path:?}"
        );
        assert!(path.to_string_lossy().contains("Frankenstein"), "{path:?}");
        assert_eq!(std::fs::read(path).unwrap(), EPUB);
        assert!(
            gideon_core::Library::new(dir.path())
                .scan()
                .unwrap()
                .is_empty(),
            "not a manga"
        );
    }

    #[test]
    fn a_book_already_on_the_kobo_says_so_and_fetches_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let existing = dir
            .path()
            .join("Books/Mary Wollstonecraft Shelley/Frankenstein; or, The modern Prometheus.epub");
        std::fs::create_dir_all(existing.parent().unwrap()).unwrap();
        std::fs::write(&existing, b"PK already here").unwrap();
        let mut gateway = gutenberg_only();
        // No EPUB body canned: a fetch would fail, so success proves no fetch.
        gateway.web = gideon_sources::FakeFetcher::new()
            .with(GUTENBERG, ROOT)
            .with(
                "https://www.gutenberg.org/ebooks/search.opds/?sort_order=downloads",
                POPULAR,
            )
            .with("https://www.gutenberg.org/ebooks/84.opds", BOOK84);
        let mut app = app(
            dir.path(),
            gateway,
            vec![nav_discover(), tap_books(), tap_row(1), tap_row(2)],
        );
        app.run().unwrap();
        let Screen::Message { body, .. } = app.screen() else {
            panic!("expected a message, got {:?}", app.screen());
        };
        assert!(body.contains("already on your Kobo"), "{body}");
        assert_eq!(
            std::fs::read(&existing).unwrap(),
            b"PK already here",
            "untouched"
        );
    }

    #[test]
    fn more_appends_the_next_page_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let mut first = app(
            dir.path(),
            gutenberg_only(),
            vec![nav_discover(), tap_books(), tap_row(1)],
        );
        first.run().unwrap();
        let (_, rows) = book_list(&first);
        assert_eq!(rows.last().map(String::as_str), Some("More…"));
        let before = rows.len();
        let more_row = before - 1;

        // The More row is on the last page of the list; jump there and tap it.
        let per_page = layout().rows_per_page();
        let last_page = (before - 1) / per_page;
        let mut events: Vec<UiEvent> = vec![nav_discover(), tap_books(), tap_row(1)];
        for _ in 0..last_page {
            events.push(tap_nav_next());
        }
        events.push(tap_row(more_row - last_page * per_page));
        let mut again = app(dir.path(), gutenberg_only(), events);
        again.run().unwrap();
        let (_, rows) = book_list(&again);
        // Same canned page again, so twice the entries — still one More row,
        // still a list (not a message), still the same screen.
        assert_eq!(rows.len(), before * 2 - 2, "{}", rows.len());
        assert_eq!(rows.last().map(String::as_str), Some("More…"));
    }

    #[test]
    fn search_goes_through_opensearch_and_lists_results() {
        let dir = tempfile::tempdir().unwrap();
        let mut events = vec![nav_discover(), tap_books(), tap_row(0)];
        for ch in "frankenstein".chars() {
            events.push(tap_key(Key::Char(ch)));
        }
        events.push(tap_key(Key::Search));
        let mut app = app(dir.path(), gutenberg_only(), events);
        app.run().unwrap();
        let (title, rows) = book_list(&app);
        assert_eq!(title, "\"frankenstein\"");
        assert!(
            rows.iter()
                .any(|r| r.starts_with("Frankenstein; or, The modern Prometheus — Mary")),
            "{rows:?}"
        );
    }

    #[test]
    fn a_catalogue_that_cannot_be_reached_explains_rather_than_erroring_out() {
        let dir = tempfile::tempdir().unwrap();
        let gateway = FakeGateway {
            catalogs: vec![gideon_core::BookCatalog {
                name: "Home".into(),
                url: "http://calibre.local:8080/opds".into(),
            }],
            ..FakeGateway::default() // nothing canned: every fetch fails
        };
        let mut app = app(dir.path(), gateway, vec![nav_discover(), tap_books()]);
        app.run().unwrap();
        let Screen::Message { title, body } = app.screen() else {
            panic!("expected a message, got {:?}", app.screen());
        };
        assert_eq!(title, "Home");
        assert!(body.starts_with("Couldn't open this catalogue"), "{body}");
    }

    #[test]
    fn calibre_books_download_directly_without_a_second_fetch() {
        let dir = tempfile::tempdir().unwrap();
        let gateway = FakeGateway {
            catalogs: vec![gideon_core::BookCatalog {
                name: "Home".into(),
                url: "http://calibre.local:8080/opds".into(),
            }],
            web: gideon_sources::FakeFetcher::new()
                .with("http://calibre.local:8080/opds", CALIBRE_ROOT)
                .with(
                    "http://calibre.local:8080/opds/navcatalog/4f6e6577657374",
                    CALIBRE_NEWEST,
                )
                .with("http://calibre.local:8080/get/epub/12/calibre", EPUB),
            ..FakeGateway::default()
        };
        // Root: [search, By Newest, Newest, By Author]; "Newest" is row 2.
        // Newest: [search, Apothecary, Slaughterhouse, More]; row 1 = the book.
        let mut app = app(
            dir.path(),
            gateway,
            vec![nav_discover(), tap_books(), tap_row(2), tap_row(1)],
        );
        app.run().unwrap();
        let Screen::Message { body, .. } = app.screen() else {
            panic!("expected the saved message, got {:?}", app.screen());
        };
        assert!(
            body.starts_with("Saved The Apothecary Diaries: Volume 1"),
            "{body}"
        );
        let saved = walk(&dir.path().join("Books"));
        assert_eq!(saved.len(), 1);
        assert!(
            saved[0].ends_with("Natsu Hyuuga, Touko Shino/The Apothecary Diaries_ Volume 1.epub"),
            "{:?}",
            saved[0]
        );
    }

    pub(super) fn walk(dir: &Path) -> Vec<PathBuf> {
        let mut out = Vec::new();
        if let Ok(rd) = std::fs::read_dir(dir) {
            for e in rd.flatten() {
                let p = e.path();
                if p.is_dir() {
                    out.extend(walk(&p));
                } else {
                    out.push(p);
                }
            }
        }
        out.sort();
        out
    }
}

// --- Library tabs: Manga | Books ---

mod library_tabs {
    use super::*;

    /// A minimal EPUB whose OPF names it; the file name does not.
    pub(super) fn make_epub(path: &Path, title: &str, author: &str) {
        make_epub_with(path, title, author, &[("One", &["So it goes."])]);
    }

    /// An EPUB whose manifest marks a `cover-image`: a 60×80 PNG of one
    /// `shade`, so a test can tell it on the shelf by a pixel.
    fn make_epub_with_cover(path: &Path, title: &str, author: &str, shade: u8) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let file = std::fs::File::create(path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        let opts = zip::write::SimpleFileOptions::default();
        zip.start_file("META-INF/container.xml", opts).unwrap();
        zip.write_all(
            br#"<container><rootfiles><rootfile full-path="OEBPS/content.opf"/></rootfiles></container>"#,
        )
        .unwrap();
        zip.start_file("OEBPS/content.opf", opts).unwrap();
        write!(
            zip,
            r#"<package xmlns:dc="http://purl.org/dc/elements/1.1/"><metadata><dc:title>{title}</dc:title><dc:creator>{author}</dc:creator></metadata><manifest><item id="c" href="c.xhtml" media-type="application/xhtml+xml"/><item id="cov" href="Images/cover.png" media-type="image/png" properties="cover-image"/></manifest><spine><itemref idref="c"/></spine></package>"#
        )
        .unwrap();
        zip.start_file("OEBPS/c.xhtml", opts).unwrap();
        zip.write_all(b"<html><body><h1>One</h1><p>So it goes.</p></body></html>")
            .unwrap();
        zip.start_file("OEBPS/Images/cover.png", opts).unwrap();
        let img = image::GrayImage::from_pixel(60, 80, image::Luma([shade]));
        let mut buf = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageLuma8(img)
            .write_to(&mut buf, image::ImageFormat::Png)
            .unwrap();
        zip.write_all(&buf.into_inner()).unwrap();
        zip.finish().unwrap();
    }

    /// A readable EPUB: each chapter is a heading and its paragraphs, in a
    /// spine in that order.
    pub(super) fn make_epub_with(
        path: &Path,
        title: &str,
        author: &str,
        chapters: &[(&str, &[&str])],
    ) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let file = std::fs::File::create(path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        let opts = zip::write::SimpleFileOptions::default();
        zip.start_file("META-INF/container.xml", opts).unwrap();
        zip.write_all(
            br#"<container><rootfiles><rootfile full-path="content.opf"/></rootfiles></container>"#,
        )
        .unwrap();
        let mut manifest = String::new();
        let mut spine = String::new();
        for i in 0..chapters.len() {
            manifest.push_str(&format!(
                r#"<item id="c{i}" href="c{i}.xhtml" media-type="application/xhtml+xml"/>"#
            ));
            spine.push_str(&format!(r#"<itemref idref="c{i}"/>"#));
        }
        zip.start_file("content.opf", opts).unwrap();
        write!(
            zip,
            r#"<package xmlns:dc="http://purl.org/dc/elements/1.1/"><metadata><dc:title>{title}</dc:title><dc:creator>{author}</dc:creator></metadata><manifest>{manifest}</manifest><spine>{spine}</spine></package>"#
        )
        .unwrap();
        for (i, (heading, paras)) in chapters.iter().enumerate() {
            zip.start_file(format!("c{i}.xhtml"), opts).unwrap();
            write!(zip, "<html><body><h1>{heading}</h1>").unwrap();
            for p in paras.iter() {
                write!(zip, "<p>{p}</p>").unwrap();
            }
            write!(zip, "</body></html>").unwrap();
        }
        zip.finish().unwrap();
    }

    /// An EPUB whose one chapter is a paragraph, a picture, a paragraph.
    /// The picture is `w × h`, solid dark grey, so its pixels are unmistakable.
    fn make_illustrated_epub(path: &Path, w: u32, h: u32) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let file = std::fs::File::create(path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        let opts = zip::write::SimpleFileOptions::default();
        zip.start_file("META-INF/container.xml", opts).unwrap();
        zip.write_all(
            br#"<container><rootfiles><rootfile full-path="OEBPS/content.opf"/></rootfiles></container>"#,
        )
        .unwrap();
        zip.start_file("OEBPS/content.opf", opts).unwrap();
        zip.write_all(br#"<package xmlns:dc="http://purl.org/dc/elements/1.1/"><metadata><dc:title>Pictures</dc:title><dc:creator>A</dc:creator></metadata><manifest><item id="c" href="Text/c.xhtml" media-type="application/xhtml+xml"/><item id="p" href="Images/p.png" media-type="image/png"/></manifest><spine><itemref idref="c"/></spine></package>"#).unwrap();
        zip.start_file("OEBPS/Text/c.xhtml", opts).unwrap();
        zip.write_all(br#"<html><body><p>Before the picture.</p><img src="../Images/p.png"/><p>After the picture.</p></body></html>"#).unwrap();
        zip.start_file("OEBPS/Images/p.png", opts).unwrap();
        let img = image::GrayImage::from_pixel(w, h, image::Luma([40u8]));
        let mut buf = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageLuma8(img)
            .write_to(&mut buf, image::ImageFormat::Png)
            .unwrap();
        zip.write_all(&buf.into_inner()).unwrap();
        zip.finish().unwrap();
    }

    /// Tap the "Books" tab in the Library's title bar.
    fn tap_books_tab() -> UiEvent {
        let l = layout();
        let [_, books] = library_tab_zones(&l);
        UiEvent::Tap {
            x: (books.0 + books.1) / 2,
            y: l.title_h / 2,
        }
    }

    /// Tap the "Manga" tab in the Library's title bar.
    fn tap_manga_tab() -> UiEvent {
        let l = layout();
        let [manga, _] = library_tab_zones(&l);
        UiEvent::Tap {
            x: (manga.0 + manga.1) / 2,
            y: l.title_h / 2,
        }
    }

    /// Row `i` of an EPUB's four-row book sheet: 0 read here, 1 read in
    /// Kobo, 2 delete, 3 close.
    fn tap_book_file_row(i: usize) -> UiEvent {
        tap_modal_row(5, i)
    }

    /// A tap in the reader: left third back, middle menu, right third next.
    fn tap_reader(zone: ReaderZone) -> UiEvent {
        let x = match zone {
            ReaderZone::PrevPage => W / 6,
            ReaderZone::Back => W / 2,
            ReaderZone::NextPage => W * 5 / 6,
        };
        UiEvent::Tap { x, y: H / 2 }
    }

    /// The manga reader's controls gesture: a flick up off the bottom edge
    /// of the page opens the reading menu.
    fn open_reading_menu() -> UiEvent {
        UiEvent::Swipe {
            x0: W / 2,
            y0: H - 20,
            x1: W / 2,
            y1: H - 20 - H / 6,
        }
    }

    /// Row `i` of the reading menu: 0 contents, 1 bookmarks, 2 mark page,
    /// 3 text, 4 night mode, 5 rotate, 6 close book, 7 close.
    fn tap_reading_row(i: usize) -> UiEvent {
        tap_modal_row(8, i)
    }

    /// The y of row `i` of the reading menu.
    fn reading_row_y(i: usize) -> u32 {
        tap_modal_row_y(8, i)
    }

    /// A plain StarDict dictionary (ifo, idx, dict) at `dir/<stem>`.
    fn write_stardict(dir: &Path, stem: &str, entries: &[(&str, &str)]) {
        std::fs::create_dir_all(dir).unwrap();
        let mut sorted = entries.to_vec();
        sorted.sort_by_key(|(w, _)| w.to_lowercase());
        let (mut idx, mut data) = (Vec::new(), Vec::new());
        for (w, d) in &sorted {
            idx.extend_from_slice(w.as_bytes());
            idx.push(0);
            idx.extend_from_slice(&(data.len() as u32).to_be_bytes());
            idx.extend_from_slice(&(d.len() as u32).to_be_bytes());
            data.extend_from_slice(d.as_bytes());
        }
        std::fs::write(dir.join(format!("{stem}.idx")), &idx).unwrap();
        std::fs::write(dir.join(format!("{stem}.dict")), &data).unwrap();
        std::fs::write(
            dir.join(format!("{stem}.ifo")),
            format!(
                "StarDict's dict ifo file\nversion=2.4.2\nwordcount={}\nidxfilesize={}\nbookname=Test Words\nsametypesequence=m\n",
                sorted.len(),
                idx.len()
            ),
        )
        .unwrap();
    }

    /// Open the first book on the Books tab in gideon's reader.
    fn open_first_book() -> Vec<UiEvent> {
        vec![
            tap_nav(0),
            tap_books_tab(),
            tap_row(0),
            tap_book_file_row(0),
        ]
    }

    /// A novel long enough to span pages and chapters.
    fn novel(dir: &Path) -> (PathBuf, PathBuf) {
        let lib = dir.join("Manga");
        make_cbz(&lib.join("Series/vol1.cbz"), 2);
        let books = lib.join("Books");
        let long: Vec<String> = (0..60)
            .map(|i| {
                format!(
                    "Paragraph {i}: Billy Pilgrim has come unstuck in time, and so it goes, \
                     again and again, across every page of this chapter."
                )
            })
            .collect();
        let long_refs: Vec<&str> = long.iter().map(String::as_str).collect();
        make_epub_with(
            &books.join("Kurt Vonnegut/book-0001.epub"),
            "Slaughterhouse-Five",
            "Kurt Vonnegut",
            &[("One", &long_refs), ("Two", &["Listen:", "Short chapter."])],
        );
        (lib, books)
    }

    fn shelf(dir: &Path) -> (PathBuf, PathBuf) {
        let lib = dir.join("Manga");
        make_cbz(&lib.join("Series/vol1.cbz"), 2);
        let books = lib.join("Books");
        make_epub(
            &books.join("Kurt Vonnegut/book-0001.epub"),
            "Slaughterhouse-Five",
            "Kurt Vonnegut",
        );
        make_epub(
            &books.join("Natsu Hyuuga/book-0002.epub"),
            "The Apothecary Diaries, Vol. 1",
            "Natsu Hyuuga",
        );
        (lib, books)
    }

    #[test]
    fn the_library_opens_on_manga_and_the_books_tab_lists_books_by_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let (lib, _) = shelf(dir.path());
        let mut ui = app(
            &lib,
            FakeGateway::default(),
            vec![tap_nav(0), tap_books_tab()],
        );
        ui.run().unwrap();

        let Screen::BookShelf { books, page } = ui.screen() else {
            panic!("expected the Books tab, got {:?}", ui.screen());
        };
        assert_eq!(*page, 0);
        let labels: Vec<String> = books.iter().map(|b| b.label()).collect();
        assert_eq!(
            labels,
            [
                "Slaughterhouse-Five — Kurt Vonnegut",
                "The Apothecary Diaries, Vol. 1 — Natsu Hyuuga",
            ]
        );
        // The tab switch repaints the whole content area: Full, never a
        // partial that would ghost covers under the list.
        assert_eq!(ui.display().flushes.last(), Some(&RefreshMode::Full));
        // The nav bar still marks Library as where you are.
        assert!(
            ui.nav_items()[0].active,
            "Books is a Library tab, not a new destination"
        );
    }

    #[test]
    fn manga_never_show_on_the_books_tab_and_books_never_on_manga() {
        let dir = tempfile::tempdir().unwrap();
        let (lib, _) = shelf(dir.path());
        let mut ui = app(
            &lib,
            FakeGateway::default(),
            vec![tap_nav(0), tap_books_tab()],
        );
        ui.run().unwrap();
        let Screen::BookShelf { books, .. } = ui.screen() else {
            panic!("expected the Books tab");
        };
        assert!(
            books.iter().all(|b| b.format == "epub"),
            "a .cbz must not appear on the Books tab: {books:?}"
        );

        let mut ui = app(&lib, FakeGateway::default(), vec![tap_nav(0)]);
        ui.run().unwrap();
        let Screen::Library { items, .. } = ui.screen() else {
            panic!("expected the Manga tab");
        };
        assert_eq!(
            items.len(),
            1,
            "one manga series; the EPUBs stay off the shelf"
        );
        assert_eq!(items[0].title(), "Series");
    }

    #[test]
    fn the_nav_tab_returns_to_the_library_tab_you_left() {
        let dir = tempfile::tempdir().unwrap();
        let (lib, _) = shelf(dir.path());
        // Library → Books tab → Today → Library: lands back on Books.
        let events = vec![tap_nav(0), tap_books_tab(), tap_today(), tap_nav(0)];
        let mut ui = app(&lib, FakeGateway::default(), events);
        ui.run().unwrap();
        assert!(matches!(ui.screen(), Screen::BookShelf { .. }));

        // And the Manga tab takes you back.
        let events = vec![tap_nav(0), tap_books_tab(), tap_manga_tab()];
        let mut ui = app(&lib, FakeGateway::default(), events);
        ui.run().unwrap();
        assert!(matches!(ui.screen(), Screen::Library { .. }));
    }

    #[test]
    fn empty_books_tab_explains_itself() {
        let dir = tempfile::tempdir().unwrap();
        let lib = dir.path().join("Manga");
        make_cbz(&lib.join("Series/vol1.cbz"), 2);
        let mut ui = app(
            &lib,
            FakeGateway::default(),
            vec![tap_nav(0), tap_books_tab()],
        );
        ui.run().unwrap();
        let Screen::BookShelf { books, .. } = ui.screen() else {
            panic!("expected the Books tab");
        };
        assert!(books.is_empty());
        // Something is drawn in the first content row (the hint), not a blank.
        let l = layout();
        let y = l.row_top(0) + l.row_h / 2;
        assert!(
            (0..W).any(|x| ui.display().pixel(x, y) < 0x80),
            "the empty Books tab should say how to get a book"
        );
    }

    #[test]
    fn the_kobo_reader_row_closes_gideon() {
        let dir = tempfile::tempdir().unwrap();
        let (lib, _) = shelf(dir.path());
        let events = vec![tap_nav(0), tap_books_tab(), tap_row(0)];
        let mut ui = app(&lib, FakeGateway::default(), events);
        ui.run().unwrap();
        let Some(Sheet::BookFile { book }) = ui.sheet() else {
            panic!("expected the book menu");
        };
        assert_eq!(book.title, "Slaughterhouse-Five");
        // The sheet slides over the list: Partial, no flash.
        assert_eq!(ui.display().flushes.last(), Some(&RefreshMode::Partial));

        let events = vec![
            tap_nav(0),
            tap_books_tab(),
            tap_row(0),
            tap_book_file_row(1),
        ];
        let mut ui = app(&lib, FakeGateway::default(), events);
        let exit = ui.run().unwrap();
        assert!(
            matches!(exit, Exit::Close),
            "reading hands the device to Nickel's reader"
        );
    }

    #[test]
    fn deleting_a_book_asks_first_then_removes_it_from_disk_and_the_shelf() {
        let dir = tempfile::tempdir().unwrap();
        let (lib, books_dir) = shelf(dir.path());
        let target = books_dir.join("Kurt Vonnegut/book-0001.epub");

        // Cancel leaves it alone.
        let events = vec![
            tap_nav(0),
            tap_books_tab(),
            tap_row(0),
            tap_book_file_row(3),
            tap_modal_row(2, 1),
        ];
        let mut ui = app(&lib, FakeGateway::default(), events);
        ui.run().unwrap();
        assert!(target.exists());
        assert!(ui.sheet().is_none());

        // Confirm deletes the file, its emptied author folder, and the row.
        let events = vec![
            tap_nav(0),
            tap_books_tab(),
            tap_row(0),
            tap_book_file_row(3),
            tap_modal_row(2, 0),
        ];
        let mut ui = app(&lib, FakeGateway::default(), events);
        ui.run().unwrap();
        assert!(!target.exists());
        assert!(!books_dir.join("Kurt Vonnegut").exists());
        let Screen::BookShelf { books, .. } = ui.screen() else {
            panic!("expected to stay on the Books tab");
        };
        assert_eq!(books.len(), 1);
        assert_eq!(books[0].title, "The Apothecary Diaries, Vol. 1");
        assert_eq!(ui.display().flushes.last(), Some(&RefreshMode::Full));
    }

    #[test]
    fn the_title_bar_elsewhere_still_toggles_the_manga_view() {
        let dir = tempfile::tempdir().unwrap();
        let (lib, _) = shelf(dir.path());
        let l = layout();
        let [_, books] = library_tab_zones(&l);
        // Past both tabs, before the pager: the old shelf/list toggle.
        let toggle = UiEvent::Tap {
            x: books.1 + l.pad * 2,
            y: l.title_h / 2,
        };
        let mut ui = app(&lib, FakeGateway::default(), vec![tap_nav(0), toggle]);
        ui.run().unwrap();
        assert!(matches!(ui.screen(), Screen::Library { .. }));
        // The list is the default, so one toggle lands on the cover shelf.
        assert_eq!(ui.load_settings().library_view, "shelf");
    }

    // --- reading in gideon ---

    #[test]
    fn read_opens_the_book_in_gideon_and_turns_pages_partially() {
        let dir = tempfile::tempdir().unwrap();
        let (lib, _) = novel(dir.path());
        let mut ui = app(&lib, FakeGateway::default(), open_first_book());
        ui.run().unwrap();
        let Screen::BookReader {
            text,
            chapter,
            pages,
            page,
            ..
        } = ui.screen()
        else {
            panic!("expected the reader, got {:?}", ui.screen());
        };
        assert_eq!(text.chapters.len(), 2);
        assert_eq!((*chapter, *page), (0, 0));
        assert!(
            pages.len() > 1,
            "sixty paragraphs span pages: {}",
            pages.len()
        );
        assert_eq!(pages[0][0].text, "One");
        assert!(pages[0][0].bold, "the heading is bold");
        assert!(pages[0].iter().any(|l| l.text.starts_with("Paragraph 0:")));
        // Opening is a push: Full. The page is text only, no chrome.
        assert_eq!(ui.display().flushes.last(), Some(&RefreshMode::Full));
        assert!(
            ui.display().pixel(5, 5) > 0xf0,
            "no title bar: the page is the book"
        );

        // Turn forward: Partial, no flash for a page of text.
        let mut events = open_first_book();
        events.push(tap_reader(ReaderZone::NextPage));
        let mut ui = app(&lib, FakeGateway::default(), events);
        ui.run().unwrap();
        let Screen::BookReader { page, .. } = ui.screen() else {
            panic!("expected the reader");
        };
        assert_eq!(*page, 1);
        assert_eq!(ui.display().flushes.last(), Some(&RefreshMode::Partial));
    }

    #[test]
    fn page_turn_buttons_and_the_remote_turn_book_pages() {
        let dir = tempfile::tempdir().unwrap();
        let (lib, _) = novel(dir.path());
        let mut events = open_first_book();
        events.extend([UiEvent::PageForward, UiEvent::RemoteNext, UiEvent::PageBack]);
        let mut ui = app(&lib, FakeGateway::default(), events);
        ui.run().unwrap();
        let Screen::BookReader { page, .. } = ui.screen() else {
            panic!("expected the reader");
        };
        assert_eq!(*page, 1);
    }

    #[test]
    fn turning_past_a_chapter_crosses_into_the_next_and_back() {
        let dir = tempfile::tempdir().unwrap();
        let (lib, _) = novel(dir.path());
        let mut ui = app(&lib, FakeGateway::default(), open_first_book());
        ui.run().unwrap();
        let Screen::BookReader { pages, .. } = ui.screen() else {
            panic!("expected the reader");
        };
        let n = pages.len();
        for _ in 0..n {
            ui.turn_book_page(1).unwrap();
        }
        let Screen::BookReader {
            chapter,
            page,
            pages,
            ..
        } = ui.screen()
        else {
            panic!("expected the reader");
        };
        assert_eq!(
            (*chapter, *page),
            (1, 0),
            "one past the last page is the next chapter"
        );
        assert_eq!(pages[0][0].text, "Two");
        // The end of the book is a wall, not a crash.
        ui.turn_book_page(1).unwrap();
        ui.turn_book_page(1).unwrap();
        let Screen::BookReader { chapter, .. } = ui.screen() else {
            panic!("expected the reader");
        };
        assert_eq!(*chapter, 1);
        // Back across the boundary lands on the previous chapter's last page.
        ui.turn_book_page(-1).unwrap();
        let Screen::BookReader {
            chapter,
            page,
            pages,
            ..
        } = ui.screen()
        else {
            panic!("expected the reader");
        };
        assert_eq!((*chapter, *page), (0, pages.len() - 1));
    }

    #[test]
    fn the_reader_flashes_every_n_turns_to_clear_ghosting() {
        let dir = tempfile::tempdir().unwrap();
        let (lib, _) = novel(dir.path());
        let mut ui = app(&lib, FakeGateway::default(), open_first_book());
        ui.run().unwrap();
        let before = ui.display().flushes.len();
        for _ in 0..8 {
            ui.turn_book_page(1).unwrap();
        }
        let turns = &ui.display().flushes[before..];
        assert_eq!(turns.len(), 8);
        assert!(turns[..7].iter().all(|m| *m == RefreshMode::Partial));
        assert_eq!(turns[7], RefreshMode::Full, "the eighth turn flashes");
    }

    #[test]
    fn the_place_is_kept_and_resumed_by_paragraph_across_a_text_size_change() {
        let dir = tempfile::tempdir().unwrap();
        let (lib, books) = novel(dir.path());
        let mut ui = app(&lib, FakeGateway::default(), open_first_book());
        ui.run().unwrap();
        ui.turn_book_page(1).unwrap();
        ui.turn_book_page(1).unwrap();
        let Screen::BookReader { pages, page, .. } = ui.screen() else {
            panic!("expected the reader");
        };
        let top_block = pages[*page][0].block;
        assert!(top_block > 0);
        let saved = gideon_core::BookPlaces::load(&books)
            .get("Kurt Vonnegut/book-0001.epub")
            .expect("place saved on every turn");
        assert_eq!((saved.chapter, saved.block), (0, top_block));

        // Close via the menu, reopen: same paragraph at the top.
        let mut events = open_first_book();
        events.extend([open_reading_menu(), tap_reading_row(6)]);
        let mut ui = app(&lib, FakeGateway::default(), events);
        ui.run().unwrap();
        assert!(
            matches!(ui.screen(), Screen::BookShelf { .. }),
            "Close book returns to the shelf"
        );

        // A settings dir, so the text size has somewhere to persist.
        let mut ui = app(&lib, FakeGateway::default(), open_first_book())
            .with_settings_dir(dir.path().join("data"));
        ui.run().unwrap();
        let Screen::BookReader { pages, page, .. } = ui.screen() else {
            panic!("expected the reader");
        };
        assert_eq!(
            pages[*page][0].block, top_block,
            "resumed at the paragraph, not page 1"
        );

        // Larger text (Text › Size): same paragraph stays on the page, the
        // sheet stays up, and the whole page re-flowed, so a Full.
        ui.sheet = Some(Sheet::BookReading);
        ui.tap_sheet(W / 2, reading_row_y(3)).unwrap();
        assert!(matches!(ui.sheet, Some(Sheet::BookText { .. })));
        let next = super::text_sheet::step_of(44) + 1;
        let (x, y) = slider_point(&ui, next);
        ui.tap_sheet(x, y).unwrap();
        assert!(
            matches!(ui.sheet, Some(Sheet::BookText { .. })),
            "sheet stays up"
        );
        let Screen::BookReader { pages, page, .. } = ui.screen() else {
            panic!("expected the reader");
        };
        assert!(pages[*page].iter().any(|l| l.block == top_block));
        assert_eq!(ui.load_settings().book_text_px, 48);
        assert_eq!(ui.display().flushes.last(), Some(&RefreshMode::Full));
    }

    /// The centre of a sheet control.
    fn centre(r: (u32, u32, u32, u32)) -> (u32, u32) {
        (r.0 + r.2 / 2, r.1 + r.3 / 2)
    }

    /// Where step `i` sits on the Text sheet's size slider.
    fn slider_point(ui: &UiApp<MemoryDisplay, FakeInput, FakeGateway>, i: usize) -> (u32, u32) {
        let t = ui.text_sheet_layout().track;
        let n = gideon_core::settings::BOOK_TEXT_PX_STEPS.len() as u32;
        (t.0 + t.2 * i as u32 / (n - 1), t.1 + t.3 / 2)
    }

    #[test]
    fn the_text_sheet_sets_font_size_spacing_and_margins_and_they_persist() {
        use gideon_core::settings::BOOK_TEXT_PX_STEPS;
        let dir = tempfile::tempdir().unwrap();
        let (lib, _) = novel(dir.path());
        let settings_dir = dir.path().join("data");
        let mut ui = app(&lib, FakeGateway::default(), open_first_book())
            .with_settings_dir(settings_dir.clone());
        ui.run().unwrap();
        let Screen::BookReader { pages, .. } = ui.screen() else {
            panic!("expected the reader");
        };
        let normal_lines = pages[0].len();
        let normal_width = ui.book_page_metrics().2;

        // The menu's Text row opens the sheet over the unchanged page: no flash.
        let UiEvent::Swipe { x0, y0, x1, y1 } = open_reading_menu() else {
            unreachable!()
        };
        ui.drag_menu(x0, y0, x1, y1, true).unwrap();
        assert!(matches!(ui.sheet, Some(Sheet::BookReading)));
        ui.handle_tap(W / 2, reading_row_y(3)).unwrap();
        assert_eq!(ui.sheet, Some(Sheet::BookText { preview: None }));
        assert_eq!(ui.display().flushes.last(), Some(&RefreshMode::Partial));
        let s = ui.text_sheet_layout();

        // Line spacing: Loose puts fewer lines on the page; a re-flow moves
        // every line, so Full; the sheet stays up.
        let (x, y) = centre(s.spacing[2]);
        ui.handle_tap(x, y).unwrap();
        assert_eq!(ui.load_settings().book_line_spacing, "loose");
        assert_eq!(ui.display().flushes.last(), Some(&RefreshMode::Full));
        assert!(matches!(ui.sheet, Some(Sheet::BookText { .. })));
        let Screen::BookReader { pages, .. } = ui.screen() else {
            panic!("expected the reader");
        };
        assert!(
            pages[0].len() < normal_lines,
            "{} < {normal_lines}",
            pages[0].len()
        );
        for (i, name) in [(0, "tight"), (1, "normal")] {
            let (x, y) = centre(s.spacing[i]);
            ui.handle_tap(x, y).unwrap();
            assert_eq!(ui.load_settings().book_line_spacing, name);
        }

        // Margins: Wide narrows the text column.
        let (x, y) = centre(s.margins[2]);
        ui.handle_tap(x, y).unwrap();
        assert_eq!(ui.load_settings().book_margins, "wide");
        assert!(ui.book_page_metrics().2 < normal_width);

        // Size: a tap anywhere on the track snaps to the nearest step.
        let last = BOOK_TEXT_PX_STEPS.len() - 1;
        let (x, y) = slider_point(&ui, last);
        ui.handle_tap(x, y).unwrap();
        assert_eq!(ui.load_settings().book_text_px, 80);
        let (x, y) = slider_point(&ui, 0);
        ui.handle_tap(x.saturating_sub(5), y).unwrap();
        assert_eq!(ui.load_settings().book_text_px, 28);

        // Sliding the knob: it follows the finger with the sheet alone
        // repainting (Partial), nothing saved and nothing re-flowed yet…
        let flushes = ui.display().flushes.len();
        let (sx, sy) = slider_point(&ui, 0);
        let (mx, _) = slider_point(&ui, 5);
        ui.drag_menu(sx, sy, mx, sy, false).unwrap();
        assert_eq!(ui.sheet, Some(Sheet::BookText { preview: Some(5) }));
        assert_eq!(ui.display().flushes.len(), flushes + 1);
        assert_eq!(ui.display().flushes.last(), Some(&RefreshMode::Partial));
        assert_eq!(ui.load_settings().book_text_px, 28);
        // …the same step again repaints nothing…
        ui.drag_menu(sx, sy, mx + 1, sy, false).unwrap();
        assert_eq!(ui.display().flushes.len(), flushes + 1);
        // …and the lift saves it and re-flows the page, Full.
        let (ex, _) = slider_point(&ui, 6);
        ui.drag_menu(sx, sy, ex, sy, true).unwrap();
        assert_eq!(ui.load_settings().book_text_px, BOOK_TEXT_PX_STEPS[6]);
        assert_eq!(ui.sheet, Some(Sheet::BookText { preview: None }));
        assert_eq!(ui.display().flushes.last(), Some(&RefreshMode::Full));
        // A slide on the open sheet never reaches the reader's gestures.
        assert!(matches!(ui.screen(), Screen::BookReader { .. }));

        // Font: the row opens the list in place of the sheet (Partial); a
        // face sets the page in it and comes back to the Text sheet (Full).
        let (x, y) = centre(s.font_row);
        ui.handle_tap(x, y).unwrap();
        assert_eq!(ui.sheet, Some(Sheet::BookFonts));
        assert_eq!(ui.display().flushes.last(), Some(&RefreshMode::Partial));
        let tiles = ui.font_sheet_layout().tiles;
        assert_eq!(tiles.len(), 12);
        let literata = FontFamily::ALL
            .iter()
            .position(|f| *f == FontFamily::Literata)
            .unwrap();
        let (x, y) = centre(tiles[literata]);
        ui.handle_tap(x, y).unwrap();
        assert_eq!(ui.load_settings().book_font, "literata");
        assert_eq!(ui.book_family(), FontFamily::Literata);
        assert_eq!(ui.sheet, Some(Sheet::BookText { preview: None }));
        assert_eq!(ui.display().flushes.last(), Some(&RefreshMode::Full));

        // Close (×): the page is revealed whole, so a flash.
        let (x, y) = centre(s.close);
        ui.handle_tap(x, y).unwrap();
        assert!(ui.sheet.is_none());
        assert_eq!(ui.display().flushes.last(), Some(&RefreshMode::Full));

        // Everything survives a restart, on the device's settings.json.
        let saved = gideon_core::Settings::load(&settings_dir).unwrap();
        assert_eq!(
            (
                saved.book_text_px,
                saved.book_line_spacing.as_str(),
                saved.book_margins.as_str(),
                saved.book_font.as_str()
            ),
            (BOOK_TEXT_PX_STEPS[6], "normal", "wide", "literata")
        );
        let mut again =
            app(&lib, FakeGateway::default(), open_first_book()).with_settings_dir(settings_dir);
        again.run().unwrap();
        assert_eq!(again.book_page_metrics().0, BOOK_TEXT_PX_STEPS[6] as f32);
        assert_eq!(again.book_family(), FontFamily::Literata);
    }

    #[test]
    fn a_page_set_in_another_face_draws_different_ink() {
        let dir = tempfile::tempdir().unwrap();
        let (lib, _) = novel(dir.path());
        let settings_dir = dir.path().join("data");
        let mut ui = app(&lib, FakeGateway::default(), open_first_book())
            .with_settings_dir(settings_dir.clone());
        ui.run().unwrap();
        let sans = ui.display().buffer.clone();
        let mut st = ui.load_settings();
        st.book_font = "garamond".into();
        ui.save_settings(&st);
        ui.reflow_book().unwrap();
        assert_ne!(ui.display().buffer, sans, "the page re-set in Garamond");
    }

    #[test]
    fn a_pdf_offers_only_the_kobo_reader() {
        let dir = tempfile::tempdir().unwrap();
        let lib = dir.path().join("Manga");
        make_cbz(&lib.join("Series/vol1.cbz"), 2);
        let books = lib.join("Books");
        std::fs::create_dir_all(books.join("Someone")).unwrap();
        std::fs::write(books.join("Someone/paper.pdf"), b"%PDF-1.4").unwrap();
        let mut ui = app(
            &lib,
            FakeGateway::default(),
            vec![tap_nav(0), tap_books_tab(), tap_row(0)],
        );
        ui.run().unwrap();
        let (_, rows) = ui.sheet_rows();
        let labels: Vec<&str> = rows.iter().map(|r| r.0.as_str()).collect();
        assert_eq!(
            labels,
            [
                "Read in the Kobo reader",
                "Mark as finished",
                "Delete this book",
                "Close"
            ]
        );
    }

    #[test]
    fn the_book_reader_has_the_lamp_edge_slides_like_a_kindle() {
        let dir = tempfile::tempdir().unwrap();
        let (lib, _) = novel(dir.path());
        let (levels, log, lights) = lights_recording();
        let drag_to = |y1: u32| UiEvent::Drag {
            x0: W - 5,
            y0: H - 100,
            x1: W - 5,
            y1,
        };
        let mut events = open_first_book();
        events.extend([
            // Up the right edge in two reports, then lift: 20 → 45 → 70, and
            // the release at the same spot does not add again.
            drag_to(H - 100 - H / 4),
            drag_to(H - 100 - H / 2),
            UiEvent::Swipe {
                x0: W - 5,
                y0: H - 100,
                x1: W - 5,
                y1: H - 100 - H / 2,
            },
            // Up the left edge: night light.
            UiEvent::Swipe {
                x0: 5,
                y0: H - 100,
                x1: 5,
                y1: H - 100 - H / 4,
            },
            // A short mid-screen drag (under a quarter of the page, the
            // manga reader's threshold for leave/rotate) is nothing.
            UiEvent::Swipe {
                x0: W / 2,
                y0: H / 2,
                x1: W / 2,
                y1: H / 2 - H / 8,
            },
        ]);
        let mut ui = app(&lib, FakeGateway::default(), events).with_lights(lights);
        ui.run().unwrap();
        assert_eq!(*levels.borrow(), (70, 25));
        assert_eq!(
            *log.borrow(),
            vec![Lamp::Brightness(45), Lamp::Brightness(70), Lamp::Warmth(25)],
            "the lamp followed the finger and the lift added nothing"
        );
        let Screen::BookReader { page, .. } = ui.screen() else {
            panic!("still reading");
        };
        assert_eq!(*page, 0, "slides and swipes do not turn pages");
        // Each change repainted the page with its banner, without a flash.
        assert_eq!(ui.display().flushes.last(), Some(&RefreshMode::Partial));
    }

    #[test]
    fn an_epub_illustration_is_drawn_in_place_scaled_to_the_text_column() {
        let dir = tempfile::tempdir().unwrap();
        let lib = dir.path().join("Manga");
        make_cbz(&lib.join("Series/vol1.cbz"), 2);
        // Wider than the column and squat: scaled down to the column width.
        make_illustrated_epub(&lib.join("Books/A/pic.epub"), 4000, 1000);
        let mut ui = app(&lib, FakeGateway::default(), open_first_book());
        ui.run().unwrap();
        let Screen::BookReader { pages, .. } = ui.screen() else {
            panic!("expected the reader, got {:?}", ui.screen());
        };
        let page0 = &pages[0];
        let img_line = page0
            .iter()
            .find(|l| l.image.is_some())
            .expect("the picture is a line on the first page");
        let img = img_line.image.as_ref().unwrap();
        let (_, margin, max_w, top, line_h, _) = ui.book_page_metrics();
        assert_eq!(img.width, max_w, "fits the text column");
        assert_eq!(img.height, max_w / 4, "aspect kept");
        assert_eq!(img.rows, (img.height as usize).div_ceil(line_h as usize));
        assert!(
            !page0.iter().any(|l| l.text == "[Illustration]"),
            "no placeholder when the picture can be drawn"
        );
        assert_eq!(page0[0].text, "Before the picture.");
        assert!(page0.iter().any(|l| l.text == "After the picture."));

        // Its pixels are on the panel where the layout put it: rows above
        // the image line count one line each (the paragraph, then the blank
        // separator).
        let rows_before = page0.iter().take_while(|l| l.image.is_none()).count() as u32;
        let y = top + rows_before * line_h + img.height / 2;
        let x = margin + max_w / 2;
        assert!(
            ui.display().pixel(x, y) < 0x60,
            "dark picture pixel at ({x},{y})"
        );
        assert!(
            ui.display().pixel(x, top + 2) > 0xf0,
            "text row above stays white"
        );
    }

    #[test]
    fn a_tall_illustration_takes_its_own_page_rather_than_being_split() {
        let dir = tempfile::tempdir().unwrap();
        let lib = dir.path().join("Manga");
        make_cbz(&lib.join("Series/vol1.cbz"), 2);
        // Taller than a page: capped to the page height, placed after the
        // paragraph page, alone.
        make_illustrated_epub(&lib.join("Books/A/tall.epub"), 600, 6000);
        let mut ui = app(&lib, FakeGateway::default(), open_first_book());
        ui.run().unwrap();
        let Screen::BookReader { pages, .. } = ui.screen() else {
            panic!("expected the reader");
        };
        let (_, _, _, _, line_h, lines) = ui.book_page_metrics();
        assert!(pages.len() >= 2);
        assert!(
            pages[0].iter().all(|l| l.image.is_none()),
            "page 1 is the text before"
        );
        let img = pages[1][0]
            .image
            .as_ref()
            .expect("page 2 opens with the picture");
        assert_eq!(img.height, lines as u32 * line_h, "capped at a page");
        assert_eq!(img.rows, lines);
        assert_eq!(pages[1].len(), 1, "nothing shares the page with it");
    }

    #[test]
    fn emphasis_is_kept_as_styled_runs_and_drawn_leaning() {
        let dir = tempfile::tempdir().unwrap();
        let lib = dir.path().join("Manga");
        make_cbz(&lib.join("Series/vol1.cbz"), 2);
        make_epub_with(
            &lib.join("Books/A/em.epub"),
            "Em",
            "A",
            &[("One", &["So <i>it</i> <b>goes</b>."])],
        );
        let mut ui = app(&lib, FakeGateway::default(), open_first_book());
        ui.run().unwrap();
        let Screen::BookReader { pages, .. } = ui.screen() else {
            panic!("expected the reader, got {:?}", ui.screen());
        };
        let line = pages[0]
            .iter()
            .find(|l| l.text == "So it goes.")
            .expect("the paragraph is one line, styling dropped from `text`");
        let runs: Vec<(&str, bool, bool)> = line
            .spans
            .iter()
            .map(|s| (s.text.as_str(), s.italic, s.bold))
            .collect();
        assert_eq!(
            runs,
            [
                ("So ", false, false),
                ("it ", true, false),
                ("goes", false, true),
                (".", false, false),
            ]
        );
        // A heading's runs are all bold, whatever the markup said.
        assert!(pages[0][0].bold && pages[0][0].spans.iter().all(|s| s.bold));

        // The drawn page differs from the same text drawn plain: the lean and
        // the weight are on the panel, not only in the data.
        let styled = ui.display().buffer.clone();
        make_epub_with(
            &lib.join("Books/A/em.epub"),
            "Em",
            "A",
            &[("One", &["So it goes."])],
        );
        let mut plain = app(&lib, FakeGateway::default(), open_first_book());
        plain.run().unwrap();
        assert_ne!(styled, plain.display().buffer);
    }

    #[test]
    fn contents_lists_the_chapters_and_a_tap_jumps_there() {
        let dir = tempfile::tempdir().unwrap();
        let (lib, books) = novel(dir.path());
        let mut events = open_first_book();
        events.extend([open_reading_menu(), tap_reading_row(0)]);
        let mut ui = app(&lib, FakeGateway::default(), events);
        ui.run().unwrap();
        let Screen::BookContents {
            entries, current, ..
        } = ui.screen()
        else {
            panic!("expected the contents, got {:?}", ui.screen());
        };
        let titles: Vec<&str> = entries.iter().map(|e| e.title.as_str()).collect();
        assert_eq!(
            titles,
            ["One", "Two"],
            "no nav in the fixture: headings serve"
        );
        assert_eq!(*current, 0);
        // Pushed as a list: Full, with a Back button (it is a child screen).
        assert_eq!(ui.display().flushes.last(), Some(&RefreshMode::Full));

        let mut events = open_first_book();
        events.extend([open_reading_menu(), tap_reading_row(0), tap_row(1)]);
        let mut ui = app(&lib, FakeGateway::default(), events);
        ui.run().unwrap();
        let Screen::BookReader {
            chapter,
            page,
            pages,
            ..
        } = ui.screen()
        else {
            panic!("expected the reader back, got {:?}", ui.screen());
        };
        assert_eq!((*chapter, *page), (1, 0));
        assert_eq!(pages[0][0].text, "Two");
        assert_eq!(ui.display().flushes.last(), Some(&RefreshMode::Full));
        let saved = gideon_core::BookPlaces::load(&books)
            .get("Kurt Vonnegut/book-0001.epub")
            .unwrap();
        assert_eq!(saved.chapter, 1, "the jump is the new place");
    }

    #[test]
    fn today_continues_a_book_and_lists_unstarted_books_as_waiting() {
        let dir = tempfile::tempdir().unwrap();
        let (lib, books) = novel(dir.path());
        make_epub(
            &books.join("Natsu Hyuuga/book-0002.epub"),
            "The Apothecary Diaries, Vol. 1",
            "Natsu Hyuuga",
        );
        // Read two pages of the novel, close it, go to Today.
        let mut events = open_first_book();
        events.extend([
            tap_reader(ReaderZone::NextPage),
            tap_reader(ReaderZone::NextPage),
            open_reading_menu(),
            tap_reading_row(6),
            tap_today(),
        ]);
        let mut ui = app(&lib, FakeGateway::default(), events);
        ui.run().unwrap();
        assert!(matches!(ui.screen(), Screen::Stats));
        let (title, where_at, pct) = ui.continue_card().expect("a book was read");
        assert_eq!(title, "Slaughterhouse-Five");
        assert_eq!(where_at, "Kurt Vonnegut — chapter 1 of 2");
        assert_eq!(pct, 0.0, "still in the first chapter");

        // Waiting: the unread manga and both books (neither finished),
        // the one being read first.
        let titles: Vec<String> = ui.waiting_rows().iter().map(|r| r.title()).collect();
        assert_eq!(
            titles,
            [
                "Slaughterhouse-Five",
                "Series",
                "The Apothecary Diaries, Vol. 1"
            ]
        );

        // Tapping Continue opens the reader at the saved page.
        let l = layout();
        let top = ui.continue_card_top();
        ui.handle_tap(l.width / 2, top + l.row_h / 2).unwrap();
        let Screen::BookReader { book, page, .. } = ui.screen() else {
            panic!("expected the reader, got {:?}", ui.screen());
        };
        assert_eq!(book.title, "Slaughterhouse-Five");
        assert_eq!(*page, 2);

        // A manga read more recently than the book wins the card back.
        let now = now_unix();
        write_progress(&lib, &[("Series/vol1.cbz", 1, 2, now + 5)]);
        ui.invalidate_progress_cache();
        let (title, _, _) = ui.continue_card().unwrap();
        assert_eq!(title, "Series");
    }

    #[test]
    fn a_long_press_bookmarks_the_page_and_the_list_jumps_back_to_it() {
        let dir = tempfile::tempdir().unwrap();
        let (lib, books) = novel(dir.path());
        let l = layout();
        // Held on the margin: a word would be looked up instead.
        let hold = UiEvent::LongPress {
            x: 2,
            y: l.height / 2,
        };
        // Page 3, hold: bookmarked, with a dog-ear and a banner, partially.
        let mut events = open_first_book();
        events.extend([
            tap_reader(ReaderZone::NextPage),
            tap_reader(ReaderZone::NextPage),
            hold,
        ]);
        let mut ui = app(&lib, FakeGateway::default(), events);
        ui.run().unwrap();
        let marks = gideon_core::BookPlaces::load(&books);
        let marks = marks.bookmarks("Kurt Vonnegut/book-0001.epub");
        assert_eq!(marks.len(), 1);
        assert_eq!(marks[0].chapter, 0);
        let Screen::BookReader { pages, page, .. } = ui.screen() else {
            panic!("still reading");
        };
        assert_eq!(*page, 2);
        // The snippet is the page's first words, even mid-paragraph.
        assert_eq!(
            marks[0].snippet,
            pages[2][0].text.chars().take(48).collect::<String>()
        );
        assert_eq!(marks[0].block, pages[2][0].block);
        assert_eq!(ui.display().flushes.last(), Some(&RefreshMode::Partial));
        assert!(ui.display().pixel(W - 2, 1) < 0x40, "dog-ear in the corner");
        assert!(ui.display().pixel(1, 1) > 0xf0, "only the corner");

        // Hold again: gone, corner white.
        ui.handle_long_press(2, l.height / 2).unwrap();
        assert!(gideon_core::BookPlaces::load(&books)
            .bookmarks("Kurt Vonnegut/book-0001.epub")
            .is_empty());
        assert!(ui.display().pixel(W - 2, 1) > 0xf0);

        // Mark page 3 again, go to page 1, open Bookmarks, tap the mark:
        // back on page 3.
        ui.handle_long_press(2, l.height / 2).unwrap();
        ui.turn_book_page(-1).unwrap();
        ui.turn_book_page(-1).unwrap();
        ui.sheet = Some(Sheet::BookReading);
        ui.tap_sheet(W / 2, reading_row_y(1)).unwrap();
        let Screen::BookBookmarks { entries, .. } = ui.screen() else {
            panic!("expected the bookmarks, got {:?}", ui.screen());
        };
        assert_eq!(entries.len(), 1);
        ui.handle_tap(W / 2, l.row_top(0) + l.row_h / 2).unwrap();
        let Screen::BookReader { page, .. } = ui.screen() else {
            panic!("expected the reader back");
        };
        assert_eq!(*page, 2);
        assert_eq!(ui.display().flushes.last(), Some(&RefreshMode::Full));

        // A book with no marks: the menu says how, instead of an empty list.
        let other = tempfile::tempdir().unwrap();
        let (lib2, _) = novel(other.path());
        let mut fresh = app(&lib2, FakeGateway::default(), open_first_book());
        fresh.run().unwrap();
        fresh.sheet = Some(Sheet::BookReading);
        fresh.tap_sheet(W / 2, reading_row_y(1)).unwrap();
        assert!(matches!(fresh.screen(), Screen::Message { .. }));
    }

    #[test]
    fn word_at_x_finds_the_word_under_a_press() {
        let spans = vec![
            BookSpan {
                text: "So it ".into(),
                italic: false,
                bold: false,
            },
            BookSpan {
                text: "goes".into(),
                italic: true,
                bold: false,
            },
            BookSpan {
                text: ", Billy.".into(),
                italic: false,
                bold: false,
            },
        ];
        let px = 44.0;
        let so = measure_text(px, "So", false);
        let so_it = measure_text(px, "So it", false);
        let plain = measure_text(px, "So it ", false);
        let goes = measure_text(px, "goes", false);
        assert_eq!(
            word_at_x(px, FontFamily::Sans, &spans, so / 2).as_deref(),
            Some("So")
        );
        assert_eq!(
            word_at_x(px, FontFamily::Sans, &spans, so_it - 2).as_deref(),
            Some("it")
        );
        assert_eq!(
            word_at_x(px, FontFamily::Sans, &spans, plain + goes / 2).as_deref(),
            Some("goes")
        );
        assert_eq!(
            word_at_x(px, FontFamily::Sans, &spans, plain + goes + 4).as_deref(),
            Some(",")
        );
        let billy_at = plain + goes + measure_text(px, ", ", false) + 4;
        assert_eq!(
            word_at_x(px, FontFamily::Sans, &spans, billy_at).as_deref(),
            Some("Billy.")
        );
        assert_eq!(
            word_at_x(px, FontFamily::Sans, &spans, so + 1),
            None,
            "the space between words"
        );
        assert_eq!(
            word_at_x(px, FontFamily::Sans, &spans, 5000),
            None,
            "past the end of the line"
        );
    }

    #[test]
    fn a_long_press_on_a_word_shows_its_definition_partially() {
        let dir = tempfile::tempdir().unwrap();
        let (lib, books) = novel(dir.path());
        let data = dir.path().join("data");
        write_stardict(
            &data.join("dicts/words"),
            "words",
            &[
                ("pilgrim", "n. someone who journeys to a sacred place\nn. one of the English colonists"),
                ("time", "n. the continuum of experience in which events pass from the future through the present to the past"),
            ],
        );
        let mut ui = app(&lib, FakeGateway::default(), open_first_book()).with_settings_dir(data);
        ui.run().unwrap();
        let (px, margin, _, top, line_h, _) = ui.book_page_metrics();
        // "Paragraph 0: Billy Pilgrim has come unstuck in time, ..." is the
        // first paragraph line under the heading; press on "Pilgrim".
        let (row, first) = match ui.screen() {
            Screen::BookReader { pages, .. } => pages[0]
                .iter()
                .enumerate()
                .find(|(_, l)| l.text.contains("Pilgrim"))
                .map(|(i, l)| (i as u32, l.text.clone()))
                .expect("the fixture's first paragraph"),
            other => panic!("expected the reader, got {other:?}"),
        };
        let at = first.find("Pilgrim").unwrap();
        let x = margin + measure_text(px, &first[..at], false) + measure_text(px, "Pil", false);
        let y = top + row * line_h + line_h / 2;
        assert_eq!(ui.book_word_at(x, y).as_deref(), Some("Pilgrim"));

        ui.handle_long_press(x, y).unwrap();
        let Some(Sheet::Definition { word, lines }) = &ui.sheet else {
            panic!("expected the definition, got {:?}", ui.sheet);
        };
        assert_eq!(word, "pilgrim", "cased as the dictionary spells it");
        assert!(lines[0].starts_with("n. someone who journeys"), "{lines:?}");
        assert!(lines.iter().any(|l| l.contains("English colonists")));
        // The page behind has not moved: the sheet alone repaints.
        assert_eq!(ui.display().flushes.last(), Some(&RefreshMode::Partial));
        assert!(
            gideon_core::BookPlaces::load(&books)
                .bookmarks("Kurt Vonnegut/book-0001.epub")
                .is_empty(),
            "a lookup is not a bookmark"
        );
        // The sheet's rows read as the definition, then Close.
        let (title, rows) = ui.sheet_rows();
        assert_eq!(title, "pilgrim");
        assert!(rows.last().unwrap().2, "last row dismisses");
        // Tapping a definition line does nothing; Close reveals the page: Full.
        let n = rows.len() as u32;
        ui.tap_sheet(W / 2, tap_modal_row_y(n, 0)).unwrap();
        assert!(matches!(ui.sheet, Some(Sheet::Definition { .. })));
        ui.tap_sheet(W / 2, tap_modal_row_y(n, rows.len() - 1))
            .unwrap();
        assert!(ui.sheet.is_none());
        assert_eq!(ui.display().flushes.last(), Some(&RefreshMode::Full));

        // A word the dictionary lacks: a banner, no sheet, no bookmark.
        let at = first.find("Billy").unwrap();
        let x = margin + measure_text(px, &first[..at], false) + measure_text(px, "Bil", false);
        ui.handle_long_press(x, y).unwrap();
        assert!(ui.sheet.is_none());
        assert_eq!(ui.display().flushes.last(), Some(&RefreshMode::Partial));
        assert!(gideon_core::BookPlaces::load(&books)
            .bookmarks("Kurt Vonnegut/book-0001.epub")
            .is_empty());

        // The margin still bookmarks.
        ui.handle_long_press(2, y).unwrap();
        assert_eq!(
            gideon_core::BookPlaces::load(&books)
                .bookmarks("Kurt Vonnegut/book-0001.epub")
                .len(),
            1
        );
    }

    #[test]
    fn without_a_dictionary_a_pressed_word_offers_the_download_which_installs_it() {
        let dir = tempfile::tempdir().unwrap();
        let (lib, _) = novel(dir.path());
        let data = dir.path().join("data");
        // The download the fake web serves: a zip of a small dictionary.
        let src = dir.path().join("src");
        write_stardict(&src, "wordnet", &[("time", "n. what clocks measure")]);
        let mut buf = std::io::Cursor::new(Vec::new());
        {
            let mut w = zip::ZipWriter::new(&mut buf);
            let opts = zip::write::SimpleFileOptions::default();
            for f in ["wordnet.ifo", "wordnet.idx", "wordnet.dict"] {
                w.start_file(format!("wordnet/{f}"), opts).unwrap();
                w.write_all(&std::fs::read(src.join(f)).unwrap()).unwrap();
            }
            w.finish().unwrap();
        }
        let gateway = FakeGateway {
            web: gideon_sources::FakeFetcher::new().with(DICTIONARY_URL, buf.into_inner()),
            ..Default::default()
        };
        let mut ui = app(&lib, gateway, open_first_book()).with_settings_dir(data.clone());
        ui.run().unwrap();
        let (px, margin, _, top, line_h, _) = ui.book_page_metrics();
        let (row, first) = match ui.screen() {
            Screen::BookReader { pages, .. } => pages[0]
                .iter()
                .enumerate()
                .find(|(_, l)| l.text.contains("time"))
                .map(|(i, l)| (i as u32, l.text.clone()))
                .expect("the fixture's first paragraph"),
            other => panic!("expected the reader, got {other:?}"),
        };
        let at = first.find("time").unwrap();
        let x = margin + measure_text(px, &first[..at], false) + measure_text(px, "ti", false);
        let y = top + row * line_h + line_h / 2;

        ui.handle_long_press(x, y).unwrap();
        assert!(
            matches!(ui.sheet, Some(Sheet::NoDictionary)),
            "{:?}",
            ui.sheet
        );
        assert_eq!(ui.display().flushes.last(), Some(&RefreshMode::Partial));
        let (_, rows) = ui.sheet_rows();
        assert_eq!(rows[1].0, "Download English dictionary");

        ui.tap_sheet(W / 2, tap_modal_row_y(3, 1)).unwrap();
        let Screen::Message { title, body } = ui.screen() else {
            panic!("expected the installed message, got {:?}", ui.screen());
        };
        assert_eq!(title, "Dictionary");
        assert!(body.starts_with("Installed WordNet"), "{body}");
        assert!(data.join("dicts/wordnet/wordnet.ifo").exists());

        // Back to the book: the same press now defines.
        ui.pop().unwrap();
        ui.handle_long_press(x, y).unwrap();
        let Some(Sheet::Definition { word, lines }) = &ui.sheet else {
            panic!("expected the definition, got {:?}", ui.sheet);
        };
        assert_eq!(
            (word.as_str(), lines[0].as_str()),
            ("time", "n. what clocks measure")
        );
    }

    #[test]
    fn the_reading_menu_marks_and_unmarks_the_page() {
        let dir = tempfile::tempdir().unwrap();
        let (lib, books) = novel(dir.path());
        let mut ui = app(&lib, FakeGateway::default(), open_first_book());
        ui.run().unwrap();
        ui.sheet = Some(Sheet::BookReading);
        assert_eq!(ui.sheet_rows().1[2].0, "Mark this page");
        ui.tap_sheet(W / 2, reading_row_y(2)).unwrap();
        assert!(ui.sheet.is_none());
        assert_eq!(
            gideon_core::BookPlaces::load(&books)
                .bookmarks("Kurt Vonnegut/book-0001.epub")
                .len(),
            1
        );
        // The sheet closing reveals the page: a flash, dog-ear included.
        assert_eq!(ui.display().flushes.last(), Some(&RefreshMode::Full));
        assert!(ui.display().pixel(W - 2, 1) < 0x40);
        ui.sheet = Some(Sheet::BookReading);
        assert_eq!(ui.sheet_rows().1[2].0, "Unmark this page");
        ui.tap_sheet(W / 2, reading_row_y(2)).unwrap();
        assert!(gideon_core::BookPlaces::load(&books)
            .bookmarks("Kurt Vonnegut/book-0001.epub")
            .is_empty());
    }

    /// Deliver a tap event to a running app.
    fn tap(ui: &mut UiApp<MemoryDisplay, FakeInput, FakeGateway>, ev: UiEvent) {
        let UiEvent::Tap { x, y } = ev else {
            panic!("not a tap: {ev:?}");
        };
        ui.handle_tap(x, y).unwrap();
    }

    /// Tap the middle of shelf cell `i` on a Library grid.
    fn tap_shelf_cell(i: usize) -> UiEvent {
        let l = layout();
        let shelf = ShelfLayout::new(l.width, l.content_height(), SHELF_COLUMNS);
        let (cx, cy) = shelf.cell_origin(i);
        UiEvent::Tap {
            x: cx + shelf.cell_width() / 2,
            y: l.content_top() + cy + shelf.cell_height() / 2,
        }
    }

    #[test]
    fn the_books_shelf_view_shows_covers_recent_first_and_a_cell_opens_the_book() {
        let dir = tempfile::tempdir().unwrap();
        let lib = dir.path().join("Manga");
        make_cbz(&lib.join("Series/vol1.cbz"), 2);
        let books = lib.join("Books");
        make_epub_with_cover(&books.join("A/book-0001.epub"), "Arrival", "A", 20);
        make_epub(&books.join("B/book-0002.epub"), "Borrowed Time", "B");
        // Borrowed Time has been read a little: chapter 1 of 4.
        let mut places = gideon_core::BookPlaces::default();
        places.set(
            "B/book-0002.epub",
            gideon_core::BookPlace {
                chapter: 1,
                block: 0,
                line: 0,
                chapters: 4,
                updated_at: 100,
                ..Default::default()
            },
        );
        places.save(&books).unwrap();
        let settings_dir = dir.path().join("data");
        let settings = gideon_core::Settings {
            library_view: "shelf".into(),
            ..Default::default()
        };
        settings.save(&settings_dir).unwrap();

        let mut ui = app(
            &lib,
            FakeGateway::default(),
            vec![tap_nav(0), tap_books_tab()],
        )
        .with_settings_dir(settings_dir);
        ui.run().unwrap();
        let Screen::BookShelf { books: shown, .. } = ui.screen() else {
            panic!("expected the Books tab, got {:?}", ui.screen());
        };
        // Cards: the read book first, then the unread by title.
        let cards = ui.book_cards(shown);
        let titles: Vec<&str> = cards.iter().map(|c| c.title.as_str()).collect();
        assert_eq!(titles, ["Borrowed Time", "Arrival"]);
        assert_eq!(cards[0].progress, Some(0.25));
        assert_eq!(cards[1].progress, None);
        assert_eq!(ui.display().flushes.last(), Some(&RefreshMode::Full));

        // The grid: a plain card where there is no cover, the dark cover
        // where there is one, decoded once into a dotfile beside the book.
        let l = layout();
        let shelf = ShelfLayout::new(l.width, l.content_height(), SHELF_COLUMNS);
        let probe = |cell: usize| {
            let (cx, cy) = shelf.cell_origin(cell);
            ui.display().pixel(
                cx + shelf.cell_width() / 2,
                l.content_top() + cy + shelf.cell_width() * 2 / 3,
            )
        };
        assert!(probe(0) > 0xc0, "no cover: a plain card ({:#x})", probe(0));
        assert!(probe(1) < 0x60, "the cover's shade ({:#x})", probe(1));
        assert!(books.join("A/.book-0001.cover.jpg").exists());

        // A cell opens the book's sheet over the unmoved shelf.
        tap(&mut ui, tap_shelf_cell(0));
        let Some(Sheet::BookFile { book }) = &ui.sheet else {
            panic!("expected the book sheet, got {:?}", ui.sheet);
        };
        assert_eq!(book.title, "Borrowed Time");
        assert_eq!(ui.display().flushes.last(), Some(&RefreshMode::Partial));
    }

    #[test]
    fn volumes_of_a_series_share_one_card_that_opens_the_volume_list() {
        let dir = tempfile::tempdir().unwrap();
        let lib = dir.path().join("Manga");
        make_cbz(&lib.join("Series/vol1.cbz"), 2);
        let books = lib.join("Books");
        for (n, file) in [(1, "x-1"), (3, "x-3"), (2, "x-2")] {
            make_epub(
                &books.join(format!("Natsu Hyuuga/{file}.epub")),
                &format!("The Apothecary Diaries, Vol. {n}"),
                "Natsu Hyuuga",
            );
        }
        make_epub(
            &books.join("Kurt Vonnegut/sh5.epub"),
            "Slaughterhouse-Five",
            "Kurt Vonnegut",
        );
        let mut ui = app(
            &lib,
            FakeGateway::default(),
            vec![tap_nav(0), tap_books_tab()],
        );
        ui.run().unwrap();
        let Screen::BookShelf { books: shown, .. } = ui.screen() else {
            panic!("expected the Books tab");
        };
        let cards = ui.book_cards(shown);
        let titles: Vec<&str> = cards.iter().map(|c| c.title.as_str()).collect();
        assert_eq!(
            titles,
            ["Slaughterhouse-Five", "The Apothecary Diaries, Vol. 1–3"]
        );
        assert_eq!(cards[1].books.len(), 3);

        // The series row opens its volumes, in number order, as a list.
        tap(&mut ui, tap_row(1));
        let Screen::BookSeries {
            title,
            books: volumes,
            ..
        } = ui.screen()
        else {
            panic!("expected the volume list, got {:?}", ui.screen());
        };
        assert_eq!(title, "The Apothecary Diaries, Vol. 1–3");
        let vols: Vec<&str> = volumes.iter().map(|b| b.title.as_str()).collect();
        assert_eq!(
            vols,
            [
                "The Apothecary Diaries, Vol. 1",
                "The Apothecary Diaries, Vol. 2",
                "The Apothecary Diaries, Vol. 3",
            ]
        );
        assert_eq!(ui.display().flushes.last(), Some(&RefreshMode::Full));
        // A volume raises the same sheet a lone book does.
        tap(&mut ui, tap_row(1));
        let Some(Sheet::BookFile { book }) = &ui.sheet else {
            panic!("expected the book sheet, got {:?}", ui.sheet);
        };
        assert_eq!(book.title, "The Apothecary Diaries, Vol. 2");
        assert_eq!(ui.display().flushes.last(), Some(&RefreshMode::Partial));
    }

    #[test]
    fn the_title_bar_toggles_the_books_view_too() {
        let dir = tempfile::tempdir().unwrap();
        let (lib, _) = shelf(dir.path());
        let l = layout();
        let [_, books] = library_tab_zones(&l);
        let toggle = UiEvent::Tap {
            x: books.1 + l.pad * 2,
            y: l.title_h / 2,
        };
        let mut ui = app(
            &lib,
            FakeGateway::default(),
            vec![tap_nav(0), tap_books_tab(), toggle],
        );
        ui.run().unwrap();
        assert!(matches!(ui.screen(), Screen::BookShelf { .. }));
        assert_eq!(ui.load_settings().library_view, "shelf");
        // Whole content area replaced: a flash.
        assert_eq!(ui.display().flushes.last(), Some(&RefreshMode::Full));
    }

    #[test]
    fn the_footer_estimates_time_left_once_the_pace_is_known() {
        let dir = tempfile::tempdir().unwrap();
        let (lib, _) = novel(dir.path());
        let now = std::rc::Rc::new(std::cell::Cell::new(1_000_000u64));
        let clock = now.clone();
        let mut ui =
            app(&lib, FakeGateway::default(), open_first_book()).with_clock(move || clock.get());
        ui.run().unwrap();
        let left = |ui: &UiApp<MemoryDisplay, FakeInput, FakeGateway>| {
            let Screen::BookReader {
                text,
                chapter,
                pages,
                page,
                ..
            } = ui.screen()
            else {
                panic!("expected the reader");
            };
            ui.time_left_text(text, *chapter, pages, *page)
        };
        assert_eq!(left(&ui), None, "no pace yet");
        assert_eq!(ui.book_secs_per_page(), None);
        // Four turns half a minute apart: three gaps, a pace.
        for _ in 0..4 {
            now.set(now.get() + 30);
            ui.turn_book_page(1).unwrap();
        }
        assert_eq!(ui.book_secs_per_page(), Some(30.0));
        let text = left(&ui).expect("a pace");
        assert!(text.ends_with(" in book"), "{text}");
        assert!(text.contains(" left in chapter · "), "{text}");
        // A long paragraph-heavy chapter at 30 s a page reads in minutes.
        assert!(text.starts_with(|c: char| c.is_ascii_digit()), "{text}");
        assert!(text.contains("min left in chapter"), "{text}");
        // A put-down (ten minutes) does not drag the pace out.
        now.set(now.get() + 600);
        ui.turn_book_page(1).unwrap();
        assert_eq!(ui.book_secs_per_page(), Some(30.0));
        // The page's last line of the footer is the estimate, not the title.
        assert_eq!(duration_words(20.0), "under a minute");
        assert_eq!(duration_words(12.0 * 60.0), "12 min");
        assert_eq!(duration_words(65.0 * 60.0), "1 h 05 min");
        assert_eq!(duration_words(4.0 * 3600.0), "4 h");
    }

    #[test]
    fn night_mode_inverts_the_page_with_a_flash_and_persists() {
        let dir = tempfile::tempdir().unwrap();
        let (lib, _) = novel(dir.path());
        let settings_dir = dir.path().join("data");
        let mut ui = app(&lib, FakeGateway::default(), open_first_book())
            .with_settings_dir(settings_dir.clone());
        ui.run().unwrap();
        let (_, margin, _, top, _, _) = ui.book_page_metrics();
        // Paper: the margin is white.
        assert!(ui.display().pixel(margin / 2, top) > 0xf0);
        ui.sheet = Some(Sheet::BookReading);
        assert_eq!(ui.sheet_rows().1[4].1, "Off");
        ui.tap_sheet(W / 2, reading_row_y(4)).unwrap();
        assert!(ui.sheet.is_none());
        assert!(
            ui.display().pixel(margin / 2, top) < 0x10,
            "night: black margin"
        );
        assert_eq!(ui.display().flushes.last(), Some(&RefreshMode::Full));
        assert!(ui.load_settings().book_night);
        // A page turn stays partial, as on paper.
        ui.turn_book_page(1).unwrap();
        assert_eq!(ui.display().flushes.last(), Some(&RefreshMode::Partial));
        assert!(ui.display().pixel(margin / 2, top) < 0x10);
        // Survives a restart.
        let mut again =
            app(&lib, FakeGateway::default(), open_first_book()).with_settings_dir(settings_dir);
        again.run().unwrap();
        assert!(again.display().pixel(margin / 2, top) < 0x10);
        again.sheet = Some(Sheet::BookReading);
        assert_eq!(again.sheet_rows().1[4].1, "On");
        again.tap_sheet(W / 2, reading_row_y(4)).unwrap();
        assert!(again.display().pixel(margin / 2, top) > 0xf0);
        assert!(!again.load_settings().book_night);
    }

    #[test]
    fn rotate_turns_the_book_reader_landscape_and_the_tap_zones_follow() {
        let dir = tempfile::tempdir().unwrap();
        let (lib, _) = novel(dir.path());
        let settings_dir = dir.path().join("data");
        let mut ui = app(&lib, FakeGateway::default(), open_first_book())
            .with_settings_dir(settings_dir.clone());
        ui.run().unwrap();
        let portrait_lines = match ui.screen() {
            Screen::BookReader { pages, .. } => pages[0].len(),
            other => panic!("expected the reader, got {other:?}"),
        };
        ui.sheet = Some(Sheet::BookReading);
        assert_eq!(ui.sheet_rows().1[5].1, "Portrait");
        ui.tap_sheet(W / 2, reading_row_y(5)).unwrap();
        assert!(ui.sheet.is_none());
        // The reading frame is now the panel on its side: wider, shorter,
        // so fewer lines fit a page; whole repaint.
        assert_eq!((ui.layout.width, ui.layout.height), (H, W));
        let Screen::BookReader { pages, page, .. } = ui.screen() else {
            panic!("expected the reader");
        };
        assert!(
            pages[0].len() < portrait_lines,
            "{} < {portrait_lines}",
            pages[0].len()
        );
        assert_eq!(*page, 0);
        assert_eq!(ui.display().flushes.last(), Some(&RefreshMode::Full));
        assert_eq!(
            gideon_core::Settings::load(&settings_dir)
                .unwrap()
                .reader_rotation,
            90
        );

        // A finger on the right third of the READING frame arrives as a
        // panel point; the run loop's mapping brings it back, and it is
        // still "next page".
        let (px, py) = panel_point_for(H * 5 / 6, W / 2, 90);
        let (x, y) = ui.map_menu_point(px, py);
        ui.handle_tap(x, y).unwrap();
        let Screen::BookReader { page, .. } = ui.screen() else {
            panic!("expected the reader");
        };
        assert_eq!(*page, 1);
        ui.sheet = Some(Sheet::BookReading);
        assert_eq!(ui.sheet_rows().1[5].1, "Landscape");
    }

    /// Input that flushes like the Kobo's: `discard_taps` really drops what
    /// is queued (sleep requests aside). FakeInput keeps its script on a
    /// flush, which is how a mid-slide flush hid from every test.
    struct FlushingInput {
        events: std::collections::VecDeque<UiEvent>,
        flushes: std::rc::Rc<std::cell::Cell<usize>>,
    }

    impl gideon_device::InputSource for FlushingInput {
        fn next_event(&mut self) -> gideon_device::Result<UiEvent> {
            self.events
                .pop_front()
                .ok_or_else(|| gideon_device::Error::Display("input exhausted".into()))
        }
        fn poll_event(
            &mut self,
            _timeout: std::time::Duration,
        ) -> gideon_device::Result<Option<UiEvent>> {
            self.next_event().map(Some)
        }
        fn discard_queued(&mut self) {
            self.flushes.set(self.flushes.get() + 1);
            self.events.clear();
        }
        fn discard_taps(&mut self) {
            self.flushes.set(self.flushes.get() + 1);
            self.events.retain(|e| matches!(e, UiEvent::Sleep));
        }
    }

    #[test]
    fn a_slow_repaint_never_flushes_the_rest_of_an_edge_slide() {
        let dir = tempfile::tempdir().unwrap();
        let (lib, _) = novel(dir.path());
        // Open the book with a scripted run first, then hand the open reader
        // a flushing input and a stale window of zero: every event "took
        // too long", as a book-page repaint does on the Kobo.
        let mut ui = app(&lib, FakeGateway::default(), open_first_book());
        ui.run().unwrap();
        let book = match ui.screen() {
            Screen::BookReader { book, .. } => book.clone(),
            other => panic!("expected the reader, got {other:?}"),
        };
        let (levels, log, lights) = lights_recording();
        let flushes = std::rc::Rc::new(std::cell::Cell::new(0));
        let mut events = std::collections::VecDeque::new();
        // Slide up the right edge in five reports, lift, then tap the
        // middle (opens the reading menu, proving input still flows).
        for k in 1..=5u32 {
            events.push_back(UiEvent::Drag {
                x0: W - 5,
                y0: H - 100,
                x1: W - 5,
                y1: H - 100 - k * H / 10,
            });
        }
        events.push_back(UiEvent::Swipe {
            x0: W - 5,
            y0: H - 100,
            x1: W - 5,
            y1: H - 100 - H / 2,
        });
        let mut slow = UiApp::new(
            MemoryDisplay::new(W, H),
            FlushingInput {
                events,
                flushes: flushes.clone(),
            },
            FakeGateway::default(),
            lib.clone(),
        )
        .with_books_dir(lib.join("Books"))
        .with_lights(lights)
        .with_stale_tap_after(std::time::Duration::ZERO);
        slow.open_book_reader(book).unwrap();
        let _ = slow.run();
        // 20 % + half the height = 70 %, every report applied in order.
        assert_eq!(
            levels.borrow().0,
            70,
            "the whole slide landed: {:?}",
            log.borrow()
        );
        assert_eq!(
            *log.borrow(),
            (1..=5u32)
                .map(|k| Lamp::Brightness(20 + (k * H / 10 * 100 / H) as u8))
                .collect::<Vec<_>>()
        );
        assert_eq!(flushes.get(), 0, "motion is never flushed as stale");
    }

    #[test]
    fn a_slide_whose_lift_was_lost_does_not_pin_the_next_one() {
        let dir = tempfile::tempdir().unwrap();
        let (lib, _) = novel(dir.path());
        let (levels, _, lights) = lights_recording();
        let mut events = open_first_book();
        events.extend([
            // A slide from 20 % up a quarter: 45 %. Its lift never arrives.
            UiEvent::Drag {
                x0: W - 5,
                y0: H - 100,
                x1: W - 5,
                y1: H - 100 - H / 4,
            },
            // A new finger, lower down, up another quarter: from 45, not
            // from the lost slide's 20.
            UiEvent::Drag {
                x0: W - 6,
                y0: H - 60,
                x1: W - 6,
                y1: H - 60 - H / 4,
            },
            UiEvent::Swipe {
                x0: W - 6,
                y0: H - 60,
                x1: W - 6,
                y1: H - 60 - H / 4,
            },
            // And all the way down from there: clamps at 0, not stuck.
            UiEvent::Swipe {
                x0: W - 5,
                y0: 100,
                x1: W - 5,
                y1: H - 10,
            },
        ]);
        let mut ui = app(&lib, FakeGateway::default(), events).with_lights(lights);
        let probe = levels.clone();
        ui.run().unwrap();
        assert_eq!(probe.borrow().0, 0, "slid to the bottom");
        drop(ui);

        // The middle state, checked on its own: two slides, 20 → 45 → 70.
        let (levels, _, lights) = lights_recording();
        let mut events = open_first_book();
        events.extend([
            UiEvent::Drag {
                x0: W - 5,
                y0: H - 100,
                x1: W - 5,
                y1: H - 100 - H / 4,
            },
            UiEvent::Swipe {
                x0: W - 6,
                y0: H - 60,
                x1: W - 6,
                y1: H - 60 - H / 4,
            },
        ]);
        let mut ui = app(&lib, FakeGateway::default(), events).with_lights(lights);
        ui.run().unwrap();
        assert_eq!(levels.borrow().0, 70);
    }

    #[test]
    fn the_lamp_reaches_both_ends_and_back_in_small_steps() {
        let dir = tempfile::tempdir().unwrap();
        let (lib, _) = novel(dir.path());
        let (levels, _, lights) = lights_recording();
        let mut events = open_first_book();
        // Ten separate short slides up (each its own finger), then ten down.
        for i in 0..10u32 {
            let y0 = H - 100 - i;
            events.push(UiEvent::Swipe {
                x0: W - 5,
                y0,
                x1: W - 5,
                y1: y0 - H / 10,
            });
        }
        let mut ui = app(&lib, FakeGateway::default(), events).with_lights(lights);
        ui.run().unwrap();
        assert_eq!(levels.borrow().0, 100, "20 + 10 × 10, clamped");

        let (levels, _, lights) = lights_recording();
        let mut events = open_first_book();
        for i in 0..10u32 {
            let y0 = 200 + i;
            events.push(UiEvent::Swipe {
                x0: W - 5,
                y0,
                x1: W - 5,
                y1: y0 + H / 10,
            });
        }
        let mut ui = app(&lib, FakeGateway::default(), events).with_lights(lights);
        ui.run().unwrap();
        assert_eq!(levels.borrow().0, 0, "20 − 10 × 10, clamped at off");
    }

    #[test]
    fn a_flick_up_off_the_bottom_raises_the_reading_menu_with_the_lamp() {
        let dir = tempfile::tempdir().unwrap();
        let (lib, _) = novel(dir.path());
        let (levels, _, lights) = lights_recording();
        let mut events = open_first_book();
        events.push(open_reading_menu());
        let mut ui = app(&lib, FakeGateway::default(), events).with_lights(lights);
        ui.run().unwrap();
        assert!(
            matches!(ui.sheet, Some(Sheet::BookReading)),
            "{:?}",
            ui.sheet
        );
        assert_eq!(ui.display().flushes.last(), Some(&RefreshMode::Partial));
        assert!(
            matches!(ui.screen(), Screen::BookReader { .. }),
            "still reading"
        );
        // The lamp sliders sit under the menu's rows; a drag on the first
        // sets brightness where the finger is, a tap on the second warmth.
        let l = layout();
        let top = ui.reading_lamp_sliders_top().expect("a lamp strip");
        let x = l.pad + (W - 2 * l.pad) * 3 / 10;
        let y = top + l.row_h / 2;
        assert_eq!(ui.quick_slider_at(y), Some(0));
        ui.drag_menu(x, y, x, y, false).unwrap();
        assert_eq!(levels.borrow().0, 30);
        assert_eq!(ui.display().flushes.last(), Some(&RefreshMode::Partial));
        let y2 = top + l.row_h + SETTINGS_GAP + l.row_h / 2;
        let x2 = l.pad + (W - 2 * l.pad) * 6 / 10;
        ui.tap_sheet(x2, y2).unwrap();
        assert_eq!(levels.borrow().1, 60);
        assert!(
            matches!(ui.sheet, Some(Sheet::BookReading)),
            "sliders keep the menu up"
        );
        // The menu's rows sit above the strip and still act: Contents.
        let (sheet_top, h) = ui.sheet_bounds().unwrap();
        let strip = ui.reading_lamp_strip_h();
        let rows = ui.sheet_rows().1.len() as u32;
        let rows_top = sheet_top + (h - strip) - rows * l.row_h;
        ui.tap_sheet(W / 2, rows_top + l.row_h / 2).unwrap();
        assert!(
            matches!(ui.screen(), Screen::BookContents { .. }),
            "{:?}",
            ui.screen()
        );

        // Mid-page, a short flick does nothing; nor does a short one off
        // the bottom (a sloppy page-turn tap).
        let mut events = open_first_book();
        events.extend([
            UiEvent::Swipe {
                x0: W / 2,
                y0: H / 2,
                x1: W / 2,
                y1: H / 2 - H / 8,
            },
            UiEvent::Swipe {
                x0: W / 2,
                y0: H - 20,
                x1: W / 2,
                y1: H - 20 - 40,
            },
        ]);
        let mut ui = app(&lib, FakeGateway::default(), events);
        ui.run().unwrap();
        assert!(ui.sheet.is_none());
        assert!(matches!(ui.screen(), Screen::BookReader { .. }));
    }

    #[test]
    fn the_book_reader_leaves_and_rotates_by_the_manga_readers_gestures() {
        let dir = tempfile::tempdir().unwrap();
        let (lib, books) = novel(dir.path());
        let key = "Kurt Vonnegut/book-0001.epub";
        // Middle tap: out to the shelf, place saved.
        let mut events = open_first_book();
        events.extend([
            tap_reader(ReaderZone::NextPage),
            tap_reader(ReaderZone::Back),
        ]);
        let mut ui = app(&lib, FakeGateway::default(), events);
        ui.run().unwrap();
        assert!(
            matches!(ui.screen(), Screen::BookShelf { .. }),
            "{:?}",
            ui.screen()
        );
        assert!(ui.sheet.is_none(), "no menu from a page tap");
        assert!(gideon_core::BookPlaces::load(&books).get(key).is_some());

        // Swipe down a quarter of the page: out too.
        let mut events = open_first_book();
        events.push(UiEvent::Swipe {
            x0: W / 2,
            y0: H / 4,
            x1: W / 2,
            y1: H / 4 + H / 3,
        });
        let mut ui = app(&lib, FakeGateway::default(), events);
        ui.run().unwrap();
        assert!(
            matches!(ui.screen(), Screen::BookShelf { .. }),
            "{:?}",
            ui.screen()
        );

        // Swipe up mid-page: a quarter turn, still reading.
        let mut events = open_first_book();
        events.push(UiEvent::Swipe {
            x0: W / 2,
            y0: H * 3 / 4,
            x1: W / 2,
            y1: H * 3 / 4 - H / 3,
        });
        let mut ui = app(&lib, FakeGateway::default(), events);
        ui.run().unwrap();
        assert!(matches!(ui.screen(), Screen::BookReader { .. }));
        assert_eq!((ui.layout.width, ui.layout.height), (H, W));
    }

    #[test]
    fn the_sheet_gesture_is_one_rule_for_both_readers() {
        let h = 1448;
        // From the bottom eighth, an eighth or more of travel, vertical.
        assert!(is_sheet_swipe(500, h - 10, 500, h - 10 - h / 8, h));
        assert!(
            !is_sheet_swipe(500, h - 10, 500, h - 10 - h / 8 + 1, h),
            "too short"
        );
        assert!(
            !is_sheet_swipe(500, h - h / 4, 500, h / 4, h),
            "starts too high"
        );
        assert!(
            !is_sheet_swipe(500, h - 10, 900, h - 300, h),
            "too sideways"
        );
        assert!(!is_sheet_swipe(500, h - 300, 500, h - 10, h), "downwards");
    }
    #[test]
    fn reaching_the_last_page_finishes_the_book_and_unread_resets_it() {
        let dir = tempfile::tempdir().unwrap();
        let lib = dir.path().join("Manga");
        make_cbz(&lib.join("Series/vol1.cbz"), 2);
        let books = lib.join("Books");
        make_epub_with(
            &books.join("K/short.epub"),
            "Short",
            "K",
            &[("One", &["a"]), ("Two", &["b"])],
        );
        let mut ui = app(&lib, FakeGateway::default(), open_first_book()).with_clock(|| 5_000);
        ui.run().unwrap();
        let key = "K/short.epub";
        ui.turn_book_page(1).unwrap();
        assert_eq!(
            gideon_core::BookPlaces::load(&books)
                .get(key)
                .unwrap()
                .chapter,
            1
        );
        let place = gideon_core::BookPlaces::load(&books).get(key).unwrap();
        assert!(place.is_finished(), "the last page of the last chapter");

        // The shelf says so, and the sheet offers the way back.
        ui.pop().unwrap();
        let Screen::BookShelf { books: shown, .. } = ui.screen() else {
            panic!("expected the shelf, got {:?}", ui.screen());
        };
        let cards = ui.book_cards(shown);
        assert_eq!(cards[0].progress, Some(1.0));
        let book = shown[0].clone();
        ui.sheet = Some(Sheet::BookFile { book: book.clone() });
        let labels: Vec<String> = ui.sheet_rows().1.into_iter().map(|r| r.0).collect();
        assert!(labels.contains(&"Mark as unread".to_string()), "{labels:?}");
        ui.tap_sheet(W / 2, tap_modal_row_y(5, 2)).unwrap();
        assert!(
            gideon_core::BookPlaces::load(&books).get(key).is_none(),
            "back to page one"
        );
        assert_eq!(ui.display().flushes.last(), Some(&RefreshMode::Full));

        // And "Mark as finished" without reading it.
        ui.sheet = Some(Sheet::BookFile { book });
        ui.tap_sheet(W / 2, tap_modal_row_y(5, 2)).unwrap();
        let place = gideon_core::BookPlaces::load(&books).get(key).unwrap();
        assert_eq!(place.finished_at, Some(5_000));
    }

    #[test]
    fn finished_books_past_the_delay_are_offered_for_deletion_never_taken() {
        let dir = tempfile::tempdir().unwrap();
        let (lib, books) = shelf(dir.path());
        let day = 86_400u64;
        let now = 100 * day;
        let mut places = gideon_core::BookPlaces::default();
        places.set(
            "Kurt Vonnegut/book-0001.epub",
            gideon_core::BookPlace {
                chapter: 0,
                chapters: 1,
                finished_at: Some(now - 3 * day),
                ..Default::default()
            },
        );
        places.save(&books).unwrap();
        let target = books.join("Kurt Vonnegut/book-0001.epub");

        // Opening the tab offers it (default delay: two days), over the
        // shelf just drawn: Partial. "Not now" leaves everything as is.
        let mut ui = app(
            &lib,
            FakeGateway::default(),
            vec![tap_nav(0), tap_books_tab()],
        )
        .with_clock(move || now);
        ui.run().unwrap();
        let Some(Sheet::CleanFinishedBooks { books: offered }) = &ui.sheet else {
            panic!("expected the offer, got {:?}", ui.sheet);
        };
        assert_eq!(offered.len(), 1);
        assert_eq!(ui.sheet_rows().0, "You finished \"Slaughterhouse-Five\"");
        assert_eq!(ui.display().flushes.last(), Some(&RefreshMode::Partial));
        ui.tap_sheet(W / 2, tap_modal_row_y(3, 2)).unwrap();
        assert!(target.exists());
        assert_eq!(ui.display().flushes.last(), Some(&RefreshMode::Full));

        // "Keep them": never offered again.
        let mut ui = app(
            &lib,
            FakeGateway::default(),
            vec![tap_nav(0), tap_books_tab()],
        )
        .with_clock(move || now);
        ui.run().unwrap();
        ui.tap_sheet(W / 2, tap_modal_row_y(3, 1)).unwrap();
        assert!(target.exists());
        let mut ui = app(
            &lib,
            FakeGateway::default(),
            vec![tap_nav(0), tap_books_tab()],
        )
        .with_clock(move || now);
        ui.run().unwrap();
        assert!(ui.sheet.is_none(), "kept books are not offered again");

        // A fresh finished book, deleted on request: file and shelf row go.
        let mut places = gideon_core::BookPlaces::load(&books);
        places.set(
            "Natsu Hyuuga/book-0002.epub",
            gideon_core::BookPlace {
                finished_at: Some(now - 5 * day),
                ..Default::default()
            },
        );
        places.save(&books).unwrap();
        let other = books.join("Natsu Hyuuga/book-0002.epub");
        let mut ui = app(
            &lib,
            FakeGateway::default(),
            vec![tap_nav(0), tap_books_tab()],
        )
        .with_clock(move || now);
        ui.run().unwrap();
        ui.tap_sheet(W / 2, tap_modal_row_y(3, 0)).unwrap();
        assert!(!other.exists());
        assert!(target.exists(), "the kept one stays");
        let Screen::BookShelf { books: shown, .. } = ui.screen() else {
            panic!("expected the shelf");
        };
        assert_eq!(shown.len(), 1);

        // A delay of 0 (never) offers nothing.
        let settings_dir = dir.path().join("data");
        gideon_core::Settings {
            finished_cleanup_hours: 0,
            ..Default::default()
        }
        .save(&settings_dir)
        .unwrap();
        let mut places = gideon_core::BookPlaces::load(&books);
        let mut p = places.get("Kurt Vonnegut/book-0001.epub").unwrap();
        p.keep = false;
        places.set("Kurt Vonnegut/book-0001.epub", p);
        places.save(&books).unwrap();
        let mut ui = app(
            &lib,
            FakeGateway::default(),
            vec![tap_nav(0), tap_books_tab()],
        )
        .with_settings_dir(settings_dir)
        .with_clock(move || now);
        ui.run().unwrap();
        assert!(ui.sheet.is_none());
    }
}

// --- Internet Archive: search-first screens (design `internet-archive-search`) ---
//
// The whole path a reader takes — Books › Internet Archive › keyboard ›
// results › book sheet › Download › Read now — against canned archive.org
// JSON, with every repaint's refresh mode pinned (docs/REFRESH.md).

mod internet_archive {
    use super::*;
    use crate::ui::book_search::{self, DownloadState};

    const GUTENBERG: &str = "https://www.gutenberg.org/ebooks.opds/";

    /// A search answer: `found` in all, these docs on this page.
    fn search_json(found: usize, docs: &[(&str, &str, &str, &str, bool)]) -> String {
        let docs: Vec<String> = docs
            .iter()
            .map(|(id, title, creator, year, epub)| {
                let format = if *epub { r#"["EPUB","Text PDF"]"# } else { r#"["Text PDF"]"# };
                format!(
                    r#"{{"identifier":"{id}","title":"{title}","creator":"{creator}","year":{year},"language":"eng","downloads":10,"format":{format}}}"#
                )
            })
            .collect();
        format!(
            r#"{{"response":{{"numFound":{found},"start":0,"docs":[{}]}}}}"#,
            docs.join(",")
        )
    }

    fn item_json(id: &str, title: &str, creator: &str, size: u64) -> String {
        format!(
            r#"{{"metadata":{{"identifier":"{id}","title":"{title}","creator":"{creator}"}},"files":[{{"name":"{id}.epub","format":"EPUB","size":"{size}"}},{{"name":"{id}.pdf","format":"Text PDF","size":"1"}}]}}"#
        )
    }

    fn search_url(q: &str) -> String {
        gideon_sources::archive::search_url(q).unwrap().to_string()
    }

    /// A real (tiny) EPUB, so "Read now" can open it.
    fn epub_bytes(title: &str, author: &str) -> Vec<u8> {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("b.epub");
        super::library_tabs::make_epub(&path, title, author);
        std::fs::read(path).unwrap()
    }

    /// Gutenberg plus the built-in Internet Archive, with "frankenstein"
    /// answering one book with an EPUB and one scan without.
    fn gateway() -> FakeGateway {
        let mut catalogs = vec![gideon_core::BookCatalog {
            name: "Project Gutenberg".into(),
            url: GUTENBERG.into(),
        }];
        super::super::gateway::with_internet_archive(&mut catalogs);
        FakeGateway {
            catalogs,
            web: gideon_sources::FakeFetcher::new()
                .with(
                    &search_url("frankenstein"),
                    search_json(
                        2,
                        &[
                            (
                                "frankenstein00shel",
                                "Frankenstein",
                                "Mary Wollstonecraft Shelley",
                                "1818",
                                true,
                            ),
                            (
                                "frankenstein-scan",
                                "Frankenstein (scan)",
                                "Shelley",
                                "1900",
                                false,
                            ),
                        ],
                    ),
                )
                .with(
                    "https://archive.org/metadata/frankenstein00shel",
                    item_json(
                        "frankenstein00shel",
                        "Frankenstein",
                        "Mary Wollstonecraft Shelley",
                        2_097_152,
                    ),
                )
                .with(
                    "https://archive.org/download/frankenstein00shel/frankenstein00shel.epub",
                    epub_bytes("Frankenstein", "Mary Wollstonecraft Shelley"),
                )
                .with(&search_url("xyzzy"), search_json(0, &[])),
            ..FakeGateway::default()
        }
    }

    /// Discover › Books › Search all free books (the first row).
    fn open_ia() -> Vec<UiEvent> {
        vec![nav_discover(), tap_card(3), tap_row(0)]
    }

    fn type_and_search(q: &str) -> Vec<UiEvent> {
        let mut e: Vec<UiEvent> = q.chars().map(|c| tap_key(Key::Char(c))).collect();
        e.push(tap_key(Key::Search));
        e
    }

    /// Centre of result row `i` on the current page.
    fn tap_result(i: usize) -> UiEvent {
        let l = layout();
        let rh = book_search::result_row_h(&l);
        UiEvent::Tap {
            x: l.width / 2,
            y: l.content_top() + i as u32 * rh + rh / 2,
        }
    }

    /// The book sheet's buttons: 0 the secondary (left), 1 the primary.
    fn tap_sheet_button(i: usize) -> UiEvent {
        let probe = app(Path::new("/nonexistent"), FakeGateway::default(), vec![]);
        let r = probe.download_sheet_layout().buttons[i];
        UiEvent::Tap {
            x: r.0 + r.2 / 2,
            y: r.1 + r.3 / 2,
        }
    }

    fn tap_footer(i: usize) -> UiEvent {
        let r = book_search::results_footer_zones(&layout())[i];
        UiEvent::Tap {
            x: r.0 + r.2 / 2,
            y: r.1 + r.3 / 2,
        }
    }

    fn run(
        dir: &Path,
        gateway: FakeGateway,
        events: Vec<UiEvent>,
    ) -> UiApp<MemoryDisplay, FakeInput, FakeGateway> {
        let mut ui = app(dir, gateway, events);
        ui.run().unwrap();
        ui
    }

    #[test]
    fn the_internet_archive_is_built_in_once_beside_the_configured_catalogues() {
        let mut catalogs = vec![gideon_core::BookCatalog {
            name: "Project Gutenberg".into(),
            url: GUTENBERG.into(),
        }];
        super::super::gateway::with_internet_archive(&mut catalogs);
        let labels: Vec<String> = catalogs.iter().map(|c| c.label()).collect();
        assert_eq!(labels, ["Search all free books", "Project Gutenberg"]);
        catalogs[1].name = "IA".into();
        super::super::gateway::with_internet_archive(&mut catalogs);
        assert_eq!(
            catalogs.len(),
            2,
            "typed in by hand, under another name: not twice"
        );
    }

    #[test]
    fn the_catalogue_row_says_what_it_does() {
        let dir = tempfile::tempdir().unwrap();
        let ui = run(dir.path(), gateway(), vec![nav_discover(), tap_card(3)]);
        let Screen::Books { catalogs } = ui.screen() else {
            panic!("expected the catalogue list, got {:?}", ui.screen());
        };
        assert_eq!(catalogs.len(), 2);
    }

    #[test]
    fn tapping_the_internet_archive_opens_straight_onto_the_keyboard() {
        let dir = tempfile::tempdir().unwrap();
        let ui = run(dir.path(), gateway(), open_ia());
        let Screen::BookSearch {
            title,
            query,
            recent,
            ..
        } = ui.screen()
        else {
            panic!("expected the search screen, got {:?}", ui.screen());
        };
        assert_eq!(title, "Search all free books");
        assert!(query.is_empty());
        assert!(recent.is_empty(), "no history yet");
        // A new screen: Full, in colour (the accent Search key).
        assert_eq!(ui.display().flushes.last(), Some(&RefreshMode::Full));
        assert_eq!(ui.display().blits.last(), Some(&true));
    }

    #[test]
    fn typing_repaints_partially() {
        let dir = tempfile::tempdir().unwrap();
        let mut events = open_ia();
        events.extend("ab".chars().map(|c| tap_key(Key::Char(c))));
        let ui = run(dir.path(), gateway(), events);
        let Screen::BookSearch { query, .. } = ui.screen() else {
            panic!("expected the search screen, got {:?}", ui.screen());
        };
        assert_eq!(query, "ab");
        let f = &ui.display().flushes;
        assert_eq!(
            &f[f.len() - 2..],
            [RefreshMode::Partial, RefreshMode::Partial]
        );
    }

    #[test]
    fn results_are_book_rows_without_the_pdf_only_scan() {
        let dir = tempfile::tempdir().unwrap();
        let mut events = open_ia();
        events.extend(type_and_search("frankenstein"));
        let ui = run(dir.path(), gateway(), events);
        let Screen::BookResults {
            query,
            feed,
            page,
            error,
        } = ui.screen()
        else {
            panic!("expected results, got {:?}", ui.screen());
        };
        assert_eq!(query, "frankenstein");
        assert_eq!(*page, 0);
        assert_eq!(*error, None);
        let rows: Vec<(String, Option<String>, Option<String>)> = feed
            .entries
            .iter()
            .map(|e| (e.title.clone(), e.detail.clone(), e.year.clone()))
            .collect();
        assert_eq!(
            rows,
            [(
                "Frankenstein".to_string(),
                Some("Mary Wollstonecraft Shelley".to_string()),
                Some("1818".to_string())
            )]
        );
        assert_eq!(ui.display().flushes.last(), Some(&RefreshMode::Full));
        assert_eq!(
            ui.display().blits.last(),
            Some(&true),
            "chips are colour blocks"
        );
    }

    #[test]
    fn a_search_is_remembered_and_its_chip_runs_it_again() {
        let dir = tempfile::tempdir().unwrap();
        // Search, back out to the keyboard, back out to the list, reopen.
        let mut events = open_ia();
        events.extend(type_and_search("frankenstein"));
        events.push(tap_footer(0)); // Back to the keyboard
        events.push(tap_nav(0)); // nav strip's Back on the keyboard screen
        events.push(tap_row(0)); // the search catalogue again
        let ui = run(dir.path(), gateway(), events);
        let Screen::BookSearch { recent, .. } = ui.screen() else {
            panic!("expected the search screen, got {:?}", ui.screen());
        };
        assert_eq!(recent, &["frankenstein"]);

        // The chip runs the search without typing.
        let chip = ui.search_extras(recent).chips[0];
        let mut events = open_ia();
        events.push(UiEvent::Tap {
            x: chip.0 + chip.2 / 2,
            y: chip.1 + chip.3 / 2,
        });
        let ui = run(dir.path(), gateway(), events);
        assert!(
            matches!(ui.screen(), Screen::BookResults { query, .. } if query == "frankenstein"),
            "{:?}",
            ui.screen()
        );
    }

    #[test]
    fn recent_searches_keep_the_last_three_newest_first_without_repeats() {
        let dir = tempfile::tempdir().unwrap();
        for q in ["a", "b", "A", "c", "d"] {
            book_search::remember_search(dir.path(), q);
        }
        assert_eq!(
            book_search::load_recent_searches(dir.path()),
            ["d", "c", "A"]
        );
    }

    #[test]
    fn a_tap_raises_the_book_sheet_without_flashing_then_downloads_and_reads() {
        let dir = tempfile::tempdir().unwrap();
        let mut events = open_ia();
        events.extend(type_and_search("frankenstein"));
        events.push(tap_result(0));
        let ui = run(dir.path(), gateway(), events.clone());
        let Some(Sheet::BookDownload { epub, state, .. }) = ui.sheet() else {
            panic!("expected the book sheet, got {:?}", ui.sheet());
        };
        assert_eq!(*state, DownloadState::Ready);
        assert_eq!(epub.as_ref().and_then(|e| e.size), Some(2_097_152));
        // "Checking the file…" then the facts: the same strip, twice, no flash.
        let f = &ui.display().flushes;
        assert_eq!(
            &f[f.len() - 2..],
            [RefreshMode::Partial, RefreshMode::Partial]
        );
        assert!(
            super::books::walk(&dir.path().join("Books")).is_empty(),
            "nothing downloads until Download is pressed"
        );

        // Download.
        events.push(tap_sheet_button(1));
        let ui = run(dir.path(), gateway(), events.clone());
        let Some(Sheet::BookDownload {
            state: DownloadState::Saved(path),
            ..
        }) = ui.sheet()
        else {
            panic!("expected Saved, got {:?}", ui.sheet());
        };
        assert!(
            path.ends_with("Mary Wollstonecraft Shelley/Frankenstein.epub"),
            "{path:?}"
        );
        assert!(path.exists());
        let f = &ui.display().flushes;
        assert_eq!(
            &f[f.len() - 2..],
            [RefreshMode::Partial, RefreshMode::Partial]
        );
        std::fs::remove_dir_all(dir.path().join("Books")).unwrap();

        // Read now: gideon's own reader, a new screen, Full.
        events.push(tap_sheet_button(1));
        let ui = run(dir.path(), gateway(), events);
        assert!(ui.sheet().is_none());
        assert!(
            matches!(ui.screen(), Screen::BookReader { book, .. } if book.title == "Frankenstein"),
            "{:?}",
            ui.screen()
        );
        assert_eq!(ui.display().flushes.last(), Some(&RefreshMode::Full));
    }

    #[test]
    fn cancel_closes_the_sheet_with_a_full_refresh() {
        let dir = tempfile::tempdir().unwrap();
        let mut events = open_ia();
        events.extend(type_and_search("frankenstein"));
        events.push(tap_result(0));
        events.push(tap_sheet_button(0));
        let ui = run(dir.path(), gateway(), events);
        assert!(ui.sheet().is_none());
        assert!(matches!(ui.screen(), Screen::BookResults { .. }));
        // The sheet held a strip of the list stale: reveal it Full.
        assert_eq!(ui.display().flushes.last(), Some(&RefreshMode::Full));
    }

    #[test]
    fn a_book_already_on_the_kobo_opens_saved_without_fetching() {
        let dir = tempfile::tempdir().unwrap();
        let on_disk = dir
            .path()
            .join("Books/Mary Wollstonecraft Shelley/Frankenstein.epub");
        std::fs::create_dir_all(on_disk.parent().unwrap()).unwrap();
        std::fs::write(
            &on_disk,
            epub_bytes("Frankenstein", "Mary Wollstonecraft Shelley"),
        )
        .unwrap();
        // No metadata canned: a fetch would fail the sheet.
        let mut g = gateway();
        g.web = gideon_sources::FakeFetcher::new().with(
            &search_url("frankenstein"),
            search_json(
                1,
                &[(
                    "frankenstein00shel",
                    "Frankenstein",
                    "Mary Wollstonecraft Shelley",
                    "1818",
                    true,
                )],
            ),
        );
        let mut events = open_ia();
        events.extend(type_and_search("frankenstein"));
        events.push(tap_result(0));
        let ui = run(dir.path(), g, events);
        assert!(
            matches!(ui.sheet(), Some(Sheet::BookDownload { state: DownloadState::Saved(p), .. }) if *p == on_disk),
            "{:?}",
            ui.sheet()
        );
        assert_eq!(ui.display().flushes.last(), Some(&RefreshMode::Partial));
    }

    #[test]
    fn nothing_found_says_so_and_edit_search_keeps_the_query() {
        let dir = tempfile::tempdir().unwrap();
        let mut events = open_ia();
        events.extend(type_and_search("xyzzy"));
        let ui = run(dir.path(), gateway(), events.clone());
        assert!(
            matches!(ui.screen(), Screen::BookResults { feed, error: None, .. } if feed.entries.is_empty()),
            "{:?}",
            ui.screen()
        );
        let b = book_search::edit_search_button(&layout());
        events.push(UiEvent::Tap {
            x: b.0 + b.2 / 2,
            y: b.1 + b.3 / 2,
        });
        let ui = run(dir.path(), gateway(), events);
        assert!(
            matches!(ui.screen(), Screen::BookSearch { query, .. } if query == "xyzzy"),
            "{:?}",
            ui.screen()
        );
        assert_eq!(ui.display().flushes.last(), Some(&RefreshMode::Full));
    }

    #[test]
    fn a_failed_search_is_the_empty_state_not_a_dead_end() {
        let dir = tempfile::tempdir().unwrap();
        let mut events = open_ia();
        events.extend(type_and_search("nocanned"));
        let ui = run(dir.path(), gateway(), events);
        assert!(
            matches!(ui.screen(), Screen::BookResults { error: Some(_), .. }),
            "{:?}",
            ui.screen()
        );
    }

    #[test]
    fn results_page_from_the_footer_through_one_merged_list() {
        let dir = tempfile::tempdir().unwrap();
        let per = book_search::results_per_page(&layout());
        // A unified search hands back one merged list (up to 25 books), so
        // the footer pages through it without another request.
        let total = 25;
        let docs: Vec<(String, String)> = (0..total)
            .map(|i| (format!("book{i}"), format!("Book {i}")))
            .collect();
        let d: Vec<(&str, &str, &str, &str, bool)> = docs
            .iter()
            .map(|(id, t)| (id.as_str(), t.as_str(), "Anon", "1900", true))
            .collect();
        let mut g = gateway();
        g.web = g.web.with(&search_url("book"), search_json(total, &d));
        let last_page = total.div_ceil(per) - 1;
        assert!(last_page >= 1, "more than one page of results");
        let mut events = open_ia();
        events.extend(type_and_search("book"));
        events.extend((0..last_page).map(|_| tap_footer(3))); // Next › to the end
        let ui = run(dir.path(), g, events);
        let Screen::BookResults { feed, page, .. } = ui.screen() else {
            panic!("expected results, got {:?}", ui.screen());
        };
        assert_eq!(*page, last_page);
        assert_eq!(feed.entries.len(), total);
        assert_eq!(feed.next, None);
        // A page flip replaces the whole list: Full.
        assert_eq!(ui.display().flushes.last(), Some(&RefreshMode::Full));
    }

    /// Renders every screen of the flow at the Libra Colour's size, in
    /// colour, for the design review. `GIDEON_SHOTS=<dir> cargo test -p
    /// gideon-app -- --ignored internet_archive::shots`.
    #[test]
    #[ignore]
    fn shots() {
        let Some(out) = std::env::var_os("GIDEON_SHOTS") else {
            return;
        };
        let out = PathBuf::from(out);
        std::fs::create_dir_all(&out).unwrap();
        let (w, h) = (1264, 1680);
        let probe = UiApp::new(
            MemoryDisplay::new(w, h),
            FakeInput::new(vec![]),
            FakeGateway::default(),
            PathBuf::from("/x"),
        );
        let l = UiLayout::new(w, h);
        let tap_at = |x: u32, y: u32| UiEvent::Tap { x, y };
        let row = |i: usize| tap_at(w / 2, l.row_top(i) + l.row_h / 2);
        let key = |k: Key| {
            let (_, x, y, kw, kh) = l
                .keyboard_keys()
                .into_iter()
                .find(|(kk, ..)| *kk == k)
                .unwrap();
            tap_at(x + kw / 2, y + kh / 2)
        };
        let nav = |i: usize| tap_at(w / 8 + i as u32 * w / 4, h - l.nav_h / 2);
        let card = tap_card_at(w, h, 3);
        let rh = book_search::result_row_h(&l);
        let result = |i: usize| tap_at(w / 2, l.content_top() + i as u32 * rh + rh / 2);
        let sheet = probe.download_sheet_layout();
        let button = |i: usize| {
            let r = sheet.buttons[i];
            tap_at(r.0 + r.2 / 2, r.1 + r.3 / 2)
        };
        let shots_gateway = || {
            let mut g = gateway();
            g.web = g
                .web
                .with(
                    &search_url("frankenstein"),
                    search_json(
                        48,
                        &[
                            (
                                "frankenstein00shel",
                                "Frankenstein; or, The Modern Prometheus",
                                "Mary Wollstonecraft Shelley",
                                "1818",
                                true,
                            ),
                            (
                                "frank1831",
                                "Frankenstein (1831 revised edition)",
                                "Mary Shelley",
                                "1831",
                                true,
                            ),
                            ("lastman", "The Last Man", "Mary Shelley", "1826", true),
                            (
                                "presumption",
                                "Presumption; or, The Fate of Frankenstein",
                                "Richard Brinsley Peake",
                                "1823",
                                true,
                            ),
                            ("mathilda", "Mathilda", "Mary Shelley", "1959", true),
                            (
                                "tales",
                                "Tales and Stories",
                                "Mary Wollstonecraft Shelley",
                                "1891",
                                true,
                            ),
                        ],
                    ),
                )
                .with(
                    "https://archive.org/metadata/frankenstein00shel",
                    item_json(
                        "frankenstein00shel",
                        "Frankenstein; or, The Modern Prometheus",
                        "Mary Wollstonecraft Shelley",
                        2_097_152,
                    ),
                )
                .with(&search_url("frankenstien"), search_json(0, &[]));
            g
        };
        let mut steps: Vec<(&str, Vec<UiEvent>)> = Vec::new();
        let base = vec![nav(2), card, row(1)];
        steps.push(("1-catalogues", vec![nav(2), card]));
        let mut s = base.clone();
        s.extend("frankenstein".chars().map(|c| key(Key::Char(c))));
        steps.push(("2-search", s.clone()));
        s.push(key(Key::Search));
        steps.push(("3-results", s.clone()));
        s.push(result(0));
        steps.push(("4-sheet", s.clone()));
        s.push(button(1));
        steps.push(("5-saved", s.clone()));
        let mut e = base.clone();
        e.extend("frankenstien".chars().map(|c| key(Key::Char(c))));
        e.push(key(Key::Search));
        steps.push(("6-empty", e));
        for (name, events) in steps {
            let run_dir = tempfile::tempdir().unwrap();
            let gd = run_dir.path();
            book_search::remember_search(gd, "sherlock holmes");
            book_search::remember_search(gd, "moby dick");
            book_search::remember_search(gd, "jane austen");
            let mut ui = UiApp::new(
                MemoryDisplay::new(w, h),
                FakeInput::new(events),
                shots_gateway(),
                gd.to_path_buf(),
            )
            .with_books_dir(gd.join("Books"));
            ui.run().unwrap();
            let rgb = ui.compose_final().unwrap();
            image::RgbImage::from_raw(rgb.width, rgb.height, rgb.pixels)
                .unwrap()
                .save(out.join(format!("{name}.png")))
                .unwrap();
        }
    }
}

// --- Reader tools: renders for the design review ---------------------------
//
// Every screen of the reader's new tools at the Libra Colour's real size,
// set in a book of original placeholder prose (not a real novel's text).
// `GIDEON_SHOTS=<dir> cargo test -p gideon-app -- --ignored reader_tools::shots`.

mod reader_tools {
    use super::*;

    pub(super) const PROSE: [&str; 6] = [
        "The harbour was quiet when Elena reached it, the boats drawn up in a crooked line and the nets left to dry along the wall.",
        "She had promised herself she would not look back at the house, and for a while she kept the promise.",
        "Further along, a boy was mending a sail with slow, careful stitches. The light had gone gloaming-grey over the water.",
        "“You’re early,” he said, without looking up. She told him the tide would not wait for either of them.",
        "They worked until the lamps came on in the cottages above, and neither of them spoke of the letter in her coat.",
        "Some doors are only ever open from the inside, her mother used to say, and Elena had never understood it until now.",
    ];

    /// A reader on a full-size panel, the placeholder book open on page 1.
    pub(super) fn big_reader(dir: &Path) -> UiApp<MemoryDisplay, FakeInput, FakeGateway> {
        let lib = dir.join("Manga");
        let books = lib.join("Books");
        let paras: Vec<&str> = PROSE.iter().cycle().take(18).copied().collect();
        super::library_tabs::make_epub_with(
            &books.join("Placeholder Author/harbour.epub"),
            "The Harbour Wall",
            "Placeholder Author",
            &[("Chapter IV", &paras)],
        );
        let mut ui = UiApp::new(
            MemoryDisplay::new(1264, 1680),
            FakeInput::new(vec![]),
            FakeGateway::default(),
            lib.clone(),
        )
        .with_books_dir(books.clone())
        .with_settings_dir(dir.join("data"));
        let book = gideon_core::books::scan(&books).unwrap().remove(0);
        ui.open_book_reader(book).unwrap();
        ui
    }

    fn save(ui: &UiApp<MemoryDisplay, FakeInput, FakeGateway>, out: &Path, name: &str) {
        let rgb = ui.compose_final().unwrap();
        image::RgbImage::from_raw(rgb.width, rgb.height, rgb.pixels)
            .unwrap()
            .save(out.join(format!("{name}.png")))
            .unwrap();
    }

    #[test]
    #[ignore]
    fn shots() {
        let Some(out) = std::env::var_os("GIDEON_SHOTS") else {
            return;
        };
        let out = PathBuf::from(out);
        std::fs::create_dir_all(&out).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let mut ui = big_reader(dir.path());
        let mut st = ui.load_settings();
        st.book_font = "literata".into();
        ui.save_settings(&st);
        ui.reflow_book().unwrap();
        ui.sheet = Some(Sheet::BookText { preview: None });
        save(&ui, &out, "t1-text-sheet");
        ui.sheet = Some(Sheet::BookFonts);
        save(&ui, &out, "t2-font-list");
        ui.sheet = None;
        for f in FontFamily::ALL {
            let mut st = ui.load_settings();
            st.book_font = f.name().into();
            ui.save_settings(&st);
            ui.reflow_book().unwrap();
            save(&ui, &out, &format!("t3-page-{}", f.name()));
        }
    }
}
