//! On-device browse UI: a tap-driven menu system rendered straight to the
//! framebuffer, so the device is usable without SSH.
//!
//! [`UiApp`] is generic over [`Display`], [`InputSource`] and
//! [`SourceGateway`], so the whole state machine is unit-testable with
//! `MemoryDisplay` + `FakeInput` + a fake gateway (no network, no WASM).
//!
//! Screens: Home → Library (cover shelf → Reader) and Home → Sources →
//! Listings → MangaList → ChapterList → download → Reader. Navigation is a
//! stack; the bottom bar is [Back] [Prev] [Next]. Screen changes use a full
//! e-ink refresh, in-screen updates (pagination, status) partial ones.
//! Errors never panic the UI: they land on a message screen with Back.

mod gateway;
mod layout;
#[cfg(test)]
mod tests;

pub use gateway::{AidokuGateway, ChapterEntry, MangaEntry, SourceEntry, SourceGateway};
pub use layout::{ReaderZone, TapTarget, UiLayout};

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use gideon_core::{CbzDocument, Library, LibraryEntry, ProgressStore};
use gideon_device::{Display, InputSource, RefreshMode, UiEvent};
use gideon_render::shelf::{compose_shelf, ShelfEntry, ShelfLayout};
use gideon_render::text::{draw_text, measure_text};
use gideon_render::{FitMode, GrayPage};

use crate::reader::Reader;

const HOME_ROWS: [&str; 3] = ["Library", "Browse sources", "Check for updates"];
const SHELF_COLUMNS: u32 = 3;

/// One row on the Sources screen.
#[derive(Debug, Clone)]
enum SourceRow {
    Installed(SourceEntry),
    Separator(String),
    Available(SourceEntry),
    /// Non-tappable informational row (e.g. a list fetch error).
    Note(String),
}

impl SourceRow {
    fn label(&self) -> (String, bool) {
        match self {
            SourceRow::Installed(s) => (s.name.clone(), true),
            SourceRow::Separator(text) | SourceRow::Note(text) => (text.clone(), false),
            SourceRow::Available(s) => (format!("{} — install", s.name), false),
        }
    }
}

#[derive(Debug, Clone)]
enum Screen {
    Home,
    Library {
        entries: Vec<LibraryEntry>,
        page: usize,
    },
    Sources {
        rows: Vec<SourceRow>,
        page: usize,
    },
    Listings {
        source: SourceEntry,
    },
    MangaList {
        source: SourceEntry,
        listing: String,
        mangas: Vec<MangaEntry>,
        page: usize,
    },
    ChapterList {
        source: SourceEntry,
        manga: MangaEntry,
        chapters: Vec<ChapterEntry>,
        page: usize,
    },
    /// Update available; any content tap installs, Back declines.
    UpdatePrompt {
        body: String,
    },
    /// Error/info screen; any content tap (or Back) returns.
    Message {
        title: String,
        body: String,
    },
}

enum Flow {
    Continue,
    Quit,
}

pub struct UiApp<D: Display, I: InputSource, G: SourceGateway> {
    display: D,
    input: I,
    gateway: G,
    library_dir: PathBuf,
    layout: UiLayout,
    stack: Vec<Screen>,
    /// Reader fit mode (from settings.json `reader_fit`).
    reader_fit: FitMode,
    /// Reader rotation in degrees (from settings.json `reader_rotation`).
    reader_rotation: u32,
}

impl<D: Display, I: InputSource, G: SourceGateway> UiApp<D, I, G> {
    pub fn new(display: D, input: I, gateway: G, library_dir: PathBuf) -> Self {
        let layout = UiLayout::new(display.width(), display.height());
        Self {
            display,
            input,
            gateway,
            library_dir,
            layout,
            stack: vec![Screen::Home],
            reader_fit: FitMode::Contain,
            reader_rotation: 0,
        }
    }

    /// Apply the reader-related settings (fit mode and rotation).
    pub fn with_reader_settings(mut self, fit: FitMode, rotation: u32) -> Self {
        self.reader_fit = fit;
        self.reader_rotation = rotation;
        self
    }

