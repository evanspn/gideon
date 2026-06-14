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
pub use layout::{page_button_advances, Key, ReaderZone, TapTarget, UiLayout};

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use gideon_core::{CbzDocument, Library, LibraryEntry, ProgressStore};
use gideon_device::{Display, InputSource, LightControl, RefreshMode, UiEvent};
use gideon_render::shelf::{compose_shelf, compose_shelf_rgb, ShelfEntry, ShelfLayout};
use gideon_render::text::{draw_text, measure_text};
use gideon_render::{rotate_page, rotate_page_rgb, FitMode, GrayPage, RgbPage};

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
/// Full-refresh interval choices (page turns between flashes); higher is
/// smoother but ghosts more. Must stay within settings' 4–24 clamp.
const FULL_REFRESH_STEPS: [u32; 4] = [6, 8, 12, 16];
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

/// One library shelf card: a series directory grouping every downloaded
/// chapter inside it, or a single loose CBZ at the library root. Grouping
/// happens here in the UI layer — `Library::scan` still returns one entry
/// per file — so ten downloaded chapters of one series make ONE card.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SeriesCard {
    /// The top-level series directory, or `None` for a loose root CBZ.
    series: Option<String>,
    /// The chapters in this card, in natural (reading) order. Never empty:
    /// cards are only built by [`group_library`] from scanned files.
    chapters: Vec<LibraryEntry>,
}

impl SeriesCard {
    /// Card title: the series directory name, or the loose file's stem.
    fn title(&self) -> String {
        match &self.series {
            Some(dir) => dir.clone(),
            None => entry_title(&self.chapters[0].relative_path),
        }
    }

    /// The chapter a tap opens: the most recently read unfinished chapter
    /// (tapping the card resumes where the reader left off), else the
    /// first chapter in natural order.
    fn resume_chapter(&self, store: &ProgressStore) -> &LibraryEntry {
        self.chapters
            .iter()
            .filter_map(|c| {
                store
                    .get(&c.relative_path)
                    .filter(|p| !p.is_finished())
                    .map(|p| (p.last_read_at, c))
            })
            .max_by_key(|(at, _)| *at)
            .map(|(_, c)| c)
            .unwrap_or(&self.chapters[0])
    }

    /// The chapter after `current` within this card, for continuous
    /// reading (entries keep their natural scan order).
    fn next_after(&self, current: &LibraryEntry) -> Option<&LibraryEntry> {
        self.chapters
            .iter()
            .skip_while(|c| c.relative_path != current.relative_path)
            .nth(1)
    }

    /// Card progress: the most recently read chapter's progress (finished
    /// or not) — "where is this series at?" at a glance.
    fn progress(&self, store: &ProgressStore) -> Option<f32> {
        self.chapters
            .iter()
            .filter_map(|c| store.get(&c.relative_path))
            .max_by_key(|p| p.last_read_at)
            .map(|p| {
                if p.total_pages == 0 {
                    0.0
                } else {
                    (p.current_page + 1) as f32 / p.total_pages as f32
                }
            })
    }

    /// The entry whose file supplies the card's cover fallback (the first
    /// chapter's page 0); the series' `.cover.jpg` is preferred upstream.
    fn cover_entry(&self) -> &LibraryEntry {
        &self.chapters[0]
    }
}

/// Group scanned library entries into shelf cards: one per top-level
/// series directory and one per loose root CBZ. Cards keep the natural
/// order of their first chapter; chapters keep their natural scan order.
fn group_library(entries: Vec<LibraryEntry>) -> Vec<SeriesCard> {
    let mut cards: Vec<SeriesCard> = Vec::new();
    for entry in entries {
        let series = entry
            .relative_path
            .split_once('/')
            .map(|(dir, _)| dir.to_string());
        let existing = series
            .as_deref()
            .and_then(|s| cards.iter().position(|c| c.series.as_deref() == Some(s)));
        match existing {
            Some(i) => cards[i].chapters.push(entry),
            None => cards.push(SeriesCard {
                series,
                chapters: vec![entry],
            }),
        }
    }
    cards
}

