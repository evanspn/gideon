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
pub use layout::{Key, ReaderZone, TapTarget, UiLayout};

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use gideon_core::{CbzDocument, Library, LibraryEntry, ProgressStore};
use gideon_device::{Display, InputSource, LightControl, RefreshMode, UiEvent};
use gideon_render::shelf::{compose_shelf, compose_shelf_rgb, ShelfEntry, ShelfLayout};
use gideon_render::text::{draw_text, measure_text};
use gideon_render::{FitMode, GrayPage, RgbPage};

use crate::reader::Reader;

const HOME_ROWS: [&str; 5] = [
    "Library",
    "Search",
    "Browse sources",
    "Settings",
    "Check for updates",
];
const SHELF_COLUMNS: u32 = 3;

/// Values the Settings screen cycles through per tap.
const PREDOWNLOAD_STEPS: [u32; 5] = [0, 1, 2, 3, 5];
const STORAGE_LIMIT_STEPS: [u64; 4] = [
    500 * 1024 * 1024,
    1024 * 1024 * 1024,
    2 * 1024 * 1024 * 1024,
    5 * 1024 * 1024 * 1024,
];

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
    /// On-screen keyboard for a manga search. `source: None` searches every
    /// installed source (the Home-screen entry point — e-ink refreshes cost
    /// a second each, so search must not hide behind a source picker).
    Search {
        source: Option<SourceEntry>,
        query: String,
    },
    /// Global search results: each row knows which source it came from.
    SearchResults {
        query: String,
        results: Vec<(SourceEntry, MangaEntry)>,
        page: usize,
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
    /// Context menu for a library book (long press on its card).
    BookMenu {
        entry: LibraryEntry,
        series_dir: String,
    },
    /// Profile picker, opened from the left half of Home's title bar.
    ProfileMenu {
        profiles: Vec<String>,
    },
    /// On-screen keyboard for naming a new profile; the action key creates
    /// it and switches to it.
    NewProfile {
        name: String,
    },
    /// Device-global settings (NOT per profile): each tap cycles a value
    /// and saves settings.json immediately.
    Settings,
    /// Restart/close menu, opened from the power symbol on Home.
    PowerMenu,
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

/// Why the UI loop ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exit {
    /// Close the app: the launcher takes over (back to Nickel).
    Close,
    /// Restart the app in place (exec of the current binary).
    Restart,
}

enum Flow {
    Continue,
    Quit(Exit),
}

/// What the suspend hook did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SleepResult {
    /// The device suspended and has since woken up.
    Slept,
    /// Suspend was skipped (e.g. charger plugged in); still awake.
    Skipped,
}

/// Suspend-to-RAM hook: blocks until the device wakes up again. The UI
/// saves state before calling it and repaints in full after it returns.
pub type SleepFn = Box<dyn FnMut() -> Result<SleepResult>>;

/// Ignore sleep requests this soon after a wake: the press that woke the
/// device can be delivered *after* the post-wake input drain (KOReader hit
/// the same race), and must not bounce us straight back into suspend.
const SLEEP_DEBOUNCE: std::time::Duration = std::time::Duration::from_secs(1);

/// How long the "staying awake" notice stays up when suspend is skipped.
const SKIP_NOTICE_HOLD: std::time::Duration = std::time::Duration::from_millis(1200);

/// Force a full e-ink refresh every Nth keyboard repaint, so ghosting
/// can't accumulate over a long editing session.
const KEYBOARD_FULL_REFRESH_INTERVAL: u32 = 8;

pub struct UiApp<D: Display, I: InputSource, G: SourceGateway> {
    display: D,
    input: I,
    gateway: G,
    /// The profile-resolved library directory: every scan, download and
    /// progress path goes through this.
    library_dir: PathBuf,
    /// The library ROOT passed at startup; profile dirs hang off it (the
    /// "default" profile IS the root).
    base_library: PathBuf,
    /// Active profile name (settings.json `active_profile`).
    active_profile: String,
    layout: UiLayout,
    stack: Vec<Screen>,
    /// Reader fit mode (from settings.json `reader_fit`).
    reader_fit: FitMode,
    /// Reader rotation in degrees (from settings.json `reader_rotation`).
    reader_rotation: u32,
    /// Suspend hook for [`UiEvent::Sleep`]; `None` (tests, headless) means
    /// sleep events are ignored.
    sleeper: Option<SleepFn>,
    /// When the device last woke up, for [`SLEEP_DEBOUNCE`].
    last_wake: Option<std::time::Instant>,
    /// Keyboard repaints since the search screen opened, for the periodic
    /// anti-ghosting full refresh.
    keyboard_paints: u32,
    /// Frontlight hook for the reader's edge slides; `None` (tests,
    /// headless) means swipes are ignored.
    lights: Option<Box<dyn LightControl>>,
    /// Where settings.json lives, for persisting in-reader changes
    /// (rotation lock). `None` skips persistence.
    settings_dir: Option<PathBuf>,
}

impl<D: Display, I: InputSource, G: SourceGateway> UiApp<D, I, G> {
    pub fn new(display: D, input: I, gateway: G, library_dir: PathBuf) -> Self {
        let layout = UiLayout::new(display.width(), display.height());
        Self {
            display,
            input,
            gateway,
            base_library: library_dir.clone(),
            library_dir,
            active_profile: "default".to_string(),
            layout,
            stack: vec![Screen::Home],
            reader_fit: FitMode::Contain,
            reader_rotation: 0,
            sleeper: None,
            last_wake: None,
            keyboard_paints: 0,
            lights: None,
            settings_dir: None,
        }
    }

    /// Start in this profile (resolved from settings.json at startup):
    /// the library directory becomes the profile's subdirectory.
    pub fn with_profile(mut self, name: &str) -> Self {
        self.active_profile = name.to_string();
        self.library_dir = profile_library_dir(&self.base_library, name);
        self
    }

    /// Apply the reader-related settings (fit mode and rotation).
    pub fn with_reader_settings(mut self, fit: FitMode, rotation: u32) -> Self {
        self.reader_fit = fit;
        self.reader_rotation = rotation;
        self
    }

    /// Install the suspend hook (power button / sleep cover).
    pub fn with_sleeper(mut self, sleeper: SleepFn) -> Self {
        self.sleeper = Some(sleeper);
        self
    }