    /// The underlying display (for tests and headless screenshots).
    pub fn display(&self) -> &D {
        &self.display
    }

    #[cfg(test)]
    #[cfg(test)]
    pub(crate) fn gateway(&self) -> &G {
        &self.gateway
    }

    #[cfg_attr(feature = "kobo", allow(dead_code))]
    fn screen(&self) -> &Screen {
        self.stack.last().expect("screen stack is never empty")
    }

    /// Render the current screen without entering the event loop (used by
    /// the headless `--screenshot` mode).
    pub fn render_once(&mut self) -> Result<()> {
        self.render_current(RefreshMode::Full)
    }

    /// Main loop: render, then process taps until the input source ends
    /// (or the user backs out of the home screen).
    pub fn run(&mut self) -> Result<()> {
        self.render_current(RefreshMode::Full)?;
        loop {
            let Ok(UiEvent::Tap { x, y }) = self.input.next_event() else {
                return Ok(()); // input source closed
            };
            match self.handle_tap(x, y) {
                Ok(Flow::Quit) => return Ok(()),
                Ok(Flow::Continue) => {}
                // The UI must never die on an error: show it instead.
                Err(e) => self.show_error(&e)?,
            }
        }
    }

    // --- navigation ---

    fn push(&mut self, screen: Screen) -> Result<()> {
        self.stack.push(screen);
        self.render_current(RefreshMode::Full)
    }

    fn pop(&mut self) -> Result<Flow> {
        if self.stack.len() <= 1 {
            return Ok(Flow::Quit); // Back on Home exits the app
        }
        self.stack.pop();
        self.render_current(RefreshMode::Full)?;
        Ok(Flow::Continue)
    }

    fn show_error(&mut self, error: &anyhow::Error) -> Result<()> {
        self.push(Screen::Message {
            title: "Error".to_string(),
            body: format!("{error:#}"),
        })
    }

    // --- input handling ---

    fn handle_tap(&mut self, x: u32, y: u32) -> Result<Flow> {
        match self.layout.tap_target(x, y) {
            TapTarget::Back => self.pop(),
            TapTarget::Prev => {
                self.flip_page(-1)?;
                Ok(Flow::Continue)
            }
            TapTarget::Next => {
                self.flip_page(1)?;
                Ok(Flow::Continue)
            }
            TapTarget::Row(row) => self.activate(row, x, y),
            TapTarget::Title => Ok(Flow::Continue),
        }
    }

    /// Change page within the current screen (partial refresh).
    fn flip_page(&mut self, delta: i64) -> Result<()> {
        let per_page = self.layout.rows_per_page();
        let shelf_capacity = self.shelf_layout().capacity().max(1);
        let Some(screen) = self.stack.last_mut() else {
            return Ok(());
        };
        let (page, count) = match screen {
            Screen::Library { entries, page } => (page, entries.len().div_ceil(shelf_capacity)),
            Screen::Sources { rows, page } => (page, rows.len().div_ceil(per_page)),
            Screen::MangaList { mangas, page, .. } => (page, mangas.len().div_ceil(per_page)),
            Screen::ChapterList { chapters, page, .. } => (page, chapters.len().div_ceil(per_page)),
            _ => return Ok(()),
        };
        let count = count.max(1);
        let new = (*page as i64 + delta).clamp(0, count as i64 - 1) as usize;
        if new != *page {
            *page = new;
            self.render_current(RefreshMode::Partial)?;
        }
        Ok(())
    }