#[derive(Debug, Clone)]
enum Screen {
    Home,
    Library {
        items: Vec<SeriesCard>,
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

/// How a reader session ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReaderOutcome {
    /// The user backed out to the screen beneath.
    Back,
    /// The input source closed: quit the app.
    Quit,
    /// The user turned past the last page and a next chapter exists.
    NextChapter,
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

/// A reader page turn slower than this counts as "the user couldn't see the
/// result yet". Presses that queued while such a turn rendered were made
/// blind (a big page decoding, or a full-flash refresh) — almost always a
/// frustrated multi-press — so they're dropped instead of cascading several
/// pages past where the reader wanted to be. Fast turns (the common partial
/// refresh, well under this) keep every press, so deliberate quick paging
/// still works.
const SLOW_TURN: std::time::Duration = std::time::Duration::from_millis(450);

/// How long `ensure_online` waits for Wi-Fi to associate + get an address
/// before giving up and letting the action surface the offline message.
const WIFI_CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

/// After a failed connect, skip the bring-up for this long so back-to-back
/// network taps don't each freeze for the full timeout (no saved network,
/// wrong password, captive portal).
const WIFI_FAIL_BACKOFF: std::time::Duration = std::time::Duration::from_secs(45);

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
    /// Page turns between full (flashing) refreshes (settings.json
    /// `reader_full_refresh_interval`); higher = fewer flashes = smoother.
    full_refresh_interval: u32,
    /// When the last Wi-Fi auto-connect attempt failed, to back off so every
    /// network tap doesn't re-pay the full connect timeout.
    last_wifi_fail: Option<std::time::Instant>,
    /// Reader rotation in degrees (from settings.json `reader_rotation`).
    reader_rotation: u32,
    /// Whether the reading orientation is locked. Locked: the accelerometer
    /// is ignored and manual rotations persist across sessions. Unlocked
    /// ("auto"): the gyro drives rotation app-wide and manual rotations stay
    /// session-only. Mirrors settings.json `reader_rotation_locked`; kept in
    /// sync when the reader's controls sheet toggles it.
    rotation_locked: bool,
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
    /// Battery charge probe (sysfs on hardware); `None` (tests, headless)
    /// hides the percentage from the Home title and the sleep notice.
    battery: Option<Box<dyn Fn() -> Option<u8>>>,
    /// Cell-sized cover thumbnails for the library shelf: Library repaints
    /// (page flips, returning from the reader) re-compose the shelf, and
    /// re-decoding every cover JPEG each time made repaints visibly slow.
    /// Keyed by (source path, file mtime, cell size); evicted least
    /// recently used — never wholesale, so flipping a shelf page back
    /// stays warm.
    cover_cache: std::cell::RefCell<CoverCache>,
    /// The shelf's ProgressStore, loaded once and reused across repaints
    /// (a disk read + JSON parse per shelf page flip was measurable).
    /// Invalidated whenever the UI writes progress or switches profile.
    progress_cache: std::cell::RefCell<Option<ProgressStore>>,
}

/// Cover-cache key: (source path, file mtime, target cell size).
type CoverKey = (PathBuf, std::time::SystemTime, (u32, u32));

/// LRU cache of cell-sized shelf thumbnails. `tick` is a logical clock:
/// every lookup stamps its entry, evictions remove the stalest stamp.
#[derive(Default)]
struct CoverCache {
    tick: u64,
    entries: std::collections::HashMap<CoverKey, (u64, image::DynamicImage)>,
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
            full_refresh_interval: 8,
            last_wifi_fail: None,
            reader_rotation: 0,
            rotation_locked: true,
            sleeper: None,
            last_wake: None,
            keyboard_paints: 0,
            lights: None,
            settings_dir: None,
            battery: None,
            cover_cache: std::cell::RefCell::new(CoverCache::default()),
            progress_cache: std::cell::RefCell::new(None),
        }
    }

    /// Start in this profile (resolved from settings.json at startup):
    /// the library directory becomes the profile's subdirectory.
    pub fn with_profile(mut self, name: &str) -> Self {
        self.active_profile = name.to_string();
        self.library_dir = profile_library_dir(&self.base_library, name);
        self
    }

    /// Apply the reader-related settings (fit mode and rotation). The
    /// rotation is app-wide: menus follow it too, so the layout is rebuilt
    /// against the rotated dimensions.
    pub fn with_reader_settings(mut self, fit: FitMode, rotation: u32) -> Self {
        self.reader_fit = fit;
        self.reader_rotation = rotation;
        self.rebuild_layout();
        self
    }

    /// (Re)build the menu layout against the current reading orientation:
    /// menus follow the reader rotation, so for 90/270 the layout uses the
    /// swapped (reading-frame) dimensions and [`Self::render_current`]
    /// rotates the composed page into the panel before blitting.
    fn rebuild_layout(&mut self) {
        let (w, h) = (self.display.width(), self.display.height());
        self.layout = if self.reader_rotation % 180 == 90 {
            UiLayout::new(h, w)
        } else {
            UiLayout::new(w, h)
        };
    }

    /// Map a panel tap into menu (reading-frame) coordinates: menus are
    /// composed against the rotated layout and rotated to the panel just
    /// before blitting, so input inverts that rotation HERE — the single
    /// chokepoint in [`Self::run`] that every screen inherits.
    fn map_menu_point(&self, x: u32, y: u32) -> (u32, u32) {
        layout::map_reader_tap(
            x,
            y,
            self.display.width(),
            self.display.height(),
            self.reader_rotation,
        )
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
    /// Also seeds the in-memory orientation-lock state, so the menus know
    /// up front whether the accelerometer should drive auto-rotation.
    pub fn with_settings_dir(mut self, dir: PathBuf) -> Self {
        if let Ok(settings) = gideon_core::Settings::load(&dir) {
            self.rotation_locked = settings.reader_rotation_locked;
            self.full_refresh_interval = settings.reader_full_refresh_interval;
        }
        self.settings_dir = Some(dir);
        self
    }

    /// Install the battery probe (sysfs capacity on hardware): the Home
    /// title and the sleep notice show the charge percentage.
    pub fn with_battery(mut self, battery: Box<dyn Fn() -> Option<u8>>) -> Self {
        self.battery = Some(battery);
        self
    }

    /// The current battery percentage, when a probe is installed and a
    /// battery reports one.
    fn battery_now(&self) -> Option<u8> {
        self.battery.as_ref().and_then(|probe| probe())
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
                // Every pointer event funnels through map_menu_point first
                // (the one chokepoint), so taps land where the rotated
                // menus drew their targets.
                Ok(UiEvent::Tap { x, y }) => {
                    let (x, y) = self.map_menu_point(x, y);
                    match self.handle_tap(x, y) {
                        Ok(Flow::Quit(exit)) => return Ok(exit),
                        Ok(Flow::Continue) => {}
                        // The UI must never die on an error: show it instead.
                        Err(e) => self.show_error(&e)?,
                    }
                }
                // Edge slides only matter in the reader; elsewhere a swipe
                // is just an overshot tap — ignore it.
                Ok(UiEvent::Swipe { .. }) => {}
                Ok(UiEvent::LongPress { x, y }) => {
                    let (x, y) = self.map_menu_point(x, y);
                    match self.handle_long_press(x, y) {
                        Ok(Flow::Quit(exit)) => return Ok(exit),
                        Ok(Flow::Continue) => {}
                        Err(e) => self.show_error(&e)?,
                    }
                }
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
                // The accelerometer reported a new orientation: in "auto"
                // mode the whole app follows it; locked ignores it.
                Ok(UiEvent::Rotate { rotation }) => {
                    if let Err(e) = self.auto_rotate_menus(rotation) {
                        self.show_error(&e)?;
                    }
                }
            }
        }
    }

    /// Apply a gyro-reported orientation to the menus (auto mode only):
    /// rebuild the layout against the new reading frame and repaint. A
    /// locked orientation, or no actual change, is a no-op.
    fn auto_rotate_menus(&mut self, rotation: u32) -> Result<()> {
        let rotation = rotation % 360;
        if self.rotation_locked || rotation == self.reader_rotation {
            return Ok(());
        }
        self.reader_rotation = rotation;
        self.rebuild_layout();
        self.render_current(RefreshMode::Full)
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
        // / button press registered. The battery line answers "should I
        // plug it in before the nap?" at a glance.
        let mut lines = vec!["Sleeping…".to_string()];
        if let Some(percent) = self.battery_now() {
            lines.push(format!("Battery {percent}%"));
        }
        lines.push("Press power or open the cover to wake.".to_string());
        let lines: Vec<&str> = lines.iter().map(String::as_str).collect();
        self.show_status_full(&lines)?;
        let result = self.sleeper.as_mut().expect("checked above")();
        self.last_wake = Some(std::time::Instant::now());
        if matches!(result, Ok(SleepResult::Skipped)) {
            // Pressing power while plugged in does nothing visible
            // otherwise — say why before restoring the screen.
            self.show_status_full(&["Plugged in — staying awake."])?;
            std::thread::sleep(SKIP_NOTICE_HOLD);
            self.render_current(RefreshMode::Full)?;
            return Ok(());
        }
        // Drop the key press that woke us, THEN reopen the (possibly
        // re-registered) input nodes — in that order. Reopening can take up
        // to ~3s on MTK while the nodes come back, and it hands us fresh,
        // empty fds; draining *after* it would throw away a press the user
        // made post-wake (e.g. the button that turns the last page into the
        // next chapter). Draining first flushes the wake press on the old
        // fds; input made after the reopen survives.
        self.input.discard_queued();
        self.input.refresh_devices();
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
            Screen::Library { items, page } => {
                let Some(card) = self.library_cell_at(&items, page, x, y) else {
                    return Ok(Flow::Continue);
                };
                // The menu targets the chapter a tap would open (the
                // card's resume point), so "Delete this chapter" removes
                // exactly what the user is looking at.
                let entry = self.with_progress(|_, store| card.resume_chapter(store).clone());
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
                        let cbz_path = self.download_to_library(&source, &manga, &chapter)?;
                        // No reader session here — fetch the cover now.
                        self.fetch_cover_if_missing(&manga, &cbz_path);
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
        let items = group_library(Library::new(&self.library_dir).scan()?);
        self.stack.pop(); // leave the book menu
        if let Some(screen @ Screen::Library { .. }) = self.stack.last_mut() {
            *screen = Screen::Library { items, page: 0 };
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
            Screen::Library { items, page } => (page, items.len().div_ceil(shelf_capacity)),
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
            Screen::Library { items, page } => self.tap_library_cell(&items, page, x, y),
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
                    return self.download_and_read(&source, &manga, &chapter, &chapters);
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
            Screen::UpdatePrompt { .. } => self.install_update(),
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
        let items = group_library(Library::new(&self.library_dir).scan()?);
        self.push(Screen::Library { items, page: 0 })
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
        self.ensure_online()?;
        // The available-sources fetch hits the network: without feedback
        // the tap looks dead for seconds on device WiFi.
        self.show_status(&["Loading sources…"])?;
        let rows = self.build_source_rows()?;
        self.push(Screen::Sources { rows, page: 0 })
    }

    fn install_and_refresh(&mut self, source: &SourceEntry) -> Result<()> {
        self.ensure_online()?;
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
        self.ensure_online()?;
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
            // TODO: this toggle only persists the preference today —
            // nothing reads auto_check_updates yet (see cmd_browse). Wire
            // it to an idle-time update check, not startup.
            3 => settings.auto_check_updates = !settings.auto_check_updates,
            4 => {
                // Cycle the Kaleido color boost: vivid → standard → off.
                // Dialing it down clears rainbow banding on color gradients.
                use gideon_device::ColorPostProcess as Cpp;
                let next = match Cpp::from_setting(&settings.color_post_process) {
                    Cpp::Vivid => Cpp::Standard,
                    Cpp::Standard => Cpp::Off,
                    Cpp::Off => Cpp::Vivid,
                };
                settings.color_post_process = next.as_setting().to_string();
                // Apply to the live panel so the next color refresh shows it.
                self.display.set_color_post_process(next);
            }
            5 => {
                // Cycle the full-refresh interval: fewer flashes = smoother,
                // more ghosting. Takes effect on the next opened book.
                settings.reader_full_refresh_interval =
                    cycle(&FULL_REFRESH_STEPS, settings.reader_full_refresh_interval);
                self.full_refresh_interval = settings.reader_full_refresh_interval;
            }
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
        // The progress cache belongs to the previous profile's library.
        self.invalidate_progress_cache();
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
        self.ensure_online()?;
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
        self.ensure_online()?;
        let sources = self.gateway.installed_sources()?;
        let mut results: Vec<(SourceEntry, MangaEntry)> = Vec::new();
        let mut failed: Vec<String> = Vec::new();
        for (i, source) in sources.iter().enumerate() {
            // One status screen for the whole search, partially updated
            // per source — N full flashes made an N-source search strobe.
            self.show_status(&[
                &format!("Searching for \"{query}\"…"),
                &format!("{}/{}: {}…", i + 1, sources.len(), source.name),
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
        self.ensure_online()?;
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
        self.ensure_online()?;
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

    /// Install the update; on success the app restarts itself in place so
    /// the new version is live immediately (no manual close-and-reopen).
    fn install_update(&mut self) -> Result<Flow> {
        self.show_status(&["Downloading update…"])?;
        let body = self
            .gateway
            .install_update()
            .context("update install failed")?;
        if body.starts_with("Updated to") {
            self.show_status(&["Update installed — restarting…"])?;
            return Ok(Flow::Quit(Exit::Restart));
        }
        self.pop()?; // leave the prompt
        self.push(Screen::Message {
            title: "Updates".to_string(),
            body,
        })?;
        Ok(Flow::Continue)
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
        self.ensure_online()?;
        self.show_status(&[&format!("Downloading {label}…")])?;

        let layout = self.layout;
        let rotation = self.reader_rotation;
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
                let page = rotate_for_panel(page, rotation);
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
        }
        Ok(cbz_path)
    }

    /// Fetch the manga cover once per series (library cards show the real
    /// cover art instead of a chapter's first page). Best-effort metadata,
    /// deliberately kept OFF the chapter-open critical path: callers run it
    /// after the reader session (or after a download-only long press),
    /// never between the tap and the first page.
    fn fetch_cover_if_missing(&mut self, manga: &MangaEntry, cbz_path: &Path) {
        let Some(dir) = cbz_path.parent().and_then(|p| p.file_name()) else {
            return;
        };
        let cover_path = self.library_dir.join(dir).join(".cover.jpg");
        if cover_path.exists() {
            return;
        }
        if let Some(url) = manga.cover_url.as_deref() {
            if let Err(e) = self.gateway.download_cover(url, &cover_path) {
                eprintln!("gideon: couldn't fetch the cover: {e:#}");
            }
        }
    }

    fn download_and_read(
        &mut self,
        source: &SourceEntry,
        manga: &MangaEntry,
        chapter: &ChapterEntry,
        chapters: &[ChapterEntry],
    ) -> Result<Flow> {
        let mut chapter = chapter.clone();
        loop {
            // Already on disk? Straight into the reader — no network.
            let cbz_path = match self.downloaded_chapter_path(source, manga, &chapter.id) {
                Some(path) => path,
                None => self.download_to_library(source, manga, &chapter)?,
            };

            // Taps queued while the download ran were aimed at the (now
            // gone) chapter list — drop them so they don't flip pages in
            // the reader. A sleep cover closed during the download
            // survives the drain: the device must still suspend instead
            // of sitting awake in a bag.
            self.input.discard_taps();

            let next = next_chapter(chapters, &chapter.id);
            let key = progress_key(&self.library_dir, &cbz_path);
            let outcome = self.run_reader(&cbz_path, &key, next.is_some())?;
            // The cover fetch (a network round-trip) runs after the
            // session, never between the tap and the first page.
            if outcome != ReaderOutcome::Quit {
                self.fetch_cover_if_missing(manga, &cbz_path);
            }
            match outcome {
                ReaderOutcome::Quit => return Ok(Flow::Quit(Exit::Close)),
                ReaderOutcome::Back => return Ok(Flow::Continue),
                // Turning past the last page flows into the next chapter.
                ReaderOutcome::NextChapter => {
                    chapter = next.expect("NextChapter only with a next");
                }
            }
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

    /// The series card whose shelf cell contains the tap, if any.
    fn library_cell_at(
        &self,
        items: &[SeriesCard],
        page: usize,
        x: u32,
        y: u32,
    ) -> Option<SeriesCard> {
        let shelf = self.shelf_layout();
        let capacity = shelf.capacity().max(1);
        let local_y = y.saturating_sub(self.layout.content_top());
        let visible = items.len().saturating_sub(page * capacity).min(capacity);
        for cell in 0..visible {
            let (cx, cy) = shelf.cell_origin(cell);
            if x >= cx
                && x < cx + shelf.cell_width()
                && local_y >= cy
                && local_y < cy + shelf.cell_height()
            {
                return Some(items[page * capacity + cell].clone());
            }
        }
        None
    }

    fn tap_library_cell(
        &mut self,
        items: &[SeriesCard],
        page: usize,
        x: u32,
        y: u32,
    ) -> Result<Flow> {
        let Some(card) = self.library_cell_at(items, page, x, y) else {
            return Ok(Flow::Continue);
        };
        // Resume the series where it was left: the most recently read
        // unfinished chapter, else its first chapter.
        let mut entry = self.with_progress(|_, store| card.resume_chapter(store).clone());
        loop {
            // Continuation within the series: the card's next chapter
            // (chapters keep the scan's natural order).
            let next = card.next_after(&entry).cloned();
            match self.run_reader(&entry.path, &entry.relative_path, next.is_some())? {
                ReaderOutcome::Quit => return Ok(Flow::Quit(Exit::Close)),
                ReaderOutcome::Back => return Ok(Flow::Continue),
                ReaderOutcome::NextChapter => {
                    entry = next.expect("NextChapter only with a next");
                }
            }
        }
    }

    // --- reader session ---

    /// Open a CBZ in the reader and loop until the user taps the center
    /// zone (back), turns past the last page (with `next_available`), or
    /// the input source ends.
    fn run_reader(
        &mut self,
        path: &Path,
        key: &str,
        next_available: bool,
    ) -> Result<ReaderOutcome> {
        let doc =
            CbzDocument::open(path).with_context(|| format!("couldn't open {}", path.display()))?;
        let progress_file = progress_path(&self.library_dir);
        let mut store = ProgressStore::load(&progress_file).unwrap_or_default();

        // The reader works in PANEL coordinates (self.layout may be the
        // rotated menu layout): build its gesture geometry from the
        // display itself, leaving the reader pipeline untouched.
        let panel = UiLayout::new(self.display.width(), self.display.height());
        let mut rotation = self.reader_rotation;
        // Orientation lock (kept in sync with the app-wide field): locked
        // persists rotation across sessions and ignores the gyro; "auto"
        // keeps manual rotation session-only AND lets the accelerometer
        // drive it. Toggled from the controls sheet.
        let mut rotation_locked = self.rotation_locked;
        // The reader-controls sheet (Rotate 90° / Orientation / Close),
        // opened by an up-swipe that starts in the bottom eighth of the
        // reading frame.
        let mut sheet_open = false;
        let mut outcome = ReaderOutcome::Back;
        {
            let mut reader = Reader::new(doc, &mut self.display, self.reader_fit, rotation);
            reader.set_full_refresh_interval(self.full_refresh_interval);
            reader.resume_from(&store, key);
            // Warm the render-ahead at the resume page before the first
            // paint: the decode + scale + dither run on the prefetch
            // thread, and the first paint just takes the finished render.
            reader.warm();
            reader.show_current_page()?;
            loop {
                let event = self.input.next_event();
                // While the controls sheet is up, taps go to its rows; any
                // other event closes it (Sleep still suspends below).
                if sheet_open {
                    match &event {
                        Err(_) => {}
                        Ok(UiEvent::Tap { x, y }) | Ok(UiEvent::LongPress { x, y }) => {
                            let (_, my) =
                                layout::map_reader_tap(*x, *y, panel.width, panel.height, rotation);
                            let reading_h = if rotation % 180 == 90 {
                                panel.width
                            } else {
                                panel.height
                            };
                            match controls_sheet_row(reading_h, panel.row_h, my) {
                                Some(SHEET_ROW_ROTATE) => {
                                    sheet_open = false;
                                    rotate_reader_90(
                                        &mut reader,
                                        &mut rotation,
                                        self.settings_dir.as_deref(),
                                        rotation_locked,
                                    )?;
                                    self.reader_rotation = rotation;
                                }
                                Some(SHEET_ROW_ORIENTATION) => {
                                    rotation_locked = !rotation_locked;
                                    // Keep the app-wide field in sync so the
                                    // menus know whether the gyro is live.
                                    self.rotation_locked = rotation_locked;
                                    let locked = rotation_locked;
                                    persist_settings(self.settings_dir.as_deref(), |s| {
                                        s.reader_rotation_locked = locked;
                                        if locked {
                                            // Locking captures the current
                                            // orientation for next time.
                                            s.reader_rotation = rotation;
                                        }
                                    });
                                    // Switching to auto snaps to how the device
                                    // is held right now (no need to physically
                                    // move it first).
                                    let snapped = if locked {
                                        None
                                    } else {
                                        self.input.resync_orientation()
                                    };
                                    if let Some(UiEvent::Rotate { rotation: target }) = snapped {
                                        let target = target % 360;
                                        if target != rotation {
                                            sheet_open = false;
                                            reader.set_rotation(target);
                                            rotation = target;
                                            self.reader_rotation = target;
                                            reader.show_current_page()?;
                                            continue;
                                        }
                                    }
                                    // Redraw with the flipped label.
                                    show_controls_sheet(
                                        &mut reader,
                                        &panel,
                                        rotation,
                                        rotation_locked,
                                    )?;
                                }
                                _ => {
                                    // Close, or a tap above the sheet.
                                    sheet_open = false;
                                    reader.show_current_page()?;
                                }
                            }
                            continue;
                        }
                        Ok(UiEvent::Sleep) => {
                            // Fall through: the suspend handling below
                            // repaints in full, wiping the sheet away.
                            sheet_open = false;
                        }
                        Ok(UiEvent::Rotate { rotation: target }) => {
                            // A gyro report with the sheet up: apply it (auto
                            // mode) and repaint, which also wipes the sheet.
                            sheet_open = false;
                            let target = *target % 360;
                            if !rotation_locked && target != rotation {
                                reader.set_rotation(target);
                                rotation = target;
                                self.reader_rotation = target;
                            }
                            reader.show_current_page()?;
                            continue;
                        }
                        Ok(_) => {
                            sheet_open = false;
                            reader.show_current_page()?;
                            continue;
                        }
                    }
                }
                match event {
                    Err(_) => {
                        outcome = ReaderOutcome::Quit;
                        break;
                    }
                    // Tap zones follow the reading orientation, not the panel.
                    Ok(UiEvent::Tap { x, y }) => match panel.reader_zone_rotated(x, y, rotation) {
                        ReaderZone::NextPage => {
                            // Turning past the last page continues into
                            // the next chapter (when one exists).
                            if !turn_reader_page(&mut reader, &mut self.input, true)?
                                && next_available
                            {
                                outcome = ReaderOutcome::NextChapter;
                                break;
                            }
                        }
                        ReaderZone::PrevPage => {
                            turn_reader_page(&mut reader, &mut self.input, false)?;
                        }
                        ReaderZone::Back => break,
                    },
                    // Physical page-turn buttons follow the reading
                    // orientation: held upside down (180°) the two keys have
                    // physically swapped places, so the forward button goes
                    // back and vice versa (upright and landscape keep
                    // forward = next).
                    Ok(ev @ (UiEvent::PageForward | UiEvent::PageBack)) => {
                        let forward = matches!(ev, UiEvent::PageForward);
                        if page_button_advances(forward, rotation) {
                            if !turn_reader_page(&mut reader, &mut self.input, true)?
                                && next_available
                            {
                                outcome = ReaderOutcome::NextChapter;
                                break;
                            }
                        } else {
                            turn_reader_page(&mut reader, &mut self.input, false)?;
                        }
                    }
                    // The accelerometer reported a new orientation: in "auto"
                    // mode rotate the reader to it; locked ignores it.
                    Ok(UiEvent::Rotate { rotation: target }) => {
                        let target = target % 360;
                        if !rotation_locked && target != rotation {
                            reader.set_rotation(target);
                            rotation = target;
                            self.reader_rotation = target;
                            reader.show_current_page()?;
                        }
                    }
                    // Edge slides (panel coordinates — the physical bezel
                    // edge, regardless of reading rotation): right edge is
                    // brightness, left edge is night-light warmth. Sliding
                    // up increases; the full screen height is the full
                    // 0–100 range.
                    Ok(UiEvent::Swipe { x0, y0, x1, y1 }) => {
                        let edge = (panel.width / 8).max(1);
                        let on_right = x0 >= panel.width - edge && x1 >= panel.width - edge;
                        let on_left = x0 < edge && x1 < edge;
                        if !on_right && !on_left {
                            // Mid-screen gestures follow the READING
                            // orientation (taps already do): swipe down to
                            // leave the manga, swipe up to rotate 90°
                            // clockwise — for reading on your side in bed.
                            // Both demand deliberate travel (a quarter of
                            // the reading height): a sloppy page-turn tap
                            // drifting past the 30px slop must never exit,
                            // and certainly never rotate the whole reader.
                            let (mx0, my0) =
                                layout::map_reader_tap(x0, y0, panel.width, panel.height, rotation);
                            let (mx1, my1) =
                                layout::map_reader_tap(x1, y1, panel.width, panel.height, rotation);
                            let reading_h = if rotation % 180 == 90 {
                                panel.width
                            } else {
                                panel.height
                            };
                            let min_travel = (reading_h / 4).max(1);
                            let vertical = my0.abs_diff(my1) > mx0.abs_diff(mx1);
                            // An up-swipe STARTING in the bottom eighth of
                            // the reading frame opens the controls sheet —
                            // distinct from the mid-screen rotate gesture
                            // below, which starts higher up. An eighth of
                            // travel is enough: it's a flick off the bezel.
                            let sheet_band = reading_h.saturating_sub((reading_h / 8).max(1));
                            if my0 > my1
                                && vertical
                                && my0 > sheet_band
                                && my0 - my1 >= (reading_h / 8).max(1)
                            {
                                sheet_open = true;
                                show_controls_sheet(
                                    &mut reader,
                                    &panel,
                                    rotation,
                                    rotation_locked,
                                )?;
                                continue;
                            }
                            if my1 > my0 && vertical && my1 - my0 >= min_travel {
                                break;
                            }
                            if my0 > my1 && vertical && my0 - my1 >= min_travel {
                                rotate_reader_90(
                                    &mut reader,
                                    &mut rotation,
                                    self.settings_dir.as_deref(),
                                    rotation_locked,
                                )?;
                                self.reader_rotation = rotation;
                            }
                            continue;
                        }
                        let Some(lights) = self.lights.as_mut() else {
                            continue;
                        };
                        let height = panel.height.max(1);
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
                        match panel.reader_zone_rotated(x, y, rotation) {
                            ReaderZone::NextPage => {
                                if !turn_reader_page(&mut reader, &mut self.input, true)?
                                    && next_available
                                {
                                    outcome = ReaderOutcome::NextChapter;
                                    break;
                                }
                            }
                            ReaderZone::PrevPage => {
                                turn_reader_page(&mut reader, &mut self.input, false)?;
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
                        // Drop the wake key press FIRST, then reopen the
                        // possibly re-registered input nodes — reopening hands
                        // us fresh fds and can take ~3s, so draining after it
                        // would eat a press the user makes post-wake (e.g. the
                        // button that advances the last page into the next
                        // chapter, which "sometimes" failed after sleep).
                        self.input.discard_queued();
                        self.input.refresh_devices();
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
        // The shelf's cached store is stale now — the session moved pages.
        self.invalidate_progress_cache();
        // The session may have rotated the reading orientation: the menus
        // follow it, so rebuild the layout before repainting them.
        self.rebuild_layout();

        if outcome == ReaderOutcome::Back {
            // Repaint the screen the reader covered. (NextChapter goes
            // straight into the next reader session — no repaint between.)
            self.render_current(RefreshMode::Full)?;
        }
        Ok(outcome)
    }

    // --- chapter continuation helpers ---

    // --- rendering ---

    /// Show a transient status ("Loading…", "Searching…") with a PARTIAL
    /// refresh: a full e-ink flash per status doubled the perceived
    /// latency of every network action. NOTE: partials can ghost — the
    /// destination screens that replace a status deliberately stay Full
    /// (`push`/`render_current`), flashing any ghosting away. Statuses
    /// that *stay* on the panel (the sleep notice) use
    /// [`Self::show_status_full`] instead.
    fn show_status(&mut self, lines: &[&str]) -> Result<()> {
        self.show_status_mode(lines, RefreshMode::Partial)
    }

    /// Bring Wi-Fi up if we're offline, before a network action. A user who
    /// launched gideon with Wi-Fi off in Nickel (or whose lease dropped) is
    /// recovered automatically — "it just fixes itself" — instead of only
    /// seeing "no network". Best-effort and additive: when already connected
    /// it returns instantly and changes nothing; when offline it paints a
    /// "Connecting to Wi-Fi…" status, brings the radio up and waits for an
    /// address. If it still can't connect, the action proceeds and surfaces
    /// the clear offline message itself.
    fn ensure_online(&mut self) -> Result<()> {
        if gideon_device::network::is_online() {
            return Ok(());
        }
        // Don't make every tap pay a long connect when we just failed: a
        // missing/wrong saved network or captive portal would otherwise
        // freeze for the full timeout on every action. Within the backoff
        // window, proceed straight to the action (which surfaces the clear
        // offline message) instead of bringing the radio up again.
        if self
            .last_wifi_fail
            .is_some_and(|t| t.elapsed() < WIFI_FAIL_BACKOFF)
        {
            return Ok(());
        }
        gideon_device::network::bring_up_wifi();
        // Poll with a visible per-second heartbeat so the e-ink panel doesn't
        // look frozen (a motionless "Connecting…" reads as a crash and invites
        // a force power-off). Repaint the elapsed-seconds counter each tick.
        let start = std::time::Instant::now();
        let mut online = gideon_device::network::is_online();
        while !online && start.elapsed() < WIFI_CONNECT_TIMEOUT {
            self.show_status(&[
                "Connecting to Wi-Fi…",
                &format!("({}s)", start.elapsed().as_secs()),
            ])?;
            std::thread::sleep(std::time::Duration::from_secs(1));
            online = gideon_device::network::is_online();
        }
        self.last_wifi_fail = (!online).then(std::time::Instant::now);
        Ok(())
    }

    /// A status that stays on the panel (suspend notices): full refresh,
    /// so the held image is flashed clean.
    fn show_status_full(&mut self, lines: &[&str]) -> Result<()> {
        self.show_status_mode(lines, RefreshMode::Full)
    }

    fn show_status_mode(&mut self, lines: &[&str], mode: RefreshMode) -> Result<()> {
        let page = compose_status(&self.layout, lines);
        let page = rotate_for_panel(page, self.reader_rotation);
        self.display.blit(&page, 0)?;
        self.display.flush(mode)?;
        Ok(())
    }

    fn render_current(&mut self, mode: RefreshMode) -> Result<()> {
        // Menus are composed in reading orientation (the layout was built
        // on the rotated dims) and rotated into the panel just before the
        // blit, mirroring the reader's own pipeline.
        let rotation = self.reader_rotation;
        // Color shelf: when a visible Library card has real cover art,
        // compose in RGB so Kaleido panels show it in color. The caller's
        // refresh mode passes through: the MTK driver has a non-flashing
        // color waveform (GLRC16) for partials, so shelf page flips don't
        // have to flash.
        if let Some(page) = self.compose_color_current()? {
            let page = if rotation == 0 {
                page
            } else {
                rotate_page_rgb(&page, rotation)
            };
            self.display.blit_rgb(&page, 0)?;
            self.display.flush(mode)?;
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
        let page = rotate_for_panel(page, rotation);
        self.display.blit(&page, 0)?;
        self.display.flush(mode)?;
        Ok(())
    }

    /// The current screen as a color page, when it has one: the Library
    /// shelf with at least one visible downloaded cover (.cover.jpg).
    /// Everything else renders grayscale.
    fn compose_color_current(&self) -> Result<Option<RgbPage>> {
        let Some(Screen::Library { items, page }) = self.stack.last() else {
            return Ok(None);
        };
        let l = &self.layout;
        let shelf = self.shelf_layout();
        let capacity = shelf.capacity().max(1);
        let visible = || items.iter().skip(page * capacity).take(capacity);
        if !visible().any(|c| self.cover_path(c.cover_entry()).exists()) {
            return Ok(None);
        }
        let page_count = items.len().div_ceil(capacity).max(1);
        let chrome = compose_chrome(l, "Library", *page, page_count);
        let grid = compose_shelf_rgb(&self.shelf_entries_for_page(items, *page, &shelf), &shelf);
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
                // this is (tapping the left half switches); the battery
                // percent closes the line (the panel has no status bar
                // otherwise). No Back on Home — the power symbol in the
                // top-right corner opens the restart/close menu instead.
                let title = home_title(
                    env!("CARGO_PKG_VERSION"),
                    &self.active_profile,
                    self.battery_now(),
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
            Screen::Library { items, page } => self.compose_library(items, *page)?,
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

    fn compose_library(&self, items: &[SeriesCard], page: usize) -> Result<GrayPage> {
        let l = &self.layout;
        let shelf = self.shelf_layout();
        let capacity = shelf.capacity().max(1);
        let page_count = items.len().div_ceil(capacity).max(1);

        let mut canvas = compose_chrome(l, "Library", page, page_count);
        if items.is_empty() {
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

        let grid = compose_shelf(&self.shelf_entries_for_page(items, page, &shelf), &shelf);
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
    /// RGB compositors: one card per series, titled by the series, with
    /// the most-recently-read chapter's progress.
    fn shelf_entries_for_page(
        &self,
        items: &[SeriesCard],
        page: usize,
        shelf: &ShelfLayout,
    ) -> Vec<ShelfEntry> {
        let capacity = shelf.capacity().max(1);
        // The shelf only ever shows covers at cell size: decode (and
        // cache) thumbnails at exactly that size.
        let cell = (
            shelf.cell_width(),
            shelf
                .cell_height()
                .saturating_sub(shelf.title_height + shelf.progress_bar_height),
        );
        self.with_progress(|app, store| {
            items
                .iter()
                .skip(page * capacity)
                .take(capacity)
                .map(|card| ShelfEntry {
                    cover: app.shelf_cover(card.cover_entry(), cell, capacity),
                    title: card.title(),
                    progress: card.progress(store),
                })
                .collect()
        })
    }

    /// Run `f` with the (cached) ProgressStore: the disk read + JSON parse
    /// happen at most once between [`Self::invalidate_progress_cache`]
    /// calls, not once per repaint.
    fn with_progress<R>(&self, f: impl FnOnce(&Self, &ProgressStore) -> R) -> R {
        let store = self.progress_cache.borrow_mut().take().unwrap_or_else(|| {
            ProgressStore::load(&progress_path(&self.library_dir)).unwrap_or_default()
        });
        let result = f(self, &store);
        *self.progress_cache.borrow_mut() = Some(store);
        result
    }

    /// Drop the cached ProgressStore — progress was just written, or the
    /// library root changed (profile switch).
    fn invalidate_progress_cache(&self) {
        self.progress_cache.borrow_mut().take();
    }

    /// The decoded, cell-sized cover thumbnail for a library entry,
    /// through the LRU cover cache. Prefers the manga's cover art
    /// (fetched at download time), falling back to the chapter's first
    /// page, then a placeholder. Thumbnails are keyed by (path, mtime,
    /// cell size) and evicted least recently used past two shelf pages
    /// (`capacity`) of entries — never cleared wholesale: flipping a
    /// shelf page back must stay a cache hit.
    fn shelf_cover(
        &self,
        entry: &LibraryEntry,
        cell: (u32, u32),
        capacity: usize,
    ) -> image::DynamicImage {
        // Which file would supply the cover? Its mtime invalidates stale
        // cache entries (e.g. a re-fetched .cover.jpg).
        let cover_path = self.cover_path(entry);
        let path = if cover_path.exists() {
            cover_path
        } else {
            entry.path.clone()
        };
        let mtime = std::fs::metadata(&path)
            .and_then(|m| m.modified())
            .unwrap_or(std::time::UNIX_EPOCH);

        let mut cache = self.cover_cache.borrow_mut();
        cache.tick += 1;
        let tick = cache.tick;
        let key = (path, mtime, cell);
        if let Some((stamp, image)) = cache.entries.get_mut(&key) {
            *stamp = tick;
            return image.clone();
        }
        let decoded = if key.0.extension().is_some_and(|e| e == "jpg") {
            image::open(&key.0).ok()
        } else {
            CbzDocument::open(&key.0)
                .and_then(|mut doc| doc.decode_page(0))
                .ok()
        };
        match decoded {
            Some(image) => {
                // Cache the cell-sized thumbnail, not the full decode: a
                // page is megapixels, a shelf cell a few hundred KB. The
                // resize stays a DynamicImage (RGB preserved), so Kaleido
                // color covers are unregressed.
                let thumb = image.resize(cell.0, cell.1, image::imageops::FilterType::Triangle);
                while cache.entries.len() >= 2 * capacity.max(1) {
                    let Some(oldest) = cache
                        .entries
                        .iter()
                        .min_by_key(|(_, (stamp, _))| *stamp)
                        .map(|(key, _)| key.clone())
                    else {
                        break;
                    };
                    cache.entries.remove(&oldest);
                }
                cache.entries.insert(key, (tick, thumb.clone()));
                thumb
            }
            // Failures aren't cached: they're cheap to re-hit, and the
            // file may become readable later (e.g. a finished copy).
            None => placeholder_cover(),
        }
    }
}

// --- reader controls sheet ---

/// Rows of the reader-controls sheet, top to bottom.
const SHEET_ROW_ROTATE: usize = 0;
const SHEET_ROW_ORIENTATION: usize = 1;
const SHEET_ROW_CLOSE: usize = 2;
const SHEET_ROW_COUNT: u32 = 3;

fn controls_sheet_labels(locked: bool) -> [String; 3] {
    [
        "Rotate 90°".to_string(),
        format!("Orientation: {}", if locked { "locked" } else { "auto" }),
        "Close".to_string(),
    ]
}

/// The controls sheet as a reading-frame strip (the caller rotates it
/// into the panel): three full-width rows with a dark top border.
fn compose_controls_sheet(
    reading_w: u32,
    row_h: u32,
    text_px: f32,
    pad: u32,
    locked: bool,
) -> GrayPage {
    let mut sheet = GrayPage::new_white(reading_w, SHEET_ROW_COUNT * row_h.max(1));
    hline(&mut sheet, 0, 0x00);
    for (i, label) in controls_sheet_labels(locked).iter().enumerate() {
        let top = i as u32 * row_h;
        draw_text(
            &mut sheet,
            pad,
            top + row_h.saturating_sub(text_px as u32 + 4) / 2,
            text_px,
            label,
            reading_w.saturating_sub(2 * pad),
            i == SHEET_ROW_ROTATE,
        );
        let sep_y = top + row_h - 1;
        if sep_y + 1 < sheet.height {
            hline(&mut sheet, sep_y, 0xAA);
        }
    }
    sheet
}

/// The sheet row under a reading-frame tap at height `my`; `None` when the
/// tap landed above the sheet (which closes it). The sheet hugs the bottom
/// of the reading frame.
fn controls_sheet_row(reading_h: u32, row_h: u32, my: u32) -> Option<usize> {
    let row_h = row_h.max(1);
    let top = reading_h.saturating_sub(SHEET_ROW_COUNT * row_h);
    (my >= top).then(|| (((my - top) / row_h) as usize).min(SHEET_ROW_CLOSE))
}

/// Panel-frame origin of the (already rotated) controls sheet: the strip
/// hugs the bottom edge of the READING frame, which lands on a different
/// panel edge per rotation (left for 90, top for 180, right for 270).
fn controls_sheet_origin(panel_w: u32, panel_h: u32, sheet_h: u32, rotation: u32) -> (u32, u32) {
    match rotation % 360 {
        90 | 180 => (0, 0),
        270 => (panel_w.saturating_sub(sheet_h), 0),
        _ => (0, panel_h.saturating_sub(sheet_h)),
    }
}

/// Draw the controls sheet over the current page: composed in reading
/// orientation, rotated into the panel and stamped via the reader's
/// chrome overlay (a partial flush; the next page repaint wipes it).
fn show_controls_sheet<D: Display>(
    reader: &mut Reader<D>,
    panel: &UiLayout,
    rotation: u32,
    locked: bool,
) -> Result<()> {
    let reading_w = if rotation % 180 == 90 {
        panel.height
    } else {
        panel.width
    };
    let sheet = compose_controls_sheet(reading_w, panel.row_h, panel.text_px, panel.pad, locked);
    let sheet_h = sheet.height;
    let rotated = rotate_for_panel(sheet, rotation);
    let (x, y) = controls_sheet_origin(panel.width, panel.height, sheet_h, rotation);
    reader.overlay_chrome(&rotated, x, y)
}

/// Rotate the reading orientation 90° clockwise: the single code path
/// behind the mid-screen up-swipe AND the controls sheet's "Rotate 90°"
/// row. The new rotation persists only while the orientation is locked.
fn rotate_reader_90<D: Display>(
    reader: &mut Reader<D>,
    rotation: &mut u32,
    settings_dir: Option<&Path>,
    locked: bool,
) -> Result<()> {
    *rotation = (*rotation + 90) % 360;
    reader.set_rotation(*rotation);
    if locked {
        let degrees = *rotation;
        persist_settings(settings_dir, |s| s.reader_rotation = degrees);
    }
    reader.show_banner(&rotation_banner(*rotation, locked))
}

fn rotation_banner(rotation: u32, locked: bool) -> String {
    if locked {
        format!("Rotation {rotation}° — locked")
    } else {
        format!("Rotation {rotation}°")
    }
}

/// Turn the reader one page (`forward` = next, else previous). If the render
/// was slow *because it had to decode* — `>= SLOW_TURN` on a partial-refresh
/// turn — drop any taps / button presses that queued *while it ran*: those
/// were a frustrated multi-press during the lag and must not cascade several
/// pages past the target. The expected periodic full-flash refresh (slow by
/// design, ~0.5s) is explicitly NOT treated as frustration, so a deliberate
/// tap landing during that flash still registers. A free function because the
/// reader session holds a partial borrow of the app (`self.display`), so it
/// takes `input` by reference rather than calling an `&mut self` method.
/// Returns whether a page turned (`false` at the end of the document, for the
/// next-chapter handoff).
fn turn_reader_page<D: Display, I: InputSource>(
    reader: &mut Reader<D>,
    input: &mut I,
    forward: bool,
) -> Result<bool> {
    let start = std::time::Instant::now();
    let advanced = if forward {
        reader.next_page()?
    } else {
        reader.prev_page()?
    };
    // Skip the debounce on a full-refresh turn: its ~0.5s flash always
    // exceeds SLOW_TURN, but it's expected slowness, not a lagging decode —
    // flushing there would eat a real tap roughly every Nth turn.
    if start.elapsed() >= SLOW_TURN && !reader.last_refresh_was_full() {
        // Non-blocking: drains only what already queued during the render
        // (sleep requests survive), so a fast turn with an empty queue is a
        // no-op and never costs a deliberate press.
        input.discard_taps();
    }
    Ok(advanced)
}

/// Persist a settings mutation (no-op without a settings dir); a failed
/// save is logged, never fatal. A free function because reader sessions
/// hold a partial borrow of the app and can't call `&self` methods.
fn persist_settings(settings_dir: Option<&Path>, mutate: impl FnOnce(&mut gideon_core::Settings)) {
    let Some(dir) = settings_dir else { return };
    let mut settings = gideon_core::Settings::load(dir).unwrap_or_default();
    mutate(&mut settings);
    if let Err(e) = settings.save(dir) {
        eprintln!("gideon: couldn't save settings: {e}");
    }
}

/// Rotate a composed menu page into the panel orientation (identity at 0,
/// where the menu path stays copy-free).
fn rotate_for_panel(page: GrayPage, rotation: u32) -> GrayPage {
    if rotation == 0 {
        page
    } else {
        rotate_page(&page, rotation)
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

/// Home's title line: `gideon vX — profile — 47%`, with the battery part
/// omitted when no battery reports a charge (tests, dev machines).
fn home_title(version: &str, profile: &str, battery: Option<u8>) -> String {
    let mut title = format!("gideon v{version} — {profile}");
    if let Some(percent) = battery {
        title.push_str(&format!(" — {percent}%"));
    }
    title
}

/// The Settings screen's rows, showing current values.
fn settings_rows(s: &gideon_core::Settings) -> Vec<(String, bool)> {
    let fit = match gideon_render::FitMode::from_setting(&s.reader_fit) {
        gideon_render::FitMode::FitWidth => "fit-width",
        _ => "contain",
    };
    let auto = if s.auto_check_updates { "on" } else { "off" };
    let color = gideon_device::ColorPostProcess::from_setting(&s.color_post_process).as_setting();
    vec![
        (
            format!("Pre-download ahead: {}", s.predownload_unread_chapters),
            true,
        ),
        (format!("Storage limit: {}", s.storage_size_limit), true),
        (format!("Reader fit: {fit}"), true),
        (format!("Check updates automatically: {auto}"), true),
        (format!("Color boost: {color}"), true),
        (
            format!(
                "Full refresh: every {} pages",
                s.reader_full_refresh_interval
            ),
            true,
        ),
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

/// The chapter that follows `current_id` in reading order. Chapter lists
/// from sources are usually newest-first, so order by chapter number when
/// numbers exist: the next chapter is the one with the smallest number
/// greater than the current. Without numbers, assume newest-first and
/// step toward the front of the list.
fn next_chapter(chapters: &[ChapterEntry], current_id: &str) -> Option<ChapterEntry> {
    let index = chapters.iter().position(|c| c.id == current_id)?;
    if let Some(current_num) = chapters[index].num {
        return chapters
            .iter()
            .filter(|c| c.num.is_some_and(|n| n > current_num))
            .min_by(|a, b| {
                a.num
                    .partial_cmp(&b.num)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .cloned();
    }
    index.checked_sub(1).map(|i| chapters[i].clone())
}

/// Progress key for a document: its path relative to the library root.
fn progress_key(library_dir: &Path, path: &Path) -> String {
    path.strip_prefix(library_dir)
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .unwrap_or_else(|_| path.display().to_string())
}