    /// Install the frontlight hook (reader edge slides).
    pub fn with_lights(mut self, lights: Box<dyn LightControl>) -> Self {
        self.lights = Some(lights);
        self
    }

    /// Persist in-reader setting changes (rotation lock) to this directory.
    pub fn with_settings_dir(mut self, dir: PathBuf) -> Self {
        self.settings_dir = Some(dir);
        self
    }

    /// The underlying display (for tests and headless screenshots).
    pub fn display(&self) -> &D {
        &self.display
    }

    #[cfg(test)]
    pub(crate) fn gateway(&self) -> &G {
        &self.gateway
    }

    #[cfg(test)]
    pub(crate) fn input(&self) -> &I {
        &self.input
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

    /// Main loop: render, then process events until the user quits through
    /// the power menu (or the input source ends). Returns how to exit.
    pub fn run(&mut self) -> Result<Exit> {
        self.render_current(RefreshMode::Full)?;
        loop {
            match self.input.next_event() {
                Err(_) => return Ok(Exit::Close), // input source closed
                Ok(UiEvent::Tap { x, y }) => match self.handle_tap(x, y) {
                    Ok(Flow::Quit(exit)) => return Ok(exit),
                    Ok(Flow::Continue) => {}
                    // The UI must never die on an error: show it instead.
                    Err(e) => self.show_error(&e)?,
                },
                // Edge slides only matter in the reader; elsewhere a swipe
                // is just an overshot tap — ignore it.
                Ok(UiEvent::Swipe { .. }) => {}
                Ok(UiEvent::LongPress { x, y }) => match self.handle_long_press(x, y) {
                    Ok(Flow::Quit(exit)) => return Ok(exit),
                    Ok(Flow::Continue) => {}
                    Err(e) => self.show_error(&e)?,
                },
                // Physical page-turn buttons page through whatever list is
                // on screen (library shelf, sources, results…).
                Ok(UiEvent::PageForward) => {
                    if let Err(e) = self.flip_page(1) {
                        self.show_error(&e)?;
                    }
                }
                Ok(UiEvent::PageBack) => {
                    if let Err(e) = self.flip_page(-1) {
                        self.show_error(&e)?;
                    }
                }
                Ok(UiEvent::Sleep) => {
                    if let Err(e) = self.sleep_now() {
                        self.show_error(&e)?;
                    }
                }
            }
        }
    }

    /// Suspend via the sleep hook (no-op without one), then repaint: the
    /// panel may have been dimmed or ghosted while asleep, and the key
    /// press that woke us must not fire an action.
    fn sleep_now(&mut self) -> Result<()> {
        if self.sleeper.is_none() || self.sleep_debounced() {
            return Ok(());
        }
        // E-ink keeps its image with zero power: this stays on the panel
        // for the whole nap, and doubles as feedback that the cover close
        // / button press registered.
        self.show_status(&["Sleeping…", "Press power or open the cover to wake."])?;
        let result = self.sleeper.as_mut().expect("checked above")();
        self.last_wake = Some(std::time::Instant::now());
        if matches!(result, Ok(SleepResult::Skipped)) {
            // Pressing power while plugged in does nothing visible
            // otherwise — say why before restoring the screen.
            self.show_status(&["Plugged in — staying awake."])?;
            std::thread::sleep(SKIP_NOTICE_HOLD);
            self.render_current(RefreshMode::Full)?;
            return Ok(());
        }
        // The kernel may have re-registered the input nodes across the
        // suspend — reopen them, then drop the key press that woke us.
        self.input.refresh_devices();
        self.input.discard_queued();
        // Suspend powers the frontlight down; bring it back to its levels.
        if let Some(lights) = self.lights.as_mut() {
            lights.reapply();
        }
        self.render_current(RefreshMode::Full)?;
        result.map(|_| ())
    }

    /// `true` while the post-wake debounce window is open: the key press
    /// that woke the device can arrive after the input drain and must not
    /// bounce us straight back into suspend.
    fn sleep_debounced(&self) -> bool {
        matches!(self.last_wake, Some(t) if t.elapsed() < SLEEP_DEBOUNCE)
    }

    // --- navigation ---

    fn push(&mut self, screen: Screen) -> Result<()> {
        self.stack.push(screen);
        self.render_current(RefreshMode::Full)
    }

    fn pop(&mut self) -> Result<Flow> {
        if self.stack.len() <= 1 {
            // Home has no Back: quitting goes through the power menu.
            return Ok(Flow::Continue);
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
            TapTarget::Title => {
                if matches!(self.screen(), Screen::Home) {
                    if x >= self.layout.width.saturating_sub(self.layout.title_h * 2) {
                        // The power symbol lives in Home's top-right corner.
                        self.push(Screen::PowerMenu)?;
                    } else if x < self.layout.width / 2 {
                        // The active profile name sits in the title's left
                        // half: tapping it opens the profile picker.
                        self.open_profile_menu()?;
                    }
                }
                Ok(Flow::Continue)
            }
        }
    }

    /// Long press: a library card opens its book menu; a chapter row
    /// downloads that chapter without opening the reader. Everywhere else
    /// a long press is just a slow tap.
    fn handle_long_press(&mut self, x: u32, y: u32) -> Result<Flow> {
        let screen = self.stack.last().cloned().expect("stack never empty");
        match screen {
            Screen::Library { entries, page } => {
                let Some(entry) = self.library_cell_at(&entries, page, x, y) else {
                    return Ok(Flow::Continue);
                };
                let series_dir = entry
                    .relative_path
                    .split('/')
                    .next()
                    .unwrap_or(&entry.relative_path)
                    .to_string();
                self.push(Screen::BookMenu { entry, series_dir })?;
                Ok(Flow::Continue)
            }
            Screen::ChapterList {
                source,
                manga,
                chapters,
                page,
            } => {
                // Long press on a chapter row: download it and stay on the
                // list — for stocking up before going offline.
                if let TapTarget::Row(row) = self.layout.tap_target(x, y) {
                    let index = page * self.layout.rows_per_page() + row;
                    if let Some(chapter) = chapters.get(index).cloned() {
                        self.download_to_library(&source, &manga, &chapter)?;
                        self.input.discard_taps();
                        self.render_current(RefreshMode::Full)?;
                    }
                }
                Ok(Flow::Continue)
            }
            _ => self.handle_tap(x, y),
        }
    }

    /// Open the book menu's "chapters" entry: the source's chapter list
    /// when the series is linked, otherwise the search keyboard prefilled
    /// with the series name so one download can re-link it.
    fn open_series_chapters(&mut self, series_dir: &str) -> Result<()> {
        let index = gideon_core::SeriesIndex::load(&self.library_dir);
        let Some(origin) = index.get(series_dir) else {
            self.keyboard_paints = 0;
            return self.push(Screen::Search {
                source: None,
                query: series_dir.to_ascii_lowercase(),
            });
        };
        let source = SourceEntry {
            id: origin.source_id.clone(),
            name: origin.source_name.clone(),
        };
        let manga = MangaEntry {
            id: origin.manga_id.clone(),
            title: origin.manga_title.clone(),
            cover_url: origin.cover_url.clone(),
        };
        self.open_chapter_list(&source, &manga)
    }

    /// Rebuild the Library screen beneath the book menu after a delete.
    fn refresh_library_after_delete(&mut self) -> Result<()> {
        let entries = Library::new(&self.library_dir).scan()?;
        self.stack.pop(); // leave the book menu
        if let Some(screen @ Screen::Library { .. }) = self.stack.last_mut() {
            *screen = Screen::Library { entries, page: 0 };
        }
        self.render_current(RefreshMode::Full)
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
            Screen::SearchResults { results, page, .. } => (page, results.len().div_ceil(per_page)),
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
                    self.open_global_search()?;
                    Ok(Flow::Continue)
                }
                2 => {
                    self.open_sources()?;
                    Ok(Flow::Continue)
                }
                3 => {
                    self.push(Screen::Settings)?;
                    Ok(Flow::Continue)
                }
                4 => {
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
                    2 => {
                        self.keyboard_paints = 0;
                        self.push(Screen::Search {
                            source: Some(source),
                            query: String::new(),
                        })?;
                        return Ok(Flow::Continue);
                    }
                    _ => return Ok(Flow::Continue),
                };
                self.open_manga_list(&source, listing)?;
                Ok(Flow::Continue)
            }
            Screen::Search { source, query } => {
                self.tap_keyboard(&source, &query, x, y)?;
                Ok(Flow::Continue)
            }
            Screen::SearchResults { results, page, .. } => {
                let index = page * self.layout.rows_per_page() + row;
                if let Some((source, manga)) = results.get(index).cloned() {
                    self.open_chapter_list(&source, &manga)?;
                }
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
            Screen::BookMenu { entry, series_dir } => {
                match row {
                    0 => self.open_series_chapters(&series_dir)?,
                    1 => {
                        // Delete this chapter's file; drop it from the
                        // series' download history.
                        std::fs::remove_file(&entry.path)
                            .with_context(|| format!("couldn't delete {}", entry.path.display()))?;
                        if let Some(file) = entry.path.file_name() {
                            let mut index = gideon_core::SeriesIndex::load(&self.library_dir);
                            index.forget_download(&series_dir, &file.to_string_lossy());
                            let _ = index.save(&self.library_dir);
                        }
                        // Remove the series dir too when it's now empty.
                        if let Some(parent) = entry.path.parent() {
                            if parent != self.library_dir
                                && std::fs::read_dir(parent)
                                    .map(|mut d| d.next().is_none())
                                    .unwrap_or(false)
                            {
                                let _ = std::fs::remove_dir(parent);
                            }
                        }
                        self.refresh_library_after_delete()?;
                    }
                    2 => {
                        // Delete the whole series directory.
                        let target = entry
                            .path
                            .parent()
                            .filter(|p| *p != self.library_dir)
                            .map(|p| p.to_path_buf());
                        match target {
                            Some(dir) => std::fs::remove_dir_all(&dir)
                                .with_context(|| format!("couldn't delete {}", dir.display()))?,
                            None => std::fs::remove_file(&entry.path).with_context(|| {
                                format!("couldn't delete {}", entry.path.display())
                            })?,
                        }
                        let mut index = gideon_core::SeriesIndex::load(&self.library_dir);
                        index.remove(&series_dir);
                        let _ = index.save(&self.library_dir);
                        self.refresh_library_after_delete()?;
                    }
                    _ => {}
                }
                Ok(Flow::Continue)
            }
            Screen::Settings => {
                self.tap_setting(row)?;
                Ok(Flow::Continue)
            }
            Screen::ProfileMenu { profiles } => {
                if let Some(name) = profiles.get(row).cloned() {
                    if name == self.active_profile {
                        self.pop()?; // already there — just close the menu
                    } else {
                        self.switch_profile(&name)?;
                    }
                } else if row == profiles.len() {
                    self.keyboard_paints = 0;
                    self.push(Screen::NewProfile {
                        name: String::new(),
                    })?;
                }
                Ok(Flow::Continue)
            }
            Screen::NewProfile { name } => {
                self.tap_new_profile(&name, x, y)?;
                Ok(Flow::Continue)
            }
            Screen::PowerMenu => match row {
                0 => Ok(Flow::Quit(Exit::Restart)),
                1 => Ok(Flow::Quit(Exit::Close)),
                _ => Ok(Flow::Continue),
            },
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

    /// Handle a tap on the search keyboard: edit the query in place
    /// (partial refresh) or run the search.
    fn tap_keyboard(
        &mut self,
        source: &Option<SourceEntry>,
        query: &str,
        x: u32,
        y: u32,
    ) -> Result<()> {
        let key = self.layout.key_at(x, y);
        if key == Some(Key::Search) {
            let trimmed = query.trim();
            if !trimmed.is_empty() {
                self.run_search(source, trimmed)?;
            }
            return Ok(());
        }
        if let Some(q) = key.and_then(|key| apply_key_edit(query, key)) {
            if let Some(Screen::Search { query, .. }) = self.stack.last_mut() {
                *query = q;
            }
            self.keyboard_repaint()?;
        }
        Ok(())
    }

    /// Handle a tap on the new-profile keyboard: edit the name in place,
    /// or (action key) create the profile and switch to it.
    fn tap_new_profile(&mut self, name: &str, x: u32, y: u32) -> Result<()> {
        let key = self.layout.key_at(x, y);
        if key == Some(Key::Search) {
            let trimmed = name.trim().to_string();
            if !trimmed.is_empty() {
                self.switch_profile(&trimmed)?;
            }
            return Ok(());
        }
        if let Some(n) = key.and_then(|key| apply_key_edit(name, key)) {
            if let Some(Screen::NewProfile { name }) = self.stack.last_mut() {
                *name = n;
            }
            self.keyboard_repaint()?;
        }
        Ok(())
    }

    /// Repaint after a keyboard edit. Mostly-partial refreshes keep typing
    /// fast, but ghosting accumulates — flash the panel clean every Nth
    /// repaint.
    fn keyboard_repaint(&mut self) -> Result<()> {
        self.keyboard_paints += 1;
        let mode = if self
            .keyboard_paints
            .is_multiple_of(KEYBOARD_FULL_REFRESH_INTERVAL)
        {
            RefreshMode::Full
        } else {
            RefreshMode::Partial
        };
        self.render_current(mode)
    }

    // --- settings screen ---

    /// Cycle the setting on row `row` to its next value, persist
    /// settings.json immediately (atomic save) and repaint in place.
    fn tap_setting(&mut self, row: usize) -> Result<()> {
        let mut settings = self.load_settings();
        match row {
            0 => {
                settings.predownload_unread_chapters =
                    cycle(&PREDOWNLOAD_STEPS, settings.predownload_unread_chapters);
            }
            1 => {
                settings.storage_size_limit = gideon_core::StorageSize(cycle(
                    &STORAGE_LIMIT_STEPS,
                    settings.storage_size_limit.bytes(),
                ));
            }
            2 => {
                settings.reader_fit = match FitMode::from_setting(&settings.reader_fit) {
                    FitMode::FitWidth => "contain",
                    _ => "fit-width",
                }
                .to_string();
                // The next opened book must use the new fit immediately.
                self.reader_fit = FitMode::from_setting(&settings.reader_fit);
            }
            3 => settings.auto_check_updates = !settings.auto_check_updates,
            _ => return Ok(()),
        }
        self.save_settings(&settings);
        self.render_current(RefreshMode::Partial)
    }

    // --- profiles ---

    /// Current settings; defaults when no settings dir is configured
    /// (tests, headless) or the file is unreadable.
    fn load_settings(&self) -> gideon_core::Settings {
        self.settings_dir
            .as_deref()
            .map(|dir| gideon_core::Settings::load(dir).unwrap_or_default())
            .unwrap_or_default()
    }

    /// Persist settings (no-op without a settings dir); a failed save is
    /// logged, never fatal.
    fn save_settings(&self, settings: &gideon_core::Settings) {
        if let Some(dir) = &self.settings_dir {
            if let Err(e) = settings.save(dir) {
                eprintln!("gideon: couldn't save settings: {e}");
            }
        }
    }

    /// Open the profile picker from Home's title bar.
    fn open_profile_menu(&mut self) -> Result<()> {
        let mut profiles = self.load_settings().profiles;
        // Lenient: a hand-edited settings.json may activate a profile the
        // list doesn't know — show it anyway.
        if !profiles.contains(&self.active_profile) {
            profiles.push(self.active_profile.clone());
        }
        self.push(Screen::ProfileMenu { profiles })
    }

    /// Switch to (creating if needed) the named profile: persist the
    /// choice, repoint the library and drop back to a fresh Home — the
    /// whole navigation context (library, downloads) just changed.
    fn switch_profile(&mut self, name: &str) -> Result<()> {
        self.active_profile = name.to_string();
        self.library_dir = profile_library_dir(&self.base_library, name);
        std::fs::create_dir_all(&self.library_dir).with_context(|| {
            format!(
                "couldn't create profile library {}",
                self.library_dir.display()
            )
        })?;
        let mut settings = self.load_settings();
        if !settings.profiles.iter().any(|p| p == name) {
            settings.profiles.push(name.to_string());
        }
        settings.active_profile = name.to_string();
        self.save_settings(&settings);
        self.stack.truncate(1);
        self.render_current(RefreshMode::Full)
    }

    /// Open the global-search keyboard from Home (every installed source).
    fn open_global_search(&mut self) -> Result<()> {
        if self.gateway.installed_sources()?.is_empty() {
            return self.push(Screen::Message {
                title: "Search".to_string(),
                body: "No sources installed yet.\nInstall one under Browse sources first."
                    .to_string(),
            });
        }
        self.keyboard_paints = 0;
        self.push(Screen::Search {
            source: None,
            query: String::new(),
        })
    }

    fn run_search(&mut self, source: &Option<SourceEntry>, query: &str) -> Result<()> {
        match source {
            Some(source) => self.run_source_search(source, query),
            None => self.run_global_search(query),
        }
    }

    /// Search one source; results open as a normal manga list.
    fn run_source_search(&mut self, source: &SourceEntry, query: &str) -> Result<()> {
        self.show_status(&[&format!("Searching for \"{query}\"…")])?;
        let mangas = self
            .gateway
            .search_manga(&source.id, query)
            .with_context(|| format!("search on {} failed", source.name))?;
        if mangas.is_empty() {
            // Stay on the keyboard so the user can refine the query.
            return self.push(Screen::Message {
                title: "Search".to_string(),
                body: format!("No results for \"{query}\"."),
            });
        }
        self.push(Screen::MangaList {
            source: source.clone(),
            listing: format!("\"{query}\""),
            mangas,
            page: 0,
        })
    }

    /// Search every installed source and merge the results. A source that
    /// errors is skipped (its name is noted) — one broken source must not
    /// kill the search.
    fn run_global_search(&mut self, query: &str) -> Result<()> {
        let sources = self.gateway.installed_sources()?;
        let mut results: Vec<(SourceEntry, MangaEntry)> = Vec::new();
        let mut failed: Vec<String> = Vec::new();
        for source in &sources {
            self.show_status(&[
                &format!("Searching for \"{query}\"…"),
                &format!("{}…", source.name),
            ])?;
            match self.gateway.search_manga(&source.id, query) {
                Ok(mangas) => {
                    results.extend(mangas.into_iter().map(|m| (source.clone(), m)));
                }
                Err(e) => {
                    eprintln!("gideon: search on {} failed: {e:#}", source.name);
                    failed.push(source.name.clone());
                }
            }
        }
        if results.is_empty() {
            let mut body = format!("No results for \"{query}\".");
            if !failed.is_empty() {
                body.push_str(&format!("\n(Search failed on: {}.)", failed.join(", ")));
            }
            // Stay on the keyboard so the user can refine the query.
            return self.push(Screen::Message {
                title: "Search".to_string(),
                body,
            });
        }
        self.push(Screen::SearchResults {
            query: query.to_string(),
            results,
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

    /// The on-disk CBZ for a chapter, when it was downloaded before.
    fn downloaded_chapter_path(
        &self,
        source: &SourceEntry,
        manga: &MangaEntry,
        chapter_id: &str,
    ) -> Option<PathBuf> {
        let index = gideon_core::SeriesIndex::load(&self.library_dir);
        let (dir, series) = index.find_manga(&source.id, &manga.id)?;
        let file = series.downloaded.get(chapter_id)?;
        let path = self.library_dir.join(dir).join(file);
        path.exists().then_some(path)
    }

    /// Download a chapter into the library with live progress, recording
    /// the series origin, the chapter file and (once per series) the cover.
    fn download_to_library(
        &mut self,
        source: &SourceEntry,
        manga: &MangaEntry,
        chapter: &ChapterEntry,
    ) -> Result<PathBuf> {
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

        // Remember where this series came from (long press on its card
        // reopens the chapter list) and which chapters are on disk (they
        // open instantly, get a check mark, and survive re-listing).
        if let Some(dir) = cbz_path.parent().and_then(|p| p.file_name()) {
            let dir = dir.to_string_lossy().to_string();
            let mut index = gideon_core::SeriesIndex::load(&self.library_dir);
            index.record(
                &dir,
                gideon_core::SeriesRef {
                    source_id: source.id.clone(),
                    source_name: source.name.clone(),
                    manga_id: manga.id.clone(),
                    manga_title: manga.title.clone(),
                    cover_url: manga.cover_url.clone(),
                    ..gideon_core::SeriesRef::default()
                },
            );
            if let Some(file) = cbz_path.file_name() {
                index.record_download(&dir, &chapter.id, &file.to_string_lossy());
            }
            if let Err(e) = index.save(&self.library_dir) {
                eprintln!("gideon: couldn't save the series index: {e}");
            }

            // Fetch the manga cover once per series: library cards show
            // the real cover art instead of a chapter's first page.
            let cover_path = self.library_dir.join(&dir).join(".cover.jpg");
            if !cover_path.exists() {
                if let Some(url) = manga.cover_url.as_deref() {
                    if let Err(e) = self.gateway.download_cover(url, &cover_path) {
                        eprintln!("gideon: couldn't fetch the cover: {e:#}");
                    }
                }
            }
        }
        Ok(cbz_path)
    }

    fn download_and_read(
        &mut self,
        source: &SourceEntry,
        manga: &MangaEntry,
        chapter: &ChapterEntry,
    ) -> Result<Flow> {
        // Already on disk? Straight into the reader — no network, no wait.
        let cbz_path = match self.downloaded_chapter_path(source, manga, &chapter.id) {
            Some(path) => path,
            None => self.download_to_library(source, manga, chapter)?,
        };

        // Taps queued while the download ran were aimed at the (now gone)
        // chapter list — drop them so they don't flip pages in the reader.
        // A sleep cover closed during the download survives the drain: the
        // device must still suspend instead of sitting awake in a bag.
        self.input.discard_taps();

        let key = progress_key(&self.library_dir, &cbz_path);
        if self.run_reader(&cbz_path, &key)? {
            Ok(Flow::Continue)
        } else {
            Ok(Flow::Quit(Exit::Close))
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

    /// The library entry whose shelf cell contains the tap, if any.
    fn library_cell_at(
        &self,
        entries: &[LibraryEntry],
        page: usize,
        x: u32,
        y: u32,
    ) -> Option<LibraryEntry> {
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
                return Some(entries[page * capacity + cell].clone());
            }
        }
        None
    }

    fn tap_library_cell(
        &mut self,
        entries: &[LibraryEntry],
        page: usize,
        x: u32,
        y: u32,
    ) -> Result<Flow> {
        let Some(entry) = self.library_cell_at(entries, page, x, y) else {
            return Ok(Flow::Continue);
        };
        let keep_running = self.run_reader(&entry.path, &entry.relative_path)?;
        Ok(if keep_running {
            Flow::Continue
        } else {
            Flow::Quit(Exit::Close)
        })
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
        let mut rotation = self.reader_rotation;
        let mut keep_running = true;
        {
            let mut reader = Reader::new(doc, &mut self.display, self.reader_fit, rotation);
            reader.resume_from(&store, key);
            reader.show_current_page()?;
            loop {
                match self.input.next_event() {
                    Err(_) => {
                        keep_running = false;
                        break;
                    }
                    // Tap zones follow the reading orientation, not the panel.
                    Ok(UiEvent::Tap { x, y }) => match layout.reader_zone_rotated(x, y, rotation) {
                        ReaderZone::NextPage => {
                            reader.next_page()?;
                        }
                        ReaderZone::PrevPage => {
                            reader.prev_page()?;
                        }
                        ReaderZone::Back => break,
                    },
                    Ok(UiEvent::PageForward) => {
                        reader.next_page()?;
                    }
                    Ok(UiEvent::PageBack) => {
                        reader.prev_page()?;
                    }
                    // Edge slides (panel coordinates — the physical bezel
                    // edge, regardless of reading rotation): right edge is
                    // brightness, left edge is night-light warmth. Sliding
                    // up increases; the full screen height is the full
                    // 0–100 range.
                    Ok(UiEvent::Swipe { x0, y0, x1, y1 }) => {
                        let edge = (layout.width / 8).max(1);
                        let on_right = x0 >= layout.width - edge && x1 >= layout.width - edge;
                        let on_left = x0 < edge && x1 < edge;
                        if !on_right && !on_left {
                            // Mid-screen gestures follow the READING
                            // orientation (taps already do): swipe down to
                            // leave the manga, swipe up to rotate 90°
                            // clockwise and lock it (persisted) — for
                            // reading on your side in bed. Both demand
                            // deliberate travel (a quarter of the reading
                            // height): a sloppy page-turn tap drifting past
                            // the 30px slop must never exit, and certainly
                            // never rotate-and-lock the whole reader.
                            let (mx0, my0) = layout::map_reader_tap(
                                x0,
                                y0,
                                layout.width,
                                layout.height,
                                rotation,
                            );
                            let (mx1, my1) = layout::map_reader_tap(
                                x1,
                                y1,
                                layout.width,
                                layout.height,
                                rotation,
                            );
                            let reading_h = if rotation % 180 == 90 {
                                layout.width
                            } else {
                                layout.height
                            };
                            let min_travel = (reading_h / 4).max(1);
                            let vertical = my0.abs_diff(my1) > mx0.abs_diff(mx1);
                            if my1 > my0 && vertical && my1 - my0 >= min_travel {
                                break;
                            }
                            if my0 > my1 && vertical && my0 - my1 >= min_travel {
                                rotation = (rotation + 90) % 360;
                                reader.set_rotation(rotation);
                                self.reader_rotation = rotation;
                                if let Some(dir) = &self.settings_dir {
                                    let mut settings =
                                        gideon_core::Settings::load(dir).unwrap_or_default();
                                    settings.reader_rotation = rotation;
                                    if let Err(e) = settings.save(dir) {
                                        eprintln!("gideon: couldn't persist rotation: {e}");
                                    }
                                }
                                reader.show_banner(&format!("Rotation {rotation}° — locked"))?;
                            }
                            continue;
                        }
                        let Some(lights) = self.lights.as_mut() else {
                            continue;
                        };
                        let height = layout.height.max(1);
                        let delta = ((y0 as i64 - y1 as i64) * 100 / height as i64) as i32;
                        if delta == 0 {
                            continue;
                        }
                        let banner = if on_right {
                            let new = (lights.brightness() as i32 + delta).clamp(0, 100) as u8;
                            lights.set_brightness(new);
                            format!("Brightness {new}%")
                        } else {
                            let new = (lights.warmth() as i32 + delta).clamp(0, 100) as u8;
                            lights.set_warmth(new);
                            format!("Night light {new}%")
                        };
                        reader.show_banner(&banner)?;
                    }
                    // A slow tap is still a tap in the reader.
                    Ok(UiEvent::LongPress { x, y }) => {
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
                    Ok(UiEvent::Sleep) => {
                        // Field accesses only: `reader` is borrowing
                        // `self.display`, so no whole-`self` method calls.
                        let debounced =
                            matches!(self.last_wake, Some(t) if t.elapsed() < SLEEP_DEBOUNCE);
                        if self.sleeper.is_none() || debounced {
                            continue;
                        }
                        // Save the reading position before the power goes
                        // down — a dead battery must not lose it.
                        reader.save_progress(&mut store, key);
                        store.save(&progress_file)?;
                        let result = self.sleeper.as_mut().expect("checked above")();
                        self.last_wake = Some(std::time::Instant::now());
                        if let Err(e) = &result {
                            eprintln!("gideon: suspend failed: {e:#}");
                        }
                        if matches!(result, Ok(SleepResult::Skipped)) {
                            continue; // still awake, screen untouched
                        }
                        // Reopen possibly re-registered input nodes, drop
                        // the wake key press, relight, repaint in full.
                        self.input.refresh_devices();
                        self.input.discard_queued();
                        if let Some(lights) = self.lights.as_mut() {
                            lights.reapply();
                        }
                        reader.repaint_full()?;
                    }
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
        // Color shelf: when a visible Library card has real cover art,
        // compose in RGB so Kaleido panels show it in color. Always a full
        // refresh — the MTK driver's color waveform (GCC16) only runs on
        // FULL updates.
        if let Some(page) = self.compose_color_current()? {
            self.display.blit_rgb(&page, 0)?;
            self.display.flush(RefreshMode::Full)?;
            return Ok(());
        }
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

    /// The current screen as a color page, when it has one: the Library
    /// shelf with at least one visible downloaded cover (.cover.jpg).
    /// Everything else renders grayscale.
    fn compose_color_current(&self) -> Result<Option<RgbPage>> {
        let Some(Screen::Library { entries, page }) = self.stack.last() else {
            return Ok(None);
        };
        let l = &self.layout;
        let shelf = self.shelf_layout();
        let capacity = shelf.capacity().max(1);
        let visible = || entries.iter().skip(page * capacity).take(capacity);
        if !visible().any(|e| self.cover_path(e).exists()) {
            return Ok(None);
        }
        let page_count = entries.len().div_ceil(capacity).max(1);
        let chrome = compose_chrome(l, "Library", *page, page_count);
        let grid = compose_shelf_rgb(
            &self.shelf_entries_for_page(entries, *page, capacity),
            &shelf,
        );
        let mut canvas = RgbPage::from_gray(&chrome);
        copy_into_rgb(&mut canvas, &grid, 0, l.content_top());
        Ok(Some(canvas))
    }

    fn compose_current(&self) -> Result<GrayPage> {
        let l = &self.layout;
        let per_page = l.rows_per_page();
        let screen = self.stack.last().expect("stack never empty");
        Ok(match screen {
            Screen::Home => {
                let rows: Vec<(String, bool)> =
                    HOME_ROWS.iter().map(|r| (r.to_string(), true)).collect();
                // The version in the title answers "did the update take?"
                // at a glance; the profile name after it says whose library
                // this is (tapping the left half switches). No Back on Home
                // — the power symbol in the top-right corner opens the
                // restart/close menu instead.
                let title = format!(
                    "gideon v{} — {}",
                    env!("CARGO_PKG_VERSION"),
                    self.active_profile
                );
                let mut canvas = compose_list_opts(l, &title, &rows, 0, 1, false);
                draw_power_icon(&mut canvas, l);
                canvas
            }
            Screen::BookMenu { series_dir, .. } => {
                let rows = vec![
                    ("All chapters (from source)".to_string(), true),
                    ("Delete this chapter".to_string(), true),
                    ("Delete whole series".to_string(), true),
                ];
                compose_list(l, series_dir, &rows, 0, 1)
            }
            Screen::ProfileMenu { profiles } => {
                let mut rows: Vec<(String, bool)> = profiles
                    .iter()
                    .map(|p| {
                        let mark = if *p == self.active_profile {
                            "● "
                        } else {
                            ""
                        };
                        (format!("{mark}{p}"), true)
                    })
                    .collect();
                rows.push(("New profile…".to_string(), true));
                compose_list(l, "Profiles", &rows, 0, 1)
            }
            Screen::NewProfile { name } => compose_keyboard(l, "New profile", name, "Create"),
            Screen::Settings => {
                let rows = settings_rows(&self.load_settings());
                compose_list(l, "Settings", &rows, 0, 1)
            }
            Screen::PowerMenu => {
                let rows = vec![
                    ("Restart gideon".to_string(), true),
                    ("Close gideon".to_string(), true),
                ];
                compose_list(l, "Power", &rows, 0, 1)
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
                let rows = vec![
                    ("Popular".to_string(), true),
                    ("Latest".to_string(), true),
                    ("Search…".to_string(), true),
                ];
                compose_list(l, &source.name, &rows, 0, 1)
            }
            Screen::Search { source, query } => {
                let scope = source.as_ref().map_or("all sources", |s| s.name.as_str());
                compose_search(l, scope, query)
            }
            Screen::SearchResults {
                query,
                results,
                page,
            } => {
                let rows: Vec<(String, bool)> = paged(results, *page, per_page)
                    .iter()
                    .map(|(s, m)| (format!("{} — {}", m.title, s.name), true))
                    .collect();
                compose_list(
                    l,
                    &format!("\"{query}\""),
                    &rows,
                    *page,
                    l.page_count(results.len()),
                )
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
                source,
                manga,
                chapters,
                page,
            } => {
                // Mark chapters that are already on disk: they open
                // instantly, and a check tells the user what's stocked up.
                let index = gideon_core::SeriesIndex::load(&self.library_dir);
                let downloaded = index
                    .find_manga(&source.id, &manga.id)
                    .map(|(_, series)| series.downloaded.clone())
                    .unwrap_or_default();
                let rows: Vec<(String, bool)> = paged(chapters, *page, per_page)
                    .iter()
                    .map(|c| {
                        let mark = if downloaded.contains_key(&c.id) {
                            "✓ "
                        } else {
                            ""
                        };
                        (format!("{mark}{}", c.label()), true)
                    })
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

        let grid = compose_shelf(
            &self.shelf_entries_for_page(entries, page, capacity),
            &shelf,
        );
        copy_into(&mut canvas, &grid, 0, l.content_top());
        Ok(canvas)
    }

    /// The series cover art for a library entry (fetched at download time).
    fn cover_path(&self, entry: &LibraryEntry) -> PathBuf {
        let series_dir = entry
            .relative_path
            .split('/')
            .next()
            .unwrap_or(&entry.relative_path);
        self.library_dir.join(series_dir).join(".cover.jpg")
    }

    /// Build the shelf cards for one Library page, shared by the gray and
    /// RGB compositors.
    fn shelf_entries_for_page(
        &self,
        entries: &[LibraryEntry],
        page: usize,
        capacity: usize,
    ) -> Vec<ShelfEntry> {
        let store = ProgressStore::load(&progress_path(&self.library_dir)).unwrap_or_default();
        let mut shelf_entries = Vec::new();
        for entry in entries.iter().skip(page * capacity).take(capacity) {
            // Prefer the manga's cover art (fetched at download time);
            // fall back to the chapter's first page, then a placeholder.
            let cover = image::open(self.cover_path(entry))
                .ok()
                .or_else(|| {
                    CbzDocument::open(&entry.path)
                        .and_then(|mut doc| doc.decode_page(0))
                        .ok()
                })
                .unwrap_or_else(placeholder_cover);
            let progress = store.get(&entry.relative_path).map(|p| {
                if p.total_pages == 0 {
                    0.0
                } else {
                    (p.current_page + 1) as f32 / p.total_pages as f32
                }
            });
            shelf_entries.push(ShelfEntry {
                cover,
                title: entry_title(&entry.relative_path),
                progress,
            });
        }
        shelf_entries
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
    compose_chrome_opts(l, title, page, page_count, true)
}

/// Like [`compose_chrome`], but Home passes `show_back = false`: its
/// bottom-left corner has no Back (quitting goes through the power menu).
fn compose_chrome_opts(
    l: &UiLayout,
    title: &str,
    page: usize,
    page_count: usize,
    show_back: bool,
) -> GrayPage {
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
    if show_back {
        draw_text(
            &mut canvas,
            l.pad,
            nav_y,
            l.text_px,
            "< Back",
            third.saturating_sub(l.pad),
            false,
        );
    }
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
    compose_list_opts(l, title, rows, page, page_count, true)
}

fn compose_list_opts(
    l: &UiLayout,
    title: &str,
    rows: &[(String, bool)],
    page: usize,
    page_count: usize,
    show_back: bool,
) -> GrayPage {
    let mut canvas = compose_chrome_opts(l, title, page, page_count, show_back);
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

/// Apply an edit key to a keyboard buffer; `None` means no change (the
/// action key is handled by the caller). Shared by the search and
/// new-profile keyboards.
fn apply_key_edit(buffer: &str, key: Key) -> Option<String> {
    match key {
        Key::Char(c) => {
            let mut b = buffer.to_string();
            b.push(c);
            Some(b)
        }
        // No leading or doubled spaces — sources won't match them, and
        // directory names shouldn't carry them either.
        Key::Space => {
            if buffer.is_empty() || buffer.ends_with(' ') {
                None
            } else {
                Some(format!("{buffer} "))
            }
        }
        Key::Backspace => {
            let mut b = buffer.to_string();
            b.pop();
            Some(b)
        }
        Key::Search => None,
    }
}

/// The search screen: chrome + the query line + the on-screen keyboard.
fn compose_search(l: &UiLayout, source_name: &str, query: &str) -> GrayPage {
    compose_keyboard(l, &format!("Search {source_name}"), query, "Search")
}

/// A keyboard screen: chrome + the edited line + the on-screen keyboard,
/// with the action key labeled `action` ("Search", "Create"…).
fn compose_keyboard(l: &UiLayout, title: &str, buffer: &str, action: &str) -> GrayPage {
    let mut canvas = compose_chrome(l, title, 0, 1);

    // Edited line with a trailing caret, in the area above the keyboard.
    // When the text outgrows the line, show its tail — the user needs to
    // see what they are typing, not how the text started.
    let max_w = l.width.saturating_sub(2 * l.pad);
    let mut shown = format!("{buffer}_");
    while measure_text(l.text_px, &shown, true) > max_w && shown.chars().count() > 1 {
        shown.remove(0);
    }
    draw_text(
        &mut canvas,
        l.pad,
        l.row_top(0) + l.row_h.saturating_sub(l.text_px as u32 + 4) / 2,
        l.text_px,
        &shown,
        max_w,
        true,
    );
    hline(&mut canvas, l.keyboard_top().saturating_sub(1), 0x55);

    for (key, x, y, w, h) in l.keyboard_keys() {
        rect_outline(&mut canvas, x, y, w, h, 0xAA);
        let label = match key {
            Key::Char(c) => c.to_string(),
            Key::Backspace => "<del".to_string(),
            Key::Space => "space".to_string(),
            Key::Search => action.to_string(),
        };
        let bold = key == Key::Search;
        let tw = measure_text(l.text_px, &label, bold).min(w);
        draw_text(
            &mut canvas,
            x + (w.saturating_sub(tw)) / 2,
            y + h.saturating_sub(l.text_px as u32 + 4) / 2,
            l.text_px,
            &label,
            w,
            bold,
        );
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

/// The standard power symbol (an arc with a stem through its gap), drawn
/// in the top-right corner of the title bar. Tappable region: the right
/// `2 × title_h` of the title bar (see `handle_tap`).
fn draw_power_icon(canvas: &mut GrayPage, l: &UiLayout) {
    let r = (l.title_h as f32) / 3.2;
    let cx = l.width.saturating_sub(l.title_h / 2 + l.pad) as f32;
    let cy = (l.title_h as f32) * 0.55;

    let span = (r as u32) + 3;
    for dy in -(span as i32)..=(span as i32) {
        for dx in -(span as i32)..=(span as i32) {
            let (fx, fy) = (dx as f32, dy as f32);
            let dist = (fx * fx + fy * fy).sqrt();
            // The arc: a ring with a gap at the top for the stem.
            let on_ring = (dist - r).abs() <= 1.6;
            let in_gap = fy < 0.0 && fx.abs() < r * 0.45;
            // The stem: a vertical bar through the gap.
            let on_stem = fx.abs() <= 1.6 && (-r - 3.0..=-r * 0.15).contains(&fy);
            if (on_ring && !in_gap) || on_stem {
                let x = cx + fx;
                let y = cy + fy;
                if x >= 0.0 && y >= 0.0 && (x as u32) < canvas.width && (y as u32) < canvas.height {
                    canvas.pixels[(y as u32 * canvas.width + x as u32) as usize] = 0x00;
                }
            }
        }
    }
}

/// 1px rectangle outline, clipped to the canvas.
fn rect_outline(canvas: &mut GrayPage, x: u32, y: u32, w: u32, h: u32, value: u8) {
    if w == 0 || h == 0 {
        return;
    }
    for yy in [y, y + h - 1] {
        if yy >= canvas.height {
            continue;
        }
        let start = (yy * canvas.width + x.min(canvas.width)) as usize;
        let end = (yy * canvas.width + (x + w).min(canvas.width)) as usize;
        canvas.pixels[start..end].fill(value);
    }
    for yy in y..(y + h).min(canvas.height) {
        for xx in [x, x + w - 1] {
            if xx < canvas.width {
                canvas.pixels[(yy * canvas.width + xx) as usize] = value;
            }
        }
    }
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

/// [`copy_into`] for RGB pages (3 bytes per pixel).
fn copy_into_rgb(dst: &mut RgbPage, src: &RgbPage, off_x: u32, off_y: u32) {
    let copy_w = src.width.min(dst.width.saturating_sub(off_x));
    let copy_h = src.height.min(dst.height.saturating_sub(off_y));
    for y in 0..copy_h {
        let src_start = (y * src.width * 3) as usize;
        let dst_start = (((off_y + y) * dst.width + off_x) * 3) as usize;
        dst.pixels[dst_start..dst_start + copy_w as usize * 3]
            .copy_from_slice(&src.pixels[src_start..src_start + copy_w as usize * 3]);
    }
}

/// Card name for a library entry: "Series — Chapter" when it lives in a
/// series directory, just the file stem otherwise.
fn entry_title(relative_path: &str) -> String {
    let mut parts = relative_path.rsplitn(2, '/');
    let file = parts.next().unwrap_or(relative_path);
    let stem = file
        .strip_suffix(".cbz")
        .or_else(|| file.strip_suffix(".CBZ"))
        .unwrap_or(file);
    match parts.next() {
        Some(series) if !series.is_empty() => format!("{series} — {stem}"),
        _ => stem.to_string(),
    }
}

fn placeholder_cover() -> image::DynamicImage {
    image::DynamicImage::ImageLuma8(image::GrayImage::from_pixel(3, 4, image::Luma([0xCC])))
}

/// The Settings screen's rows, showing current values.
fn settings_rows(s: &gideon_core::Settings) -> Vec<(String, bool)> {
    let fit = match gideon_render::FitMode::from_setting(&s.reader_fit) {
        gideon_render::FitMode::FitWidth => "fit-width",
        _ => "contain",
    };
    let auto = if s.auto_check_updates { "on" } else { "off" };
    vec![
        (
            format!("Pre-download ahead: {}", s.predownload_unread_chapters),
            true,
        ),
        (format!("Storage limit: {}", s.storage_size_limit), true),
        (format!("Reader fit: {fit}"), true),
        (format!("Check updates automatically: {auto}"), true),
    ]
}

/// Next value in a cycle: the entry after `current`, wrapping around; the
/// first entry when `current` isn't in the list (hand-edited settings).
fn cycle<T: Copy + PartialEq>(steps: &[T], current: T) -> T {
    let position = steps.iter().position(|s| *s == current);
    steps[position.map_or(0, |i| (i + 1) % steps.len())]
}

/// The library directory of a profile: the root itself for "default",
/// `<root>/@<name>` otherwise. The @ prefix keeps profile dirs from
/// colliding with series dirs, and the root scan skips them.
fn profile_library_dir(base: &Path, profile: &str) -> PathBuf {
    if profile == "default" {
        base.to_path_buf()
    } else {
        base.join(format!("@{profile}"))
    }
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