    /// Activate whatever sits at content row `row` (tap at `x`, `y`).
    fn activate(&mut self, row: usize, x: u32, y: u32) -> Result<Flow> {
        let screen = self.stack.last().cloned().expect("stack never empty");
        match screen {
            Screen::Home => match row {
                0 => {
                    self.open_library()?;
                    Ok(Flow::Continue)
                }
                1 => {
                    self.open_sources()?;
                    Ok(Flow::Continue)
                }
                2 => {
                    self.check_updates()?;
                    Ok(Flow::Continue)
                }
                _ => Ok(Flow::Continue),
            },
            Screen::Library { entries, page } => self.tap_library_cell(&entries, page, x, y),
            Screen::Sources { rows, page } => {
                let index = page * self.layout.rows_per_page() + row;
                match rows.get(index).cloned() {
                    Some(SourceRow::Installed(source)) => {
                        self.push(Screen::Listings { source })?;
                        Ok(Flow::Continue)
                    }
                    Some(SourceRow::Available(source)) => {
                        self.install_and_refresh(&source)?;
                        Ok(Flow::Continue)
                    }
                    _ => Ok(Flow::Continue),
                }
            }
            Screen::Listings { source } => {
                let listing = match row {
                    0 => "Popular",
                    1 => "Latest",
                    _ => return Ok(Flow::Continue),
                };
                self.open_manga_list(&source, listing)?;
                Ok(Flow::Continue)
            }
            Screen::MangaList {
                source,
                mangas,
                page,
                ..
            } => {
                let index = page * self.layout.rows_per_page() + row;
                if let Some(manga) = mangas.get(index).cloned() {
                    self.open_chapter_list(&source, &manga)?;
                }
                Ok(Flow::Continue)
            }
            Screen::ChapterList {
                source,
                manga,
                chapters,
                page,
            } => {
                let index = page * self.layout.rows_per_page() + row;
                if let Some(chapter) = chapters.get(index).cloned() {
                    return self.download_and_read(&source, &manga, &chapter);
                }
                Ok(Flow::Continue)
            }
            Screen::UpdatePrompt { .. } => {
                self.install_update()?;
                Ok(Flow::Continue)
            }
            Screen::Message { .. } => self.pop(),
        }
    }

    // --- screen builders ---

    fn open_library(&mut self) -> Result<()> {
        if !self.library_dir.exists() {
            std::fs::create_dir_all(&self.library_dir).with_context(|| {
                format!(
                    "couldn't create library directory {}",
                    self.library_dir.display()
                )
            })?;
        }
        let entries = Library::new(&self.library_dir).scan()?;
        self.push(Screen::Library { entries, page: 0 })
    }

    fn build_source_rows(&self) -> Result<Vec<SourceRow>> {
        let installed = self.gateway.installed_sources()?;
        let mut rows: Vec<SourceRow> = installed
            .iter()
            .cloned()
            .map(SourceRow::Installed)
            .collect();
        rows.push(SourceRow::Separator("— available —".to_string()));
        // A source-list fetch failure must not hide installed sources:
        // surface the error as a row and carry on.
        match self.gateway.available_sources() {
            Ok(available) => {
                for source in available {
                    if !installed.iter().any(|s| s.id == source.id) {
                        rows.push(SourceRow::Available(source));
                    }
                }
            }
            Err(e) => rows.push(SourceRow::Note(format!("couldn't fetch lists: {e:#}"))),
        }
        Ok(rows)
    }

    fn open_sources(&mut self) -> Result<()> {
        let rows = self.build_source_rows()?;
        self.push(Screen::Sources { rows, page: 0 })
    }

    fn install_and_refresh(&mut self, source: &SourceEntry) -> Result<()> {
        self.show_status(&[&format!("Installing {}…", source.name)])?;
        self.gateway
            .install_source(&source.id)
            .with_context(|| format!("failed to install {}", source.name))?;
        // Rebuild the sources screen in place so the new source shows up.
        let rows = self.build_source_rows()?;
        if let Some(screen @ Screen::Sources { .. }) = self.stack.last_mut() {
            *screen = Screen::Sources { rows, page: 0 };
        }
        self.render_current(RefreshMode::Full)
    }

    fn open_manga_list(&mut self, source: &SourceEntry, listing: &str) -> Result<()> {
        self.show_status(&[&format!("Loading {listing}…")])?;
        let mangas = self
            .gateway
            .list_manga(&source.id, listing)
            .with_context(|| format!("failed to load {listing} from {}", source.name))?;
        self.push(Screen::MangaList {
            source: source.clone(),
            listing: listing.to_string(),
            mangas,
            page: 0,
        })
    }

    fn open_chapter_list(&mut self, source: &SourceEntry, manga: &MangaEntry) -> Result<()> {
        self.show_status(&[&format!("Loading chapters of {}…", manga.title)])?;
        let chapters = self
            .gateway
            .chapters(&source.id, &manga.id)
            .with_context(|| format!("failed to load chapters of {}", manga.title))?;
        self.push(Screen::ChapterList {
            source: source.clone(),
            manga: manga.clone(),
            chapters,
            page: 0,
        })
    }

    fn check_updates(&mut self) -> Result<()> {
        self.show_status(&["Checking for updates…"])?;
        let check = self
            .gateway
            .check_updates()
            .context("update check failed")?;
        if check.available {
            self.push(Screen::UpdatePrompt {
                body: format!("{}\nTap to install, or Back to skip.", check.message),
            })
        } else {
            self.push(Screen::Message {
                title: "Updates".to_string(),
                body: check.message,
            })
        }
    }

    fn install_update(&mut self) -> Result<()> {
        self.show_status(&["Downloading update…"])?;
        let body = self
            .gateway
            .install_update()
            .context("update install failed")?;
        self.pop()?; // leave the prompt
        self.push(Screen::Message {
            title: "Updates".to_string(),
            body,
        })
    }

    fn download_and_read(
        &mut self,
        source: &SourceEntry,
        manga: &MangaEntry,
        chapter: &ChapterEntry,
    ) -> Result<Flow> {
        let label = chapter.label();
        self.show_status(&[&format!("Downloading {label}…")])?;

        let layout = self.layout;
        let manga_title = manga.title.clone();
        // Borrow the display for live progress while the gateway (a
        // disjoint field) does the download.
        let display = &mut self.display;
        let mut last_drawn = usize::MAX;
        let mut progress = move |done: usize, total: usize| {
            // Re-render every few pages: e-ink refreshes are not free.
            if done == 0 || done == total || done.saturating_sub(last_drawn) >= 3 {
                last_drawn = done;
                let page = compose_status(
                    &layout,
                    &[
                        &manga_title,
                        &label,
                        &format!("Downloading… page {done}/{total}"),
                    ],
                );
                let _ = display.blit(&page, 0);
                let _ = display.flush(RefreshMode::Partial);
            }
        };
        let cbz_path = self.gateway.download_chapter(
            &source.id,
            &manga.id,
            &chapter.id,
            &self.library_dir,
            &mut progress,
        )?;

        // Taps queued while the download ran were aimed at the (now gone)
        // chapter list — drop them so they don't flip pages in the reader.
        self.input.discard_queued();

        let key = progress_key(&self.library_dir, &cbz_path);
        if self.run_reader(&cbz_path, &key)? {
            Ok(Flow::Continue)
        } else {
            Ok(Flow::Quit)
        }
    }

    // --- library shelf ---

    fn shelf_layout(&self) -> ShelfLayout {
        ShelfLayout::new(
            self.layout.width,
            self.layout.content_height(),
            SHELF_COLUMNS,
        )
    }

    fn tap_library_cell(
        &mut self,
        entries: &[LibraryEntry],
        page: usize,
        x: u32,
        y: u32,
    ) -> Result<Flow> {
        let shelf = self.shelf_layout();
        let capacity = shelf.capacity().max(1);
        let local_y = y.saturating_sub(self.layout.content_top());
        let visible = entries.len().saturating_sub(page * capacity).min(capacity);
        for cell in 0..visible {
            let (cx, cy) = shelf.cell_origin(cell);
            if x >= cx
                && x < cx + shelf.cell_width()
                && local_y >= cy
                && local_y < cy + shelf.cell_height()
            {
                let entry = entries[page * capacity + cell].clone();
                let keep_running = self.run_reader(&entry.path, &entry.relative_path)?;
                return Ok(if keep_running {
                    Flow::Continue
                } else {
                    Flow::Quit
                });
            }
        }
        Ok(Flow::Continue)
    }

    // --- reader session ---

    /// Open a CBZ in the reader and loop until the user taps the center
    /// zone (back) or the input source ends. Returns `false` when the app
    /// should quit (input closed).
    fn run_reader(&mut self, path: &Path, key: &str) -> Result<bool> {
        let doc =
            CbzDocument::open(path).with_context(|| format!("couldn't open {}", path.display()))?;
        let progress_file = progress_path(&self.library_dir);
        let mut store = ProgressStore::load(&progress_file).unwrap_or_default();

        let layout = self.layout;
        let rotation = self.reader_rotation;
        let mut keep_running = true;
        {
            let mut reader = Reader::new(doc, &mut self.display, self.reader_fit, rotation);
            reader.resume_from(&store, key);
            reader.show_current_page()?;
            loop {
                let Ok(UiEvent::Tap { x, y }) = self.input.next_event() else {
                    keep_running = false;
                    break;
                };
                // Tap zones follow the reading orientation, not the panel.
                match layout.reader_zone_rotated(x, y, rotation) {
                    ReaderZone::NextPage => {
                        reader.next_page()?;
                    }
                    ReaderZone::PrevPage => {
                        reader.prev_page()?;
                    }
                    ReaderZone::Back => break,
                }
            }
            reader.save_progress(&mut store, key);
        }
        store.save(&progress_file)?;

        if keep_running {
            // Repaint the screen the reader covered.
            self.render_current(RefreshMode::Full)?;
        }
        Ok(keep_running)
    }

    // --- rendering ---

    fn show_status(&mut self, lines: &[&str]) -> Result<()> {
        let page = compose_status(&self.layout, lines);
        self.display.blit(&page, 0)?;
        self.display.flush(RefreshMode::Full)?;
        Ok(())
    }

    fn render_current(&mut self, mode: RefreshMode) -> Result<()> {
        let page = match self.compose_current() {
            Ok(page) => page,
            // Composition failures (e.g. an unreadable CBZ) become an error
            // screen rather than a crash.
            Err(e) => {
                *self.stack.last_mut().expect("stack never empty") = Screen::Message {
                    title: "Error".to_string(),
                    body: format!("{e:#}"),
                };
                self.compose_current()?
            }
        };
        self.display.blit(&page, 0)?;
        self.display.flush(mode)?;
        Ok(())
    }

    fn compose_current(&self) -> Result<GrayPage> {
        let l = &self.layout;
        let per_page = l.rows_per_page();
        let screen = self.stack.last().expect("stack never empty");
        Ok(match screen {
            Screen::Home => {
                let rows: Vec<(String, bool)> =
                    HOME_ROWS.iter().map(|r| (r.to_string(), true)).collect();
                compose_list(l, "gideon", &rows, 0, 1)
            }
            Screen::Library { entries, page } => self.compose_library(entries, *page)?,
            Screen::Sources { rows, page } => {
                let labels: Vec<(String, bool)> = paged(rows, *page, per_page)
                    .iter()
                    .map(|r| r.label())
                    .collect();
                compose_list(l, "Sources", &labels, *page, l.page_count(rows.len()))
            }
            Screen::Listings { source } => {
                let rows = vec![("Popular".to_string(), true), ("Latest".to_string(), true)];
                compose_list(l, &source.name, &rows, 0, 1)
            }
            Screen::MangaList {
                source,
                listing,
                mangas,
                page,
            } => {
                let rows: Vec<(String, bool)> = paged(mangas, *page, per_page)
                    .iter()
                    .map(|m| (m.title.clone(), true))
                    .collect();
                let title = format!("{} — {listing}", source.name);
                compose_list(l, &title, &rows, *page, l.page_count(mangas.len()))
            }
            Screen::ChapterList {
                manga,
                chapters,
                page,
                ..
            } => {
                let rows: Vec<(String, bool)> = paged(chapters, *page, per_page)
                    .iter()
                    .map(|c| (c.label(), true))
                    .collect();
                compose_list(l, &manga.title, &rows, *page, l.page_count(chapters.len()))
            }
            Screen::UpdatePrompt { body } => compose_message(l, "Update available", body),
            Screen::Message { title, body } => compose_message(l, title, body),
        })
    }

    fn compose_library(&self, entries: &[LibraryEntry], page: usize) -> Result<GrayPage> {
        let l = &self.layout;
        let shelf = self.shelf_layout();
        let capacity = shelf.capacity().max(1);
        let page_count = entries.len().div_ceil(capacity).max(1);
        let store = ProgressStore::load(&progress_path(&self.library_dir)).unwrap_or_default();

        let mut canvas = compose_chrome(l, "Library", page, page_count);
        if entries.is_empty() {
            draw_text(
                &mut canvas,
                l.pad,
                l.row_top(0) + (l.row_h - l.text_px as u32) / 2,
                l.text_px,
                "No manga yet — download chapters via Browse sources,",
                l.width - 2 * l.pad,
                false,
            );
            draw_text(
                &mut canvas,
                l.pad,
                l.row_top(1) + (l.row_h - l.text_px as u32) / 2,
                l.text_px,
                "or copy .cbz files into the Manga folder over USB.",
                l.width - 2 * l.pad,
                false,
            );
            return Ok(canvas);
        }

        let mut shelf_entries = Vec::new();
        for entry in entries.iter().skip(page * capacity).take(capacity) {
            let cover = CbzDocument::open(&entry.path)
                .and_then(|mut doc| doc.decode_page(0))
                .unwrap_or_else(|_| placeholder_cover());
            let progress = store.get(&entry.relative_path).map(|p| {
                if p.total_pages == 0 {
                    0.0
                } else {
                    (p.current_page + 1) as f32 / p.total_pages as f32
                }
            });
            shelf_entries.push(ShelfEntry { cover, progress });
        }
        let grid = compose_shelf(&shelf_entries, &shelf);
        copy_into(&mut canvas, &grid, 0, l.content_top());
        Ok(canvas)
    }
}

// --- pure composition helpers ---

fn paged<T>(items: &[T], page: usize, per_page: usize) -> &[T] {
    let start = (page * per_page).min(items.len());
    let end = (start + per_page).min(items.len());
    &items[start..end]
}

/// White canvas with the title bar and bottom navigation bar drawn.
fn compose_chrome(l: &UiLayout, title: &str, page: usize, page_count: usize) -> GrayPage {
    let mut canvas = GrayPage::new_white(l.width, l.height);
    let text_y = |top: u32, h: u32| top + h.saturating_sub(l.text_px as u32 + 4) / 2;

    // Title bar with a separator line.
    draw_text(
        &mut canvas,
        l.pad,
        text_y(0, l.title_h),
        l.text_px,
        title,
        l.width.saturating_sub(2 * l.pad) * 2 / 3,
        true,
    );
    if page_count > 1 {
        let label = format!("{}/{}", page + 1, page_count);
        let w = measure_text(l.text_px, &label, false).min(l.width / 3);
        draw_text(
            &mut canvas,
            l.width.saturating_sub(w + l.pad),
            text_y(0, l.title_h),
            l.text_px,
            &label,
            w,
            false,
        );
    }
    hline(&mut canvas, l.title_h - 1, 0x55);

    // Bottom navigation bar: [Back] [Prev] [Next] thirds.
    hline(&mut canvas, l.nav_top(), 0x55);
    let third = (l.width / 3).max(1);
    let nav_y = text_y(l.nav_top(), l.nav_h);
    draw_text(
        &mut canvas,
        l.pad,
        nav_y,
        l.text_px,
        "< Back",
        third.saturating_sub(l.pad),
        false,
    );
    if page_count > 1 {
        draw_text(
            &mut canvas,
            third + l.pad,
            nav_y,
            l.text_px,
            "Prev",
            third.saturating_sub(l.pad),
            false,
        );
        draw_text(
            &mut canvas,
            2 * third + l.pad,
            nav_y,
            l.text_px,
            "Next",
            third.saturating_sub(l.pad),
            false,
        );
    }
    canvas
}

/// A list screen: chrome + one text row per entry, with separators.
fn compose_list(
    l: &UiLayout,
    title: &str,
    rows: &[(String, bool)],
    page: usize,
    page_count: usize,
) -> GrayPage {
    let mut canvas = compose_chrome(l, title, page, page_count);
    for (i, (text, bold)) in rows.iter().take(l.rows_per_page()).enumerate() {
        let top = l.row_top(i);
        draw_text(
            &mut canvas,
            l.pad,
            top + l.row_h.saturating_sub(l.text_px as u32 + 4) / 2,
            l.text_px,
            text,
            l.width.saturating_sub(2 * l.pad),
            *bold,
        );
        let sep_y = top + l.row_h - 1;
        if sep_y < l.nav_top() {
            hline(&mut canvas, sep_y, 0xDD);
        }
    }
    canvas
}

/// A full-screen transient status (e.g. "Downloading… page 3/20").
fn compose_status(l: &UiLayout, lines: &[&str]) -> GrayPage {
    let mut canvas = GrayPage::new_white(l.width, l.height);
    let start = l.height / 3;
    for (i, line) in lines.iter().enumerate() {
        draw_text(
            &mut canvas,
            l.pad,
            start + i as u32 * l.row_h,
            l.text_px,
            line,
            l.width.saturating_sub(2 * l.pad),
            i == lines.len() - 1,
        );
    }
    canvas
}

/// An error/info screen: chrome + word-wrapped body + a Back row.
fn compose_message(l: &UiLayout, title: &str, body: &str) -> GrayPage {
    let mut canvas = compose_chrome(l, title, 0, 1);
    let max_w = l.width.saturating_sub(2 * l.pad);
    let mut row = 0usize;
    for line in wrap_text(l.text_px, body, max_w) {
        if row + 2 > l.rows_per_page() {
            break;
        }
        draw_text(
            &mut canvas,
            l.pad,
            l.row_top(row) + l.row_h.saturating_sub(l.text_px as u32 + 4) / 2,
            l.text_px,
            &line,
            max_w,
            false,
        );
        row += 1;
    }
    draw_text(
        &mut canvas,
        l.pad,
        l.row_top(row + 1) + l.row_h.saturating_sub(l.text_px as u32 + 4) / 2,
        l.text_px,
        "< Back",
        max_w,
        true,
    );
    canvas
}

/// Greedy word wrap by measured pixel width.
fn wrap_text(px: f32, text: &str, max_w: u32) -> Vec<String> {
    let mut lines = Vec::new();
    for raw_line in text.lines() {
        let mut current = String::new();
        for word in raw_line.split_whitespace() {
            let candidate = if current.is_empty() {
                word.to_string()
            } else {
                format!("{current} {word}")
            };
            if measure_text(px, &candidate, false) <= max_w || current.is_empty() {
                current = candidate;
            } else {
                lines.push(std::mem::take(&mut current));
                current = word.to_string();
            }
        }
        lines.push(current);
    }
    lines
}

fn hline(canvas: &mut GrayPage, y: u32, value: u8) {
    if y >= canvas.height {
        return;
    }
    let start = (y * canvas.width) as usize;
    canvas.pixels[start..start + canvas.width as usize].fill(value);
}

/// Copy `src` into `dst` at (`off_x`, `off_y`), clipped to `dst`.
fn copy_into(dst: &mut GrayPage, src: &GrayPage, off_x: u32, off_y: u32) {
    let copy_w = src.width.min(dst.width.saturating_sub(off_x));
    let copy_h = src.height.min(dst.height.saturating_sub(off_y));
    for y in 0..copy_h {
        let src_start = (y * src.width) as usize;
        let dst_start = ((off_y + y) * dst.width + off_x) as usize;
        dst.pixels[dst_start..dst_start + copy_w as usize]
            .copy_from_slice(&src.pixels[src_start..src_start + copy_w as usize]);
    }
}

fn placeholder_cover() -> image::DynamicImage {
    image::DynamicImage::ImageLuma8(image::GrayImage::from_pixel(3, 4, image::Luma([0xCC])))
}

/// Progress file shared with `gideon library` / `gideon read`.
pub(crate) fn progress_path(library_dir: &Path) -> PathBuf {
    library_dir.join(".gideon").join("progress.json")
}

/// Progress key for a document: its path relative to the library root.
fn progress_key(library_dir: &Path, path: &Path) -> String {
    path.strip_prefix(library_dir)
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .unwrap_or_else(|_| path.display().to_string())
}
