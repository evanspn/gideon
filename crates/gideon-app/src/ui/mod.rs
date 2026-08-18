//! On-device browse UI: a tap-driven menu system rendered straight to the
//! framebuffer, so the device is usable without SSH.
//!
//! [`UiApp`] is generic over [`Display`], [`InputSource`] and
//! [`SourceGateway`], so the whole state machine is unit-testable with
//! `MemoryDisplay` + `FakeInput` + a fake gateway (no network, no WASM).
//!
//! Screens: Home → Library (cover shelf → Reader) and Home → Sources →
//! Listings → MangaList → ChapterList → download → Reader. Navigation is a
//! stack; the bottom bar is [Back] [First] [Prev] [Next] [Last]. Screen changes use a full
//! e-ink refresh, in-screen updates (pagination, status) partial ones.
//! Errors never panic the UI: they land on a message screen with Back.

mod gateway;
mod layout;
#[cfg(test)]
mod tests;

pub use gateway::{AidokuGateway, ChapterEntry, MangaEntry, SourceEntry, SourceGateway};
pub use layout::{page_button_advances, Key, ReaderZone, TapTarget, UiLayout};

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};

use gideon_core::{CbzDocument, Library, LibraryEntry, ProgressStore};
use gideon_device::{Display, InputSource, LightControl, RefreshMode, UiEvent};
use gideon_render::shelf::{compose_shelf, compose_shelf_rgb, ShelfEntry, ShelfLayout};
use gideon_render::text::{draw_text, measure_text};
use gideon_render::{heatmap, rotate_page, rotate_page_rgb, widgets, FitMode, GrayPage, RgbPage};

use crate::reader::Reader;

const HOME_ROWS: [&str; 7] = [
    "Library",
    "Search all sources",
    "Browse sources",
    "Settings",
    "Check for updates",
    // Appended (not inserted) so the existing Home rows keep their indices.
    "Popular manga",
    "Reading stats",
];
/// A tappable top row shown on Home ONLY when offline (device only): a manual
/// "scan + reconnect" for the roam-while-idle case, without a battery-draining
/// background connectivity poll.
/// Week-columns on the stats heatmap. Matches the web dashboard's 18 so the
/// two surfaces show the same window of history.
const STATS_HEATMAP_WEEKS: u32 = 18;

const HOME_RECONNECT_ROW: &str = "No Wi-Fi - tap to reconnect";
/// Trailing row on the global-search results screen: widen the search to
/// sources that aren't installed yet (keeping any that match).
const SEARCH_MORE_ROW: &str = "+ Search more sources";
/// How many recent global searches the search UI keeps for instant reopen.
const RECENT_SEARCHES: usize = 3;
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
/// Index of the trailing "Storage" row on the Settings screen — appended after
/// the ten cycling rows ([`settings_rows`]), it opens the storage detail
/// screen instead of cycling a value.
const SETTINGS_STORAGE_ROW: usize = 12;
/// Index of the trailing "Account" row on the Settings screen — appended after
/// the storage row, it opens the sync account menu (sign in / sync / sign out).
const SETTINGS_ACCOUNT_ROW: usize = 13;
/// Row of the "Free up space now" action on the Storage screen (after three
/// read-only info rows). Shared by the renderer and the tap handler.
const STORAGE_FREE_ROW: usize = 3;

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
    /// Card title: the series directory name, or the loose file's stem —
    /// tidied for display (the dir name is the FAT32-sanitized form, where
    /// characters like ':' and '?' became underscores).
    fn title(&self) -> String {
        match &self.series {
            Some(dir) => tidy_title(dir),
            None => entry_title(&self.chapters[0].relative_path),
        }
    }

    /// The chapter a tap opens — the chapter the user **last opened** in this
    /// series, resumed at its saved page. Read from the explicitly stored
    /// `last_opened` record (written the instant the reader opens a chapter), so
    /// it's exact and clock-independent — never a guess.
    ///
    /// Only when there's no record yet (a library from before the record
    /// existed) does it fall back — to the **furthest** chapter with any
    /// progress (the last one in reading order you've touched), NOT the newest
    /// timestamp. "Furthest" is what "where am I in this series" means: if you've
    /// read up to ch209 that's ch209, even if you dipped back into an earlier
    /// chapter more recently (which is what made it jump to ch139). Then the
    /// first chapter, if nothing's been read.
    fn resume_chapter(&self, store: &ProgressStore) -> &LibraryEntry {
        let series_key = series_key_of(&self.chapters[0].relative_path);
        if let Some(key) = store.last_opened(series_key) {
            if let Some(entry) = self.chapters.iter().find(|c| c.relative_path == key) {
                return entry;
            }
        }
        self.furthest_read(store).unwrap_or(&self.chapters[0])
    }

    /// The furthest chapter (last in reading order) that has any progress —
    /// "how far into the series am I". Order-based, so it ignores when each was
    /// last touched.
    fn furthest_read(&self, store: &ProgressStore) -> Option<&LibraryEntry> {
        self.chapters
            .iter()
            .rev()
            .find(|c| store.get(&c.relative_path).is_some())
    }

    /// The chapter the user most recently read in this card (finished or not),
    /// i.e. the one a "mark as unread" should clear. `None` when nothing's read.
    fn latest_read(&self, store: &ProgressStore) -> Option<&LibraryEntry> {
        self.chapters
            .iter()
            .filter_map(|c| store.get(&c.relative_path).map(|p| (p.last_read_at, c)))
            .max_by_key(|(at, _)| *at)
            .map(|(_, c)| c)
    }

    /// The most recent `last_read_at` across this card's chapters, for
    /// shelf ordering. `0` (sorts last) when nothing in it has been read.
    fn latest_read_at(&self, store: &ProgressStore) -> u64 {
        self.chapters
            .iter()
            .filter_map(|c| store.get(&c.relative_path))
            .map(|p| p.last_read_at)
            .max()
            .unwrap_or(0)
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

/// Order shelf cards most-recently-read first, so the series you're in the
/// middle of is always the top-left tap target. A stable sort keeps
/// never-read series (all tied at `0`) in their natural (alphabetical) order,
/// trailing behind everything that's been read.
fn sort_library_by_recency(items: &mut [SeriesCard], store: &ProgressStore) {
    items.sort_by_key(|card| std::cmp::Reverse(card.latest_read_at(store)));
}

/// Display order for a chapter list. The backing `Vec` always stays in the
/// source's (or disk scan's) natural order — reading-continuity logic like
/// [`next_chapter`] and the downloaded reading chain depend on it — so this
/// only permutes which rows the user sees and taps.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum ChapterSort {
    /// As fetched from the source / scanned from disk (sources are usually
    /// newest-first). The default, so opening a manga looks unchanged.
    #[default]
    Source,
    /// Chapter number ascending — chapter 1 at the top.
    Ascending,
    /// Chapter number descending — the newest chapter at the top.
    Descending,
}

impl ChapterSort {
    /// The next order in the tap-to-cycle ring: Source → Asc → Desc → Source.
    fn next(self) -> Self {
        match self {
            ChapterSort::Source => ChapterSort::Ascending,
            ChapterSort::Ascending => ChapterSort::Descending,
            ChapterSort::Descending => ChapterSort::Source,
        }
    }

    /// Short label for the title-bar sort button (ASCII so it renders in the
    /// bundled font).
    fn label(self) -> &'static str {
        match self {
            ChapterSort::Source => "Sort: src",
            ChapterSort::Ascending => "Sort: 1-9",
            ChapterSort::Descending => "Sort: 9-1",
        }
    }
}

/// Indices into a chapter list in display order. `nums[i]` is chapter `i`'s
/// number (`None` when unknown). Ascending sorts numbered chapters low→high,
/// leaving any unnumbered ones in their original order at the end; Descending
/// is the exact reverse of that, so it still flips the list even when no
/// chapter carries a parseable number.
fn chapter_display_order(nums: &[Option<f32>], sort: ChapterSort) -> Vec<usize> {
    let mut order: Vec<usize> = (0..nums.len()).collect();
    if matches!(sort, ChapterSort::Source) {
        return order;
    }
    order.sort_by(|&a, &b| match (nums[a], nums[b]) {
        (Some(x), Some(y)) => x.partial_cmp(&y).unwrap_or(std::cmp::Ordering::Equal),
        // Numbered chapters sort ahead of unnumbered ones; unnumbered keep
        // their original relative order (the sort is stable).
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => std::cmp::Ordering::Equal,
    });
    if matches!(sort, ChapterSort::Descending) {
        order.reverse();
    }
    order
}

/// A page-navigation request within a paginated list (see [`App::move_page`]).
#[derive(Debug, Clone, Copy)]
enum PageMove {
    /// Step relative to the current page (clamped to the valid range).
    Delta(i64),
    /// Jump to the first page.
    First,
    /// Jump to the last page.
    Last,
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
    /// "Search all sources" landing screen shown when there's history: a "New
    /// search" row plus the recent searches, each tappable to reopen its
    /// cached results instantly. `(query, result count)` per recent.
    RecentSearches {
        recents: Vec<(String, usize)>,
    },
    /// Global search results: each row knows which source it came from. A
    /// trailing "Search more sources" row widens the search to not-yet-
    /// installed sources. `tried` is every source id already searched for
    /// this query (installed ones, plus any pulled in by a widen) so a
    /// repeated widen continues past them.
    SearchResults {
        query: String,
        results: Vec<(SourceEntry, MangaEntry)>,
        tried: Vec<String>,
        page: usize,
    },
    /// MyAnimeList "Popular manga" tab (Home entry). Catalogue titles, not
    /// tied to any source; tapping one runs a global search for its title so
    /// it can be found and downloaded from the installed sources.
    Popular {
        mangas: Vec<MangaEntry>,
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
        sort: ChapterSort,
    },
    /// Offline list of a series' **downloaded** chapters — built from local
    /// files, never the source. Tapping a row opens it in the reader.
    DownloadedChapters {
        title: String,
        entries: Vec<LibraryEntry>,
        page: usize,
        sort: ChapterSort,
    },
    /// Per-chapter action menu, opened from the ⋮ button on a chapter row.
    /// `key` is the chapter's progress key (its on-disk path) when downloaded —
    /// `None` means it isn't on disk yet (nothing to mark/delete). `download`
    /// carries the source context for the download actions; `None` when the
    /// menu was opened from the offline downloaded-chapters list (no source).
    ChapterMenu {
        title: String,
        key: Option<String>,
        finished: bool,
        download: Option<DownloadContext>,
    },
    /// "Download from here…" count picker, opened from a chapter's ⋮ menu. Each
    /// row queues that many chapters — from `index` forward — onto the
    /// background downloader so they're ready offline.
    DownloadAheadMenu {
        source: SourceEntry,
        manga: MangaEntry,
        chapters: Vec<ChapterEntry>,
        index: usize,
    },
    /// Context menu for a library book (long press on its card).
    BookMenu {
        entry: LibraryEntry,
        series_dir: String,
        /// The chapter to "mark as unread": the most recently read one in this
        /// card (which may differ from `entry`, the resume target — if you
        /// finished a chapter, the card resumes at the *next* one but unread
        /// should clear the chapter you actually read). `None` when nothing in
        /// the card has been read yet.
        read_key: Option<String>,
    },
    /// Confirmation before an irreversible delete from the book menu. Carries
    /// everything the delete needs, so nothing is removed from disk until the
    /// user taps the confirm row — Back cancels harmlessly.
    ConfirmDelete {
        entry: LibraryEntry,
        series_dir: String,
        scope: DeleteScope,
    },
    /// Confirmation before removing an installed source (long press on its
    /// row in Sources). Only the source package is removed — downloaded
    /// chapters and reading progress stay in the library.
    ConfirmRemoveSource {
        source: SourceEntry,
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
    /// On-screen keyboard for naming the *default* profile, converting it into
    /// an ordinary one: the action key moves the library root's contents into
    /// `@<name>` and switches to it.
    ConvertDefault {
        name: String,
    },
    /// Reading stats for the active profile: the totals and the activity
    /// heatmap, derived from this profile's own progress store.
    Stats,
    /// Device-global settings (NOT per profile): each tap cycles a value
    /// and saves settings.json immediately.
    Settings,
    /// Storage usage detail, opened from Settings: how much downloaded content
    /// is on disk against the limit, plus a manual "free up space now" action.
    Storage,
    /// Sync account menu, opened from Settings. Signed out it offers sign-in;
    /// signed in it shows the email with "Sync now" and "Sign out".
    AccountMenu,
    /// On-screen keyboard for the account email; the action key advances to
    /// [`Screen::AccountPassword`].
    AccountEmail {
        email: String,
    },
    /// On-screen keyboard for the account password; the action key signs in
    /// (email + password), stores the session, and triggers a first sync.
    AccountPassword {
        email: String,
        password: String,
    },
    /// Restart/close menu, opened from the power symbol on Home.
    PowerMenu,
    /// Manga the web queued to "send to Kobo": tap one to search for it and add
    /// it to the library. Opened from the Home notification bell.
    SentList {
        items: Vec<gideon_sync::supabase::SendItem>,
    },
    /// Wi-Fi networks from a scan: tap one to connect (or enter a password).
    WifiList {
        networks: Vec<gideon_device::network::WifiNetwork>,
    },
    /// On-screen keyboard for a secured network's password; the action key
    /// connects.
    WifiPassword {
        ssid: String,
        password: String,
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

/// What a confirmed delete removes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DeleteScope {
    /// Just the resume chapter's file (and the series dir if it empties out).
    Chapter,
    /// The whole series directory and its download history.
    Series,
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

/// How often the wait-for-unplug loop re-probes the charger after a suspend
/// was refused while plugged in. Charger power covers the polling.
const UNPLUG_POLL: std::time::Duration = std::time::Duration::from_secs(5);

/// Idle auto-suspend: with no input for [`IDLE_SUSPEND`] (15 minutes),
/// suspend as if the sleep cover closed — the same timeout Nickel and
/// KOReader default to. A static e-ink page costs nothing, but the CPU
/// stays scheduled and Wi-Fi stays fully up for the whole idle stretch; a
/// user who walks away without closing the cover otherwise drains the
/// battery for hours.
///
/// Idle is measured in WALL-CLOCK time since the last delivered event, not
/// in poll timeouts: on hardware `poll_event` can return "no event" long
/// before its timeout (mid-gesture touch traffic, gyro chatter, inotify),
/// so counting returns would accrue "idle" while a finger is on the glass.
const IDLE_SUSPEND: std::time::Duration = std::time::Duration::from_secs(15 * 60);

/// How long each idle-detection poll waits between wall-clock checks.
const IDLE_SUSPEND_TICK: std::time::Duration = std::time::Duration::from_secs(60);

/// Idle-suspend choices the Settings row cycles through, in minutes — the
/// same increments Nickel and KOReader offer. 0 means "never".
const IDLE_SUSPEND_STEPS: [u32; 6] = [5, 10, 15, 30, 60, 0];

/// The idle-suspend duration for a minutes setting; 0 ("never") maps to a
/// threshold no uptime can reach.
fn idle_suspend_duration(minutes: u32) -> std::time::Duration {
    if minutes == 0 {
        std::time::Duration::MAX
    } else {
        std::time::Duration::from_secs(u64::from(minutes) * 60)
    }
}

/// The Settings-row label for an idle-suspend minutes value.
fn idle_suspend_label(minutes: u32) -> String {
    if minutes == 0 {
        "never".to_string()
    } else {
        format!("{minutes} min")
    }
}

/// What the wait-for-unplug loop ended with.
enum UnplugWait {
    /// The charger was pulled and the suspend hook ran; carry its result.
    Slept(Result<SleepResult>),
    /// The user pressed/tapped something — they're using the device, so the
    /// pending sleep is dropped.
    Aborted,
}

/// A suspend was refused because the charger is plugged in (an MTK suspend
/// with the charger in hangs the kernel). Instead of giving up — which left
/// a device closed in its cover awake FOREVER once unplugged — wait for the
/// unplug and finish the nap the user asked for. Any input except another
/// sleep request aborts the wait (the user is clearly using the device); a
/// repeated cover-close/power-press just keeps waiting.
///
/// A free function taking the fields it needs, because the reader session
/// holds a partial borrow of the app and can't call `&mut self` methods.
fn sleep_once_unplugged<I: InputSource>(
    input: &mut I,
    charger: &dyn Fn() -> bool,
    sleeper: &mut SleepFn,
) -> Result<UnplugWait> {
    loop {
        // Probe first, poll second: the charger may already be out by the
        // time we get here (or in tests, immediately).
        if !charger() {
            return Ok(UnplugWait::Slept(sleeper()));
        }
        match input.poll_event(UNPLUG_POLL)? {
            Some(UiEvent::Sleep) => {} // already trying to sleep; keep waiting
            Some(_) => return Ok(UnplugWait::Aborted),
            None => {}
        }
    }
}

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

/// While waiting to connect, re-fire the bring-up/reassociate this often rather
/// than waiting passively — KOReader-style persistence: the chip can miss the
/// first association after waking, so keep nudging it until it sticks.
const WIFI_REKICK_INTERVAL: std::time::Duration = std::time::Duration::from_secs(6);

/// A cached global search, kept so the most recent few can be reopened
/// instantly (no network) from the "Search all sources" screen.
#[derive(Clone)]
struct RecentSearch {
    query: String,
    results: Vec<(SourceEntry, MangaEntry)>,
    /// Source ids already searched for this query, so a reopened search can
    /// keep widening past them.
    tried: Vec<String>,
}

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
    /// Auto-rotate wide double-page spreads 270° (settings.json
    /// `auto_rotate_spreads`), applied to a reader session at open.
    auto_rotate_spreads: bool,
    /// When the last Wi-Fi auto-connect attempt failed, to back off so every
    /// network tap doesn't re-pay the full connect timeout.
    last_wifi_fail: Option<std::time::Instant>,
    /// Whether gideon may bring Wi-Fi up automatically (before a network
    /// action and on wake). From settings.json `wifi_auto_connect`; the
    /// Settings "Auto-connect Wi-Fi" toggle flips it. Manual reconnect ignores
    /// it.
    wifi_auto_connect: bool,
    /// Whether the last Home paint showed the offline "reconnect" row. Cached
    /// at render time (one `is_online` probe per Home paint) so tap dispatch
    /// uses the same offset that was drawn, even if connectivity flips between
    /// the paint and the tap.
    home_offline: bool,
    /// Connectivity probe. `None` uses the real
    /// [`gideon_device::network::is_online`]; tests inject a closure so the
    /// whole UI can be driven through its offline state deterministically.
    online_probe: Option<Box<dyn Fn() -> bool>>,
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
    /// Whether the on-screen keyboard is in upper-case mode (Shift). Sticky
    /// (caps-lock style); reset each time a keyboard screen opens.
    keyboard_shift: bool,
    /// Frontlight hook for the reader's edge slides; `None` (tests,
    /// headless) means swipes are ignored.
    lights: Option<Box<dyn LightControl>>,
    /// Where settings.json lives, for persisting in-reader changes
    /// (rotation lock). `None` skips persistence.
    settings_dir: Option<PathBuf>,
    /// Battery charge probe (sysfs on hardware); `None` (tests, headless)
    /// hides the percentage from the Home title and the sleep notice.
    battery: Option<Box<dyn Fn() -> Option<u8>>>,
    /// Wall-clock inactivity before an automatic suspend (default
    /// [`IDLE_SUSPEND`]); only enforced when a sleeper is installed. Tests
    /// shrink it to zero to exercise the path.
    idle_suspend: std::time::Duration,
    /// Charger-plugged probe (sysfs on hardware). With one installed, a
    /// suspend refused while charging waits for the unplug and then sleeps
    /// (see [`sleep_once_unplugged`]); `None` (tests, headless) keeps the
    /// old behavior — stay awake.
    charger: Option<Box<dyn Fn() -> bool>>,
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
    /// Serializes whole-file `SeriesIndex` rewrites so the background
    /// pre-download thread and the foreground download path never clobber
    /// each other's entries.
    index_guard: Arc<Mutex<()>>,
    /// Background chapter pre-downloader, built lazily on first use from
    /// [`SourceGateway::background_clone`]. `None` until then — and stays
    /// `None` for gateways without background support (tests), which fall
    /// back to a foreground pre-download.
    predownloader: Option<Predownloader>,
    /// What the look-ahead was last asked to stock up, so it can be re-fired
    /// after waking from sleep. A suspend usually takes the network with it, and
    /// look-ahead jobs that ran into the dead radio quietly failed; without a
    /// re-kick the next chapter is still missing when the user turns past the
    /// last page. Cleared when the user leaves the manga.
    lookahead: Option<LookaheadPlan>,
    /// The most recent global searches (newest first, capped at
    /// [`RECENT_SEARCHES`]) for instant reopen from the search screen.
    recent_searches: Vec<RecentSearch>,
}

/// The last look-ahead request: which chapters of which manga to keep stocked
/// ahead of the one being read. Re-fired on wake (see [`UiApp::lookahead`]).
#[derive(Clone)]
struct LookaheadPlan {
    source: SourceEntry,
    manga: MangaEntry,
    chapters: Vec<ChapterEntry>,
    after_id: String,
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
            auto_rotate_spreads: false,
            last_wifi_fail: None,
            wifi_auto_connect: true,
            home_offline: false,
            online_probe: None,
            reader_rotation: 0,
            rotation_locked: true,
            sleeper: None,
            last_wake: None,
            keyboard_paints: 0,
            keyboard_shift: false,
            lights: None,
            settings_dir: None,
            battery: None,
            idle_suspend: IDLE_SUSPEND,
            charger: None,
            cover_cache: std::cell::RefCell::new(CoverCache::default()),
            progress_cache: std::cell::RefCell::new(None),
            index_guard: Arc::new(Mutex::new(())),
            predownloader: None,
            lookahead: None,
            recent_searches: Vec::new(),
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
            self.auto_rotate_spreads = settings.auto_rotate_spreads;
            self.wifi_auto_connect = settings.wifi_auto_connect;
        }
        self.settings_dir = Some(dir);
        self
    }

    /// Override the connectivity probe (tests drive the offline UI through
    /// this; production leaves it `None` and uses the real Wi-Fi check).
    #[cfg(test)]
    pub(crate) fn with_online_probe(mut self, probe: Box<dyn Fn() -> bool>) -> Self {
        self.online_probe = Some(probe);
        self
    }

    /// Whether the device has a usable connection — the single point the UI
    /// consults to decide online vs. offline behavior, so it swaps states
    /// consistently. Defers to the injected probe when present.
    fn is_online(&self) -> bool {
        match &self.online_probe {
            Some(probe) => probe(),
            None => gideon_device::network::is_online(),
        }
    }

    /// Install the battery probe (sysfs capacity on hardware): the Home
    /// title and the sleep notice show the charge percentage.
    pub fn with_battery(mut self, battery: Box<dyn Fn() -> Option<u8>>) -> Self {
        self.battery = Some(battery);
        self
    }

    /// Apply the saved idle-suspend timeout (minutes; 0 = never). Enforced
    /// only when a suspend hook is installed.
    pub fn with_idle_suspend_minutes(mut self, minutes: u32) -> Self {
        self.idle_suspend = idle_suspend_duration(minutes);
        self
    }

    /// Install the charger probe (sysfs status on hardware): a suspend
    /// refused while plugged in then waits out the charger and finishes the
    /// nap once the cable is pulled, instead of staying awake forever.
    pub fn with_charger(mut self, charger: Box<dyn Fn() -> bool>) -> Self {
        self.charger = Some(charger);
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

    #[cfg(test)]
    pub(crate) fn input_mut(&mut self) -> &mut I {
        &mut self.input
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
            // With a suspend hook installed, wait in ticks instead of
            // blocking forever, and auto-suspend after 15 idle minutes —
            // a user who walks away without closing the cover otherwise
            // leaves the CPU scheduled and Wi-Fi up for hours (Nickel and
            // KOReader both do this). Two subtleties in what "idle" means:
            // it's wall-clock time, not a count of empty polls (on hardware
            // a poll can return "no event" long before its timeout —
            // mid-gesture touch traffic, gyro chatter); and the clock
            // starts when we begin WAITING, not when the last event was
            // delivered — handling an event can nest an entire reading
            // session (the reader runs inside the tap handler), and a
            // stale clock suspended the device seconds after the user
            // stopped flipping pages.
            let event = if self.sleeper.is_some() {
                let mut idle_since = std::time::Instant::now();
                loop {
                    match self.input.poll_event(IDLE_SUSPEND_TICK) {
                        Ok(Some(event)) => break Ok(event),
                        Ok(None) => {
                            if idle_since.elapsed() >= self.idle_suspend {
                                idle_since = std::time::Instant::now();
                                if let Err(e) = self.sleep_now() {
                                    self.show_error(&e)?;
                                }
                            }
                        }
                        Err(e) => break Err(e),
                    }
                }
            } else {
                self.input.next_event()
            };
            match event {
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
                // Physical page-turn buttons and the Bluetooth remote both page
                // through whatever list is on screen (library shelf, sources,
                // results…). Paging a list is orientation-independent, so the
                // two are equivalent here.
                Ok(UiEvent::PageForward | UiEvent::RemoteNext) => {
                    if let Err(e) = self.flip_page(1) {
                        self.show_error(&e)?;
                    }
                }
                Ok(UiEvent::PageBack | UiEvent::RemotePrev) => {
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
        // Last chance to get your place off the device: the nap can last all
        // night, and nothing else would sync until the library is opened again.
        // Bounded and skipped entirely when offline (see `sync_before_sleep`).
        crate::sync::sync_before_sleep(&self.library_dir, self.gateway.background_clone());
        let mut result = self.sleeper.as_mut().expect("checked above")();
        self.last_wake = Some(std::time::Instant::now());
        if matches!(result, Ok(SleepResult::Skipped)) {
            if self.charger.is_none() {
                // No charger probe (tests, headless): pressing power while
                // plugged in does nothing visible otherwise — say why
                // before restoring the screen.
                self.show_status_full(&["Plugged in — staying awake."])?;
                std::thread::sleep(SKIP_NOTICE_HOLD);
                self.render_current(RefreshMode::Full)?;
                return Ok(());
            }
            // The user asked for sleep; the charger refused it. Wait out
            // the charger (any other input aborts) and finish the nap once
            // the cable is pulled — otherwise a device closed in its cover
            // and unplugged later stays awake until the battery is dead.
            self.show_status_full(&[
                "Plugged in — will sleep once unplugged.",
                "Tap anywhere to stay awake.",
            ])?;
            match sleep_once_unplugged(
                &mut self.input,
                self.charger.as_ref().expect("checked above"),
                self.sleeper.as_mut().expect("checked above"),
            )? {
                UnplugWait::Aborted => {
                    self.render_current(RefreshMode::Full)?;
                    return Ok(());
                }
                UnplugWait::Slept(slept) => {
                    self.last_wake = Some(std::time::Instant::now());
                    result = slept;
                }
            }
            // Re-plugged in the instant between our probe and the hook's
            // own check: give up rather than loop.
            if matches!(result, Ok(SleepResult::Skipped)) {
                self.render_current(RefreshMode::Full)?;
                return Ok(());
            }
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
        // Snap to how the device is held now (auto mode only): the gsensor
        // reports only on *change*, so without an explicit resync the menus
        // would stay at the pre-sleep orientation until the device was
        // physically moved — the "screen won't rotate after sleep" bug.
        if !self.rotation_locked {
            if let Some(UiEvent::Rotate { rotation }) = self.input.resync_orientation() {
                let rotation = rotation % 360;
                if rotation != self.reader_rotation {
                    self.reader_rotation = rotation;
                    self.rebuild_layout();
                }
            }
        }
        // Proactively rejoin Wi-Fi (unless auto-connect is off): a suspend
        // usually leaves the radio un-associated / lease-less, so kick a
        // (detached, non-blocking) scan + re-associate now rather than waiting
        // for the next network action. A FAILED suspend also took the radio
        // down before dying, so restore it even with auto-connect off — the
        // user turned off auto-connect, not the radio.
        // Bluetooth (if it was on) also needs Wi-Fi: the MTK BT stack rides
        // the shared radio, so its restore below depends on this bring-up.
        if self.wifi_auto_connect || result.is_err() || gideon_device::bluetooth::resume_pending() {
            gideon_device::network::reconnect_after_wake();
        }
        // Restore Bluetooth and re-connect the page-turn remote (detached,
        // no-op unless the suspend powered it down).
        gideon_device::bluetooth::reconnect_after_wake();
        // Re-stock the chapters ahead: anything the look-ahead missed while the
        // radio was down gets another go, so the next chapter is on disk before
        // the user reaches it. Queue-only — nothing blocks here.
        self.rekick_lookahead();
        // Push whatever the pre-sleep flush couldn't — but not now, when the
        // radio is still coming back: this waits for the network to actually be
        // up, then syncs.
        crate::sync::spawn_sync_when_online(&self.library_dir, self.gateway.background_clone());
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
        // Leaving a manga's chapter list: stop pre-downloading its chapters in
        // the background — the user has moved on and shouldn't keep fetching.
        if matches!(self.stack.last(), Some(Screen::ChapterList { .. })) {
            if let Some(worker) = self.predownloader.as_mut() {
                worker.cancel_pending();
            }
            // …and don't let a wake re-fire the abandoned look-ahead.
            self.lookahead = None;
        }
        self.stack.pop();
        // Returning to the library: rebuild it from disk so chapters downloaded
        // (and the last-opened record written) while it sat on the stack are
        // reflected. Without this the card is a stale snapshot, and a cover tap
        // resumes against chapters that don't include what you just read — so it
        // falls back to an earlier chapter.
        self.refresh_library_in_place()?;
        self.render_current(RefreshMode::Full)?;
        Ok(Flow::Continue)
    }

    /// Scan the library, group it into shelf cards, and order them
    /// most-recently-read first (top left).
    fn scan_library_items(&self) -> Result<Vec<SeriesCard>> {
        let mut items = group_library(Library::new(&self.library_dir).scan()?);
        self.with_progress(|_, store| sort_library_by_recency(&mut items, store));
        Ok(items)
    }

    /// If the current top screen is the Library, rebuild its cards from a fresh
    /// disk scan, keeping the shelf page (clamped). Cheap and only runs when the
    /// Library is actually showing.
    fn refresh_library_in_place(&mut self) -> Result<()> {
        if !matches!(self.stack.last(), Some(Screen::Library { .. })) {
            return Ok(());
        }
        let items = self.scan_library_items()?;
        let capacity = self.shelf_layout().capacity().max(1);
        let max_page = items.len().div_ceil(capacity).saturating_sub(1);
        if let Some(Screen::Library { items: slot, page }) = self.stack.last_mut() {
            *page = (*page).min(max_page);
            *slot = items;
        }
        Ok(())
    }

    fn show_error(&mut self, error: &anyhow::Error) -> Result<()> {
        self.push(Screen::Message {
            title: "Error".to_string(),
            body: format!("{error:#}"),
        })
    }

    // --- input handling ---

    fn handle_tap(&mut self, x: u32, y: u32) -> Result<Flow> {
        let paged = self.current_page_count() > 1;
        match self.layout.tap_target(x, y, paged) {
            TapTarget::Back => self.pop(),
            TapTarget::First => {
                self.move_page(PageMove::First)?;
                Ok(Flow::Continue)
            }
            TapTarget::Prev => {
                self.move_page(PageMove::Delta(-1))?;
                Ok(Flow::Continue)
            }
            TapTarget::Next => {
                self.move_page(PageMove::Delta(1))?;
                Ok(Flow::Continue)
            }
            TapTarget::Last => {
                self.move_page(PageMove::Last)?;
                Ok(Flow::Continue)
            }
            TapTarget::None => Ok(Flow::Continue),
            TapTarget::Row(row) => self.activate(row, x, y),
            TapTarget::Title => {
                // A chapter list's title bar carries the sort button on its
                // right edge; tapping it cycles the order.
                if self.screen_has_sort() && x >= sort_button_x(&self.layout) {
                    self.cycle_chapter_sort()?;
                } else if matches!(self.screen(), Screen::Home) {
                    let (w, th) = (self.layout.width, self.layout.title_h);
                    let sends = crate::sync::cached_sends(&self.library_dir);
                    // Power sits at the far right; when a notification bell is
                    // shown (queued sends), it takes the next slot to the left.
                    let power_zone = if sends.is_empty() { th * 2 } else { th };
                    if x >= w.saturating_sub(power_zone) {
                        // The power symbol lives in Home's top-right corner.
                        self.push(Screen::PowerMenu)?;
                    } else if !sends.is_empty() && x >= w.saturating_sub(th * 2) {
                        // The bell: what the web queued to send to this device.
                        self.push(Screen::SentList { items: sends })?;
                    } else if x < w / 2 {
                        // The active profile name sits in the title's left
                        // half: tapping it opens the profile picker.
                        self.open_profile_menu()?;
                    }
                } else if matches!(self.screen(), Screen::Library { .. }) {
                    // The Library title bar toggles between the cover shelf
                    // and the dense metadata list. Two views of one library:
                    // art to browse by, data to decide by.
                    self.toggle_library_view()?;
                }
                Ok(Flow::Continue)
            }
        }
    }

    /// Whether the current screen is a chapter list (and so shows the sort
    /// button in its title bar).
    fn screen_has_sort(&self) -> bool {
        matches!(
            self.stack.last(),
            Some(Screen::ChapterList { .. } | Screen::DownloadedChapters { .. })
        )
    }

    /// Advance the current chapter list's sort to the next order and redraw
    /// from the first page (the rows have all moved, so the old page is
    /// meaningless).
    fn cycle_chapter_sort(&mut self) -> Result<()> {
        match self.stack.last_mut() {
            Some(Screen::ChapterList { sort, page, .. })
            | Some(Screen::DownloadedChapters { sort, page, .. }) => {
                *sort = sort.next();
                *page = 0;
            }
            _ => return Ok(()),
        }
        self.render_current(RefreshMode::Partial)
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
                let (entry, read_key) = self.with_progress(|_, store| {
                    (
                        card.resume_chapter(store).clone(),
                        card.latest_read(store).map(|c| c.relative_path.clone()),
                    )
                });
                let series_dir = entry
                    .relative_path
                    .split('/')
                    .next()
                    .unwrap_or(&entry.relative_path)
                    .to_string();
                self.push(Screen::BookMenu {
                    entry,
                    series_dir,
                    read_key,
                })?;
                Ok(Flow::Continue)
            }
            Screen::ChapterList {
                source,
                manga,
                chapters,
                page,
                sort,
            } => {
                // Long press on a chapter row: download it and stay on the
                // list — for stocking up before going offline.
                let paged = self.current_page_count() > 1;
                if let TapTarget::Row(row) = self.layout.tap_target(x, y, paged) {
                    let nums: Vec<Option<f32>> = chapters.iter().map(|c| c.num).collect();
                    let order = chapter_display_order(&nums, sort);
                    let displayed = page * self.layout.rows_per_page() + row;
                    if let Some(chapter) =
                        order.get(displayed).and_then(|&i| chapters.get(i)).cloned()
                    {
                        let cbz_path = self.download_to_library(&source, &manga, &chapter)?;
                        // No reader session here — fetch the cover now.
                        self.fetch_cover_if_missing(&manga, &cbz_path);
                        // Stock up the next few chapters too (the whole point
                        // of a long-press download is going offline).
                        self.predownload_ahead(&source, &manga, &chapters, &chapter.id);
                        self.input.discard_taps();
                        self.render_current(RefreshMode::Full)?;
                    }
                }
                Ok(Flow::Continue)
            }
            Screen::Sources { rows, page } => {
                // Long press on an installed source offers to remove it —
                // mirroring the library's long-press book menu. Anywhere
                // else on the screen it's just a slow tap.
                let paged = self.current_page_count() > 1;
                if let TapTarget::Row(row) = self.layout.tap_target(x, y, paged) {
                    let index = page * self.layout.rows_per_page() + row;
                    if let Some(SourceRow::Installed(source)) = rows.get(index).cloned() {
                        self.push(Screen::ConfirmRemoveSource { source })?;
                        return Ok(Flow::Continue);
                    }
                }
                self.handle_tap(x, y)
            }
            _ => self.handle_tap(x, y),
        }
    }

    /// Open the book menu's "chapters" entry: the source's chapter list
    /// when the series is linked, otherwise the search keyboard prefilled
    /// with the series name so one download can re-link it.
    fn open_series_chapters(&mut self, series_dir: &str) -> Result<()> {
        // The behavior swaps on connectivity, automatically:
        //
        // - Online with a recorded source: fetch the full chapter list so you
        //   can grab chapters you don't have yet.
        // - Offline (or no source link, or the fetch fails): show what's on
        //   disk. Crucially we do NOT try to bring Wi-Fi up here — viewing your
        //   downloads must never be blocked by the UI attempting to connect.
        if self.is_online() {
            let index = gideon_core::SeriesIndex::load(&self.library_dir);
            if let Some(origin) = index.get(series_dir) {
                let source = SourceEntry {
                    id: origin.source_id.clone(),
                    name: origin.source_name.clone(),
                };
                let manga = MangaEntry {
                    id: origin.manga_id.clone(),
                    title: origin.manga_title.clone(),
                    cover_url: origin.cover_url.clone(),
                };
                // The `origin` borrow of `index` ends with these clones.
                if self.try_open_chapter_list(&source, &manga).is_ok() {
                    return Ok(());
                }
                // Source reachable but the fetch failed — fall through to the
                // downloaded list rather than stranding the user.
            }
        }
        self.open_downloaded_chapters(series_dir)
    }

    /// Like [`Self::open_chapter_list`] but assumes we're already online (the
    /// caller decides) and returns the fetch error instead of propagating, so a
    /// source failure can fall back to the offline downloaded list.
    ///
    /// An **empty** chapter list is treated as a soft failure too: a source that
    /// is rate-limited, outdated, or has had the manga delisted can return no
    /// chapters without erroring, and a next-SDK source may yield none for a
    /// chapters-only request. Stranding the reader on a blank screen when they
    /// have the chapters downloaded is never the right answer — bail so the
    /// caller falls back to what's on disk.
    fn try_open_chapter_list(&mut self, source: &SourceEntry, manga: &MangaEntry) -> Result<()> {
        self.show_status(&[&format!("Loading chapters of {}…", manga.title)])?;
        let chapters = self.gateway.chapters(&source.id, &manga.id)?;
        if chapters.is_empty() {
            anyhow::bail!("source returned no chapters for {}", manga.title);
        }
        self.push(Screen::ChapterList {
            source: source.clone(),
            manga: manga.clone(),
            chapters,
            page: 0,
            sort: ChapterSort::default(),
        })
    }

    /// Show the series' downloaded chapters from local files — no source fetch,
    /// so it works fully offline. Tapping a row opens that CBZ in the reader.
    fn open_downloaded_chapters(&mut self, series_dir: &str) -> Result<()> {
        let entries = self.downloaded_entries(series_dir);
        if entries.is_empty() {
            return self.push(Screen::Message {
                title: "Nothing downloaded".to_string(),
                body: "This series has no chapters saved on the device yet.".to_string(),
            });
        }
        self.push(Screen::DownloadedChapters {
            title: series_dir.to_string(),
            entries,
            page: 0,
            sort: ChapterSort::default(),
        })
    }

    /// The downloaded chapters belonging to a series directory (or a single
    /// loose CBZ), in natural reading order.
    fn downloaded_entries(&self, series_dir: &str) -> Vec<LibraryEntry> {
        let prefix = format!("{series_dir}/");
        Library::new(&self.library_dir)
            .scan()
            .unwrap_or_default()
            .into_iter()
            .filter(|e| e.relative_path == series_dir || e.relative_path.starts_with(&prefix))
            .collect()
    }

    /// Rebuild the Library screen beneath the book menu after a delete.
    /// Carry out a confirmed delete from the book menu, then return to a
    /// freshly-scanned library. Nothing here runs until the user has confirmed.
    fn perform_delete(
        &mut self,
        entry: &LibraryEntry,
        series_dir: &str,
        scope: DeleteScope,
    ) -> Result<()> {
        match scope {
            DeleteScope::Chapter => {
                // Delete this chapter's file; drop it from the series' download
                // history.
                std::fs::remove_file(&entry.path)
                    .with_context(|| format!("couldn't delete {}", entry.path.display()))?;
                if let Some(file) = entry.path.file_name() {
                    let mut index = gideon_core::SeriesIndex::load(&self.library_dir);
                    index.forget_download(series_dir, &file.to_string_lossy());
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
            }
            DeleteScope::Series => {
                // Delete the whole series directory.
                let target = entry
                    .path
                    .parent()
                    .filter(|p| *p != self.library_dir)
                    .map(|p| p.to_path_buf());
                match target {
                    Some(dir) => std::fs::remove_dir_all(&dir)
                        .with_context(|| format!("couldn't delete {}", dir.display()))?,
                    None => std::fs::remove_file(&entry.path)
                        .with_context(|| format!("couldn't delete {}", entry.path.display()))?,
                }
                let mut index = gideon_core::SeriesIndex::load(&self.library_dir);
                index.remove(series_dir);
                let _ = index.save(&self.library_dir);
            }
        }
        self.refresh_library_after_delete()
    }

    fn refresh_library_after_delete(&mut self) -> Result<()> {
        let items = self.scan_library_items()?;
        // Unwind whatever sits above the library (the book menu, plus the delete
        // confirmation when the delete came through it) and refresh it in place.
        while self.stack.len() > 1 && !matches!(self.stack.last(), Some(Screen::Library { .. })) {
            self.stack.pop();
        }
        if let Some(screen @ Screen::Library { .. }) = self.stack.last_mut() {
            *screen = Screen::Library { items, page: 0 };
        }
        self.render_current(RefreshMode::Full)
    }

    /// The number of pages the current top screen spans (1 when it isn't a
    /// paginated list). Mirrors the page-count arithmetic in [`Self::move_page`]
    /// — note the Library paginates by shelf capacity, not list rows.
    fn current_page_count(&self) -> usize {
        let per_page = self.layout.rows_per_page();
        let shelf_capacity = self.shelf_layout().capacity().max(1);
        match self.stack.last() {
            Some(Screen::Library { items, .. }) => items.len().div_ceil(shelf_capacity),
            Some(Screen::Sources { rows, .. }) => rows.len().div_ceil(per_page),
            Some(Screen::SearchResults { results, .. }) => (results.len() + 1).div_ceil(per_page),
            Some(Screen::MangaList { mangas, .. }) => mangas.len().div_ceil(per_page),
            Some(Screen::ChapterList { chapters, .. }) => chapters.len().div_ceil(per_page),
            Some(Screen::DownloadedChapters { entries, .. }) => entries.len().div_ceil(per_page),
            _ => 1,
        }
        .max(1)
    }

    /// Step one page forward/backward within the current screen.
    fn flip_page(&mut self, delta: i64) -> Result<()> {
        self.move_page(PageMove::Delta(delta))
    }

    /// Move within the current paginated screen (partial refresh). `Delta`
    /// steps relative to the current page (clamped); `First`/`Last` jump to an
    /// end — so a long chapter list is one tap from the start instead of many.
    fn move_page(&mut self, mv: PageMove) -> Result<()> {
        let per_page = self.layout.rows_per_page();
        let shelf_capacity = self.shelf_layout().capacity().max(1);
        let Some(screen) = self.stack.last_mut() else {
            return Ok(());
        };
        let (page, count) = match screen {
            Screen::Library { items, page } => (page, items.len().div_ceil(shelf_capacity)),
            Screen::Sources { rows, page } => (page, rows.len().div_ceil(per_page)),
            // +1 for the trailing "Search more sources" row.
            Screen::SearchResults { results, page, .. } => {
                (page, (results.len() + 1).div_ceil(per_page))
            }
            Screen::Popular { mangas, page } => (page, mangas.len().div_ceil(per_page)),
            Screen::MangaList { mangas, page, .. } => (page, mangas.len().div_ceil(per_page)),
            Screen::ChapterList { chapters, page, .. } => (page, chapters.len().div_ceil(per_page)),
            Screen::DownloadedChapters { entries, page, .. } => {
                (page, entries.len().div_ceil(per_page))
            }
            _ => return Ok(()),
        };
        let count = count.max(1);
        let new = match mv {
            PageMove::Delta(delta) => (*page as i64 + delta).clamp(0, count as i64 - 1) as usize,
            PageMove::First => 0,
            PageMove::Last => count - 1,
        };
        if new != *page {
            *page = new;
            self.render_current(RefreshMode::Partial)?;
        }
        Ok(())
    }

    /// Activate whatever sits at content row `row` (tap at `x`, `y`).
    fn activate(&mut self, mut row: usize, x: u32, y: u32) -> Result<Flow> {
        let screen = self.stack.last().cloned().expect("stack never empty");
        match screen {
            Screen::Home => {
                // Row 0 is the offline "reconnect Wi-Fi" button when shown;
                // the standard rows are offset past it. The offset comes from
                // the cached paint-time state, so it matches what was drawn.
                if self.home_offline {
                    if row == 0 {
                        self.reconnect_wifi()?;
                        return Ok(Flow::Continue);
                    }
                    row -= 1;
                }
                match row {
                    0 => self.open_library()?,
                    1 => self.open_global_search()?,
                    2 => self.open_sources()?,
                    3 => self.push(Screen::Settings)?,
                    4 => self.check_updates()?,
                    5 => self.open_popular()?,
                    6 => self.push(Screen::Stats)?,
                    _ => {}
                }
                Ok(Flow::Continue)
            }
            // Nothing on the stats screen is tappable — Back in the chrome is
            // handled before row dispatch, like every other read-only screen.
            Screen::Stats => Ok(Flow::Continue),
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
                        self.keyboard_shift = false;
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
            Screen::RecentSearches { recents } => {
                // Row 0 starts a new search; the rest reopen cached recents.
                if row == 0 {
                    self.open_search_keyboard()?;
                } else if let Some((query, _)) = recents.get(row - 1) {
                    self.reopen_recent(query)?;
                }
                Ok(Flow::Continue)
            }
            Screen::SearchResults { results, page, .. } => {
                let index = page * self.layout.rows_per_page() + row;
                match results.get(index).cloned() {
                    Some((source, manga)) => self.open_chapter_list(&source, &manga)?,
                    // The slot just past the last result is the "Search more
                    // sources" row — widen to not-yet-installed sources.
                    None if index == results.len() => self.widen_search()?,
                    None => {}
                }
                Ok(Flow::Continue)
            }
            Screen::Popular { mangas, page } => {
                let index = page * self.layout.rows_per_page() + row;
                if let Some(manga) = mangas.get(index).cloned() {
                    // Reuse the global search: find this MyAnimeList title
                    // across the installed sources so the user can download
                    // it. The results land on top of this tab; Back returns.
                    self.run_global_search(&manga.title)?;
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
                sort,
            } => {
                // Map the tapped row through the current display order back to
                // the chapter's index in the (source-order) Vec.
                let nums: Vec<Option<f32>> = chapters.iter().map(|c| c.num).collect();
                let order = chapter_display_order(&nums, sort);
                let displayed = page * self.layout.rows_per_page() + row;
                if let Some(&index) = order.get(displayed) {
                    let chapter = chapters[index].clone();
                    // Tap on the ⋮ button opens the chapter action menu (download
                    // this one / from here, mark read/unread, delete); the rest of
                    // the row opens/downloads the chapter.
                    if chapter_kebab_tapped(&self.layout, x) {
                        let key = self
                            .downloaded_chapter_path(&source, &manga, &chapter.id)
                            .map(|p| progress_key(&self.library_dir, &p));
                        let context = DownloadContext {
                            source: source.clone(),
                            manga: manga.clone(),
                            chapters: chapters.clone(),
                            index,
                        };
                        return self.open_chapter_menu(chapter.label(), key, Some(context));
                    }
                    return self.download_and_read(&source, &manga, &chapter, &chapters);
                }
                Ok(Flow::Continue)
            }
            Screen::DownloadedChapters {
                entries,
                page,
                sort,
                ..
            } => {
                let nums: Vec<Option<f32>> = entries
                    .iter()
                    .map(|e| label_chapter_num(&chapter_label(&e.relative_path)))
                    .collect();
                let order = chapter_display_order(&nums, sort);
                let displayed = page * self.layout.rows_per_page() + row;
                if let Some(&index) = order.get(displayed) {
                    let entry = &entries[index];
                    if chapter_kebab_tapped(&self.layout, x) {
                        let title = chapter_label(&entry.relative_path);
                        let key = Some(entry.relative_path.clone());
                        return self.open_chapter_menu(title, key, None);
                    }
                    return self.read_downloaded_chain(&entries, index);
                }
                Ok(Flow::Continue)
            }
            Screen::ChapterMenu {
                key,
                finished,
                download,
                ..
            } => {
                let rows = chapter_menu_rows(&download, &key, finished);
                let action = rows.get(row).filter(|r| r.1).map(|r| r.2);
                match action {
                    Some(ChapterMenuAction::DownloadThis) => {
                        let ctx = download.expect("DownloadThis only with a source link");
                        let chapter = ctx.chapters[ctx.index].clone();
                        self.pop()?; // back to the chapter list, then download over it
                        let cbz = self.download_to_library(&ctx.source, &ctx.manga, &chapter)?;
                        self.fetch_cover_if_missing(&ctx.manga, &cbz);
                        self.input.discard_taps();
                        self.render_current(RefreshMode::Full)?;
                    }
                    Some(ChapterMenuAction::DownloadAhead) => {
                        let ctx = download.expect("DownloadAhead only with a source link");
                        // Replace the menu with the count picker (no extra flash).
                        self.stack.pop();
                        self.push(Screen::DownloadAheadMenu {
                            source: ctx.source,
                            manga: ctx.manga,
                            chapters: ctx.chapters,
                            index: ctx.index,
                        })?;
                    }
                    Some(ChapterMenuAction::MarkRead) => {
                        if let Some(key) = key {
                            self.mark_read(&key)?;
                            self.show_status(&["Marked as read."])?;
                        }
                        self.pop()?;
                    }
                    Some(ChapterMenuAction::MarkUnread) => {
                        if let Some(key) = key {
                            self.mark_unread(&key)?;
                            self.show_status(&["Marked as unread."])?;
                        }
                        self.pop()?;
                    }
                    Some(ChapterMenuAction::DeleteDownload) => {
                        if let Some(key) = key {
                            self.delete_download(&key)?;
                        }
                        self.pop()?; // back to the list, rebuilt to drop the file
                        self.refresh_downloaded_in_place()?;
                    }
                    // A disabled row (e.g. the "download to track" hint) — close.
                    None => {
                        self.pop()?;
                    }
                }
                Ok(Flow::Continue)
            }
            Screen::DownloadAheadMenu {
                source,
                manga,
                chapters,
                index,
            } => {
                let remaining = chapters.len().saturating_sub(index);
                let rows = download_ahead_rows(remaining);
                if let Some((_, count)) = rows.get(row).cloned() {
                    let queued =
                        self.queue_batch_download(&source, &manga, &chapters, index, count)?;
                    self.stack.pop(); // leave the picker
                    let body = if queued == 0 {
                        "Those chapters are already downloaded.".to_string()
                    } else {
                        format!(
                            "Downloading {queued} chapter{} in the background.\n\
                             They'll appear as they finish.",
                            if queued == 1 { "" } else { "s" }
                        )
                    };
                    self.push(Screen::Message {
                        title: "Downloading".to_string(),
                        body,
                    })?;
                }
                Ok(Flow::Continue)
            }
            Screen::BookMenu {
                entry,
                series_dir,
                read_key,
            } => {
                match row {
                    0 => self.open_series_chapters(&series_dir)?,
                    1 => {
                        // Mark as unread: forget the latest-read chapter's
                        // progress (an "I tapped the wrong thing" undo). The
                        // card's resume point falls back to the prior read.
                        if let Some(key) = read_key {
                            if self.mark_unread(&key)? {
                                self.show_status(&["Marked as unread."])?;
                            }
                        }
                        self.pop()?;
                    }
                    2 => {
                        // Deleting is irreversible, so confirm first instead of
                        // acting on the long press directly (a mis-hold once wiped
                        // a title outright).
                        self.push(Screen::ConfirmDelete {
                            entry,
                            series_dir,
                            scope: DeleteScope::Chapter,
                        })?;
                    }
                    3 => {
                        self.push(Screen::ConfirmDelete {
                            entry,
                            series_dir,
                            scope: DeleteScope::Series,
                        })?;
                    }
                    _ => {}
                }
                Ok(Flow::Continue)
            }
            Screen::ConfirmDelete {
                entry,
                series_dir,
                scope,
            } => {
                // Row 0 confirms the delete; anything else (the Cancel row) just
                // backs out to the book menu, touching nothing on disk.
                if row == 0 {
                    self.perform_delete(&entry, &series_dir, scope)?;
                } else {
                    self.pop()?;
                }
                Ok(Flow::Continue)
            }
            Screen::ConfirmRemoveSource { source } => {
                // Row 0 removes the source; anything else backs out to the
                // Sources screen, touching nothing.
                if row == 0 {
                    self.remove_source(&source)?;
                } else {
                    self.pop()?;
                }
                Ok(Flow::Continue)
            }
            Screen::Settings => {
                self.tap_setting(row)?;
                Ok(Flow::Continue)
            }
            Screen::Storage => {
                if row == STORAGE_FREE_ROW {
                    let freed = self.enforce_storage_limit();
                    let msg = if freed > 0 {
                        format!("Freed {}.", gideon_core::StorageSize(freed))
                    } else {
                        "Already within the storage limit.".to_string()
                    };
                    self.show_status(&[&msg])?;
                    self.render_current(RefreshMode::Full)?;
                }
                Ok(Flow::Continue)
            }
            Screen::AccountMenu => {
                self.tap_account_menu(row)?;
                Ok(Flow::Continue)
            }
            Screen::AccountEmail { email } => {
                self.tap_account_email(&email, x, y)?;
                Ok(Flow::Continue)
            }
            Screen::AccountPassword { email, password } => {
                self.tap_account_password(&email, &password, x, y)?;
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
                    self.keyboard_shift = false;
                    self.push(Screen::NewProfile {
                        name: String::new(),
                    })?;
                } else if row == profiles.len() + 1
                    && profiles.iter().any(|p| p == gideon_core::DEFAULT_PROFILE)
                {
                    self.keyboard_paints = 0;
                    self.keyboard_shift = false;
                    self.push(Screen::ConvertDefault {
                        name: String::new(),
                    })?;
                }
                Ok(Flow::Continue)
            }
            Screen::NewProfile { name } => {
                self.tap_new_profile(&name, x, y)?;
                Ok(Flow::Continue)
            }
            Screen::ConvertDefault { name } => {
                self.tap_convert_default(&name, x, y)?;
                Ok(Flow::Continue)
            }
            Screen::PowerMenu => match row {
                // Wi-Fi networks (scan/connect) at the top of the Power menu.
                0 => {
                    self.open_wifi()?;
                    Ok(Flow::Continue)
                }
                1 => Ok(Flow::Quit(Exit::Restart)),
                2 => Ok(Flow::Quit(Exit::Close)),
                _ => Ok(Flow::Continue),
            },
            Screen::SentList { items } => {
                if let Some(item) = items.get(row).cloned() {
                    // Mark it handled (server + local cache) so the bell clears,
                    // then run the on-device search for the title — the reader
                    // picks the right match on the results screen and adds it.
                    crate::sync::mark_send_opened_bg(&self.library_dir, &item.id);
                    crate::sync::forget_cached_send(&self.library_dir, &item.id);
                    self.stack.pop(); // leave the sent list before searching
                    self.run_global_search(&item.title)?;
                }
                Ok(Flow::Continue)
            }
            Screen::WifiList { networks } => {
                let n = networks.len();
                if row == 0 {
                    // The Wi-Fi toggle (currently on): flip it off and close the
                    // whole Wi-Fi/Power menu — back to the library, not lingering
                    // on a parent menu.
                    gideon_device::network::disable_wifi();
                    self.stack.truncate(1);
                    self.render_current(RefreshMode::Full)?;
                } else if row <= n {
                    let net = networks[row - 1].clone();
                    self.tap_wifi_network(&net)?;
                } else if row == n + 1 {
                    self.refresh_wifi_list()?; // "Scan again"
                }
                Ok(Flow::Continue)
            }
            Screen::WifiPassword { ssid, password } => {
                self.tap_wifi_password(&ssid, &password, x, y)?;
                Ok(Flow::Continue)
            }
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
        // Opening the library is a good "app foreground" moment to pull any
        // progress made on another device (and push anything we're ahead on).
        // Fully background — the shelf renders immediately regardless.
        self.trigger_sync();
        let items = self.scan_library_items()?;
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

    /// Remove an installed source (from its confirmation screen), then land
    /// back on a rebuilt Sources screen. Only the source package goes —
    /// downloaded chapters and reading progress stay in the library.
    fn remove_source(&mut self, source: &SourceEntry) -> Result<()> {
        self.show_status(&[&format!("Removing {}…", source.name)])?;
        self.gateway
            .uninstall_source(&source.id)
            .with_context(|| format!("failed to remove {}", source.name))?;
        self.stack.pop(); // leave the confirmation screen (repainted below)
                          // Rebuild the sources screen in place so the row disappears; if the
                          // list fetch fails (offline), fall back to just repainting.
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

    // --- sync account ---

    /// The active profile's sync [`Account`], if sync is configured (it always
    /// is via the build default, unless disabled by an empty env override).
    fn account(&self) -> Option<gideon_sync::account::Account> {
        crate::sync::account(&self.library_dir)
    }

    /// The signed-in email for the active profile, if any.
    fn account_email(&self) -> Option<String> {
        self.account().and_then(|a| a.email())
    }

    /// Kick a best-effort background reconcile for the active profile. Never
    /// blocks (spawns a thread) and no-ops when signed out — safe on any thread.
    fn trigger_sync(&self) {
        // Hand the sync thread a background gateway clone so it can also resolve
        // and publish page URLs (device-publish), lighting up downloaded
        // chapters on the web reader. `None` (test gateways) just syncs progress.
        crate::sync::spawn_sync(&self.library_dir, self.gateway.background_clone());
    }

    /// Tap on the account menu: sign in (signed out), or sync-now / sign-out.
    fn tap_account_menu(&mut self, row: usize) -> Result<()> {
        match (self.account_email(), row) {
            (Some(_), 1) => {
                self.trigger_sync();
                self.show_status(&["Syncing in the background…"])?;
                self.render_current(RefreshMode::Full)?;
            }
            (Some(_), 2) => {
                if let Some(account) = self.account() {
                    let _ = account.sign_out();
                }
                self.pop()?; // back to Settings, now showing "sign in"
            }
            (None, 0) => {
                self.keyboard_paints = 0;
                self.keyboard_shift = false;
                self.push(Screen::AccountEmail {
                    email: String::new(),
                })?;
            }
            _ => {}
        }
        Ok(())
    }

    /// Email keyboard: the action key advances to the password screen (no
    /// network yet); other keys edit the address in place.
    fn tap_account_email(&mut self, email: &str, x: u32, y: u32) -> Result<()> {
        let key = self.layout.key_at(x, y);
        if key == Some(Key::Search) {
            let addr = email.trim().to_string();
            if addr.is_empty() {
                return Ok(());
            }
            self.keyboard_paints = 0;
            self.keyboard_shift = false;
            self.push(Screen::AccountPassword {
                email: addr,
                password: String::new(),
            })?;
            return Ok(());
        }
        if self.toggle_shift_if_pressed(key)? {
            return Ok(());
        }
        if let Some(v) = key.and_then(|key| apply_key_edit(email, key, self.keyboard_shift)) {
            if let Some(Screen::AccountEmail { email }) = self.stack.last_mut() {
                *email = v;
            }
            self.keyboard_repaint()?;
        }
        Ok(())
    }

    /// Password keyboard: the action key signs in (email + password), stores the
    /// session, and triggers a first sync; other keys edit the password in place.
    fn tap_account_password(&mut self, email: &str, password: &str, x: u32, y: u32) -> Result<()> {
        let key = self.layout.key_at(x, y);
        if key == Some(Key::Search) {
            if password.is_empty() {
                return Ok(());
            }
            let Some(account) = self.account() else {
                return Ok(());
            };
            self.show_status(&["Signing in…"])?;
            match account.sign_in(email, password, crate::sync::now()) {
                Ok(_) => {
                    self.trigger_sync();
                    // Unwind the two keyboard screens back to the account menu,
                    // which now renders the signed-in state.
                    self.stack.pop(); // AccountPassword
                    self.stack.pop(); // AccountEmail
                    self.show_status(&["Signed in. Syncing…"])?;
                    self.render_current(RefreshMode::Full)?;
                }
                Err(e) => self.push(Screen::Message {
                    title: "Couldn't sign in".to_string(),
                    body: format!("{e}\nCheck your email and password, then try again."),
                })?,
            }
            return Ok(());
        }
        if self.toggle_shift_if_pressed(key)? {
            return Ok(());
        }
        if let Some(v) = key.and_then(|key| apply_key_edit(password, key, self.keyboard_shift)) {
            if let Some(Screen::AccountPassword { password, .. }) = self.stack.last_mut() {
                *password = v;
            }
            self.keyboard_repaint()?;
        }
        Ok(())
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
        if self.toggle_shift_if_pressed(key)? {
            return Ok(());
        }
        if let Some(q) = key.and_then(|key| apply_key_edit(query, key, self.keyboard_shift)) {
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
        if self.toggle_shift_if_pressed(key)? {
            return Ok(());
        }
        if let Some(n) = key.and_then(|key| apply_key_edit(name, key, self.keyboard_shift)) {
            if let Some(Screen::NewProfile { name }) = self.stack.last_mut() {
                *name = n;
            }
            self.keyboard_repaint()?;
        }
        Ok(())
    }

    /// Handle a tap on the name-the-default-profile keyboard: edit the name in
    /// place, or (action key) convert the default profile to that name.
    fn tap_convert_default(&mut self, name: &str, x: u32, y: u32) -> Result<()> {
        let key = self.layout.key_at(x, y);
        if key == Some(Key::Search) {
            let trimmed = name.trim().to_string();
            if !trimmed.is_empty() {
                self.convert_default_profile(&trimmed)?;
            }
            return Ok(());
        }
        if self.toggle_shift_if_pressed(key)? {
            return Ok(());
        }
        if let Some(n) = key.and_then(|key| apply_key_edit(name, key, self.keyboard_shift)) {
            if let Some(Screen::ConvertDefault { name }) = self.stack.last_mut() {
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

    /// If `key` is Shift, flip the keyboard's case mode and repaint, returning
    /// `true` (the caller then skips buffer editing). Otherwise `false`.
    fn toggle_shift_if_pressed(&mut self, key: Option<Key>) -> Result<bool> {
        if key == Some(Key::Shift) {
            self.keyboard_shift = !self.keyboard_shift;
            self.keyboard_repaint()?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    // --- settings screen ---

    /// Cycle the setting on row `row` to its next value, persist
    /// settings.json immediately (atomic save) and repaint in place.
    fn tap_setting(&mut self, row: usize) -> Result<()> {
        // Row 6 opens the Wi-Fi screen (scan + connect) — an action, not a
        // stored field.
        if row == 6 {
            return self.open_wifi();
        }
        // The trailing storage row opens the usage detail screen.
        if row == SETTINGS_STORAGE_ROW {
            return self.open_storage();
        }
        // The trailing account row opens the sync account menu.
        if row == SETTINGS_ACCOUNT_ROW {
            return self.push(Screen::AccountMenu);
        }
        let mut settings = self.load_settings();
        match row {
            10 => {
                let current = settings.color_profile.as_str();
                let next = COLOR_PROFILE_STEPS
                    .iter()
                    .position(|p| *p == current)
                    .map_or(0, |i| (i + 1) % COLOR_PROFILE_STEPS.len());
                settings.color_profile = COLOR_PROFILE_STEPS[next].to_string();
            }
            11 => {
                settings.finished_cleanup_hours = cycle(
                    &gideon_core::FINISHED_CLEANUP_STEPS,
                    settings.finished_cleanup_hours,
                );
            }
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
            7 => {
                // Auto-connect Wi-Fi on/off: whether gideon brings the radio
                // up on its own before actions and on wake.
                settings.wifi_auto_connect = !settings.wifi_auto_connect;
                self.wifi_auto_connect = settings.wifi_auto_connect;
            }
            8 => {
                // Rotate wide spreads on/off — applies to the next opened book.
                settings.auto_rotate_spreads = !settings.auto_rotate_spreads;
                self.auto_rotate_spreads = settings.auto_rotate_spreads;
            }
            9 => {
                // Cycle the idle auto-suspend timeout (5/10/15/30/60 min,
                // never) and apply it to the live event loops immediately.
                settings.idle_suspend_minutes =
                    cycle(&IDLE_SUSPEND_STEPS, settings.idle_suspend_minutes);
                self.idle_suspend = idle_suspend_duration(settings.idle_suspend_minutes);
            }
            _ => return Ok(()),
        }
        self.save_settings(&settings);
        // Lowering the storage limit should take effect now, not just on the
        // next download — evict down to the new budget immediately.
        if row == 1 {
            self.enforce_storage_limit();
        }
        self.render_current(RefreshMode::Partial)
    }

    // --- profiles ---

    /// Current settings; defaults when no settings dir is configured
    /// (tests, headless) or the file is unreadable.
    /// The settings in force for the active profile: the device-global file,
    /// overlaid with whatever this profile has stated for itself.
    ///
    /// Two readers sharing a Kobo share the frontlight, the radio and the
    /// disk, but not a reading fit or a colour theme. A profile that has
    /// stated nothing inherits the device value, so an upgrading device
    /// behaves exactly as it did before any per-profile file existed.
    fn load_settings(&self) -> gideon_core::Settings {
        let device = self
            .settings_dir
            .as_deref()
            .map(|dir| gideon_core::Settings::load(dir).unwrap_or_default())
            .unwrap_or_default();
        device.with_profile(&gideon_core::ProfileSettings::load(&self.library_dir))
    }

    /// Persist settings (no-op without a settings dir); a failed save is
    /// logged, never fatal.
    /// Persist a settings change to whichever file owns each field.
    ///
    /// The personal fields go to this profile's own file. The device-global
    /// ones are written back onto the device file as it is on disk, rather
    /// than saving the merged struct wholesale — saving the merge would bake
    /// this profile's taste into the device defaults, which is precisely what
    /// every other profile falls back to.
    fn save_settings(&self, settings: &gideon_core::Settings) {
        if let Err(e) =
            gideon_core::ProfileSettings::from_settings(settings).save(&self.library_dir)
        {
            eprintln!("gideon: couldn't save profile settings: {e}");
        }
        if let Some(dir) = &self.settings_dir {
            let mut device = gideon_core::Settings::load(dir).unwrap_or_default();
            device.profiles = settings.profiles.clone();
            device.active_profile = settings.active_profile.clone();
            device.source_lists = settings.source_lists.clone();
            device.languages = settings.languages.clone();
            device.storage_size_limit = settings.storage_size_limit;
            device.auto_check_updates = settings.auto_check_updates;
            device.color_post_process = settings.color_post_process.clone();
            device.wifi_auto_connect = settings.wifi_auto_connect;
            device.idle_suspend_minutes = settings.idle_suspend_minutes;
            device.frontlight_brightness = settings.frontlight_brightness;
            device.frontlight_warmth = settings.frontlight_warmth;
            if let Err(e) = device.save(dir) {
                eprintln!("gideon: couldn't save settings: {e}");
            }
        }
    }

    /// Every profile the device knows: the ones settings.json lists, plus every
    /// profile that has a library directory on disk, plus the active one.
    ///
    /// The disk half is what makes a lost list survivable. settings.json can go
    /// missing or come back unparseable (a yanked USB cable mid-write, a
    /// hand-edit), and `load_settings` then answers with the defaults — a list
    /// of just "default". Trusting that alone would drop a profile out of the
    /// picker while all of its books sat untouched in `@name`, and the next save
    /// would make the omission permanent. So the profile's own directory is
    /// enough to keep it listed, whatever settings.json says.
    fn known_profiles(&self) -> Vec<String> {
        let mut profiles = self.load_settings().profiles;
        for name in gideon_core::profile::discover(&self.base_library) {
            if !profiles.contains(&name) {
                profiles.push(name);
            }
        }
        if !profiles.contains(&self.active_profile) {
            profiles.push(self.active_profile.clone());
        }
        profiles
    }

    /// Open the profile picker from Home's title bar.
    fn open_profile_menu(&mut self) -> Result<()> {
        let profiles = self.known_profiles();
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
        // The pre-downloader's worker thread closes over the library dir it was
        // built with and keeps writing (and recording into the series index)
        // there for its whole life. Drop it so the next queue respawns a fresh
        // worker scoped to the new profile — otherwise chapters queued after a
        // switch would land in the *previous* profile's library, mixing the two
        // profiles' downloads.
        self.predownloader = None;
        std::fs::create_dir_all(&self.library_dir).with_context(|| {
            format!(
                "couldn't create profile library {}",
                self.library_dir.display()
            )
        })?;
        let mut settings = self.load_settings();
        // Same as the conversion path: start from every profile that exists on
        // disk, so switching can't persist a list that read back short.
        settings.profiles = self.known_profiles();
        if !settings.profiles.iter().any(|p| p == name) {
            settings.profiles.push(name.to_string());
        }
        settings.active_profile = name.to_string();
        self.save_settings(&settings);
        self.stack.truncate(1);
        self.render_current(RefreshMode::Full)
    }

    /// Give the default profile a name, turning it into an ordinary profile:
    /// its library (the root itself, `.gideon` bookkeeping and all) moves into
    /// `@<name>`, and "default" stops existing. The library root is left as a
    /// pure container of profile directories.
    ///
    /// Nothing else about the profile changes — same books, same progress, same
    /// sync account — because all of it lives inside the directory that moved.
    fn convert_default_profile(&mut self, name: &str) -> Result<()> {
        // The progress cache and the pre-downloader's worker both hold paths
        // under the old location; retire them before anything moves.
        self.invalidate_progress_cache();
        self.predownloader = None;
        let target = match gideon_core::profile::convert_default(&self.base_library, name) {
            Ok(target) => target,
            // A taken or unusable name isn't an error worth an error screen —
            // say what's wrong and leave the library untouched.
            Err(e) => {
                return self.push(Screen::Message {
                    title: "Profiles".to_string(),
                    body: format!("{e}"),
                })
            }
        };
        let mut settings = self.load_settings();
        // Rebuild the list from what's actually on disk, so a settings file that
        // read back short can't make this save drop a profile permanently.
        settings.profiles = self.known_profiles();
        settings
            .profiles
            .retain(|p| p != gideon_core::DEFAULT_PROFILE && p != name);
        settings.profiles.insert(0, name.to_string());
        if settings.active_profile == gideon_core::DEFAULT_PROFILE {
            settings.active_profile = name.to_string();
        }
        self.save_settings(&settings);
        if self.active_profile == gideon_core::DEFAULT_PROFILE {
            self.active_profile = name.to_string();
            self.library_dir = target;
        }
        // The library that was on screen just moved — start over from Home.
        self.stack.truncate(1);
        self.render_current(RefreshMode::Full)
    }

    /// Open the "Popular manga" tab: MyAnimeList's popular ranking. It's a
    /// live fetch, so it needs the network and surfaces the offline message
    /// like every other network action. Tapping a title there runs a global
    /// search for it (handled in the tap dispatch).
    fn open_popular(&mut self) -> Result<()> {
        self.ensure_online()?;
        self.show_status(&["Loading popular manga…"])?;
        // A fetch failure here is almost always MyAnimeList itself being
        // down (its API answers 504 for every request), not a bug or a local
        // connectivity problem — explain that instead of an error screen.
        let mangas = self.gateway.popular_manga().unwrap_or_else(|e| {
            eprintln!("gideon: popular manga failed: {e:#}");
            Vec::new()
        });
        if mangas.is_empty() {
            return self.push(Screen::Message {
                title: "Popular manga".to_string(),
                body: "Couldn't load the popular list.\n\
                       MyAnimeList (which provides it) may be down —\n\
                       try again later. Search still works."
                    .into(),
            });
        }
        self.push(Screen::Popular { mangas, page: 0 })
    }

    /// Open global search from Home. With recent searches, land on the
    /// recents screen (tap one to reopen instantly, or start a new search);
    /// otherwise go straight to the keyboard.
    fn open_global_search(&mut self) -> Result<()> {
        if self.gateway.installed_sources()?.is_empty() {
            return self.push(Screen::Message {
                title: "Search".to_string(),
                body: "No sources installed yet.\nInstall one under Browse sources first."
                    .to_string(),
            });
        }
        if !self.recent_searches.is_empty() {
            let recents = self
                .recent_searches
                .iter()
                .map(|r| (r.query.clone(), r.results.len()))
                .collect();
            return self.push(Screen::RecentSearches { recents });
        }
        self.open_search_keyboard()
    }

    /// Push the global-search keyboard (empty query, every installed source).
    fn open_search_keyboard(&mut self) -> Result<()> {
        self.keyboard_paints = 0;
        self.keyboard_shift = false;
        self.push(Screen::Search {
            source: None,
            query: String::new(),
        })
    }

    /// Record (or refresh) a global search at the front of the recent list,
    /// deduped by query (case-insensitive, trimmed) and capped. Empty queries
    /// and empty result sets aren't worth remembering.
    fn remember_search(
        &mut self,
        query: &str,
        results: &[(SourceEntry, MangaEntry)],
        tried: &[String],
    ) {
        let key = query.trim().to_lowercase();
        if key.is_empty() || results.is_empty() {
            return;
        }
        self.recent_searches
            .retain(|r| r.query.trim().to_lowercase() != key);
        self.recent_searches.insert(
            0,
            RecentSearch {
                query: query.to_string(),
                results: results.to_vec(),
                tried: tried.to_vec(),
            },
        );
        self.recent_searches.truncate(RECENT_SEARCHES);
    }

    /// Reopen a cached recent search instantly (no network); if it has aged
    /// out of the cache, run it fresh.
    fn reopen_recent(&mut self, query: &str) -> Result<()> {
        let key = query.trim().to_lowercase();
        if let Some(recent) = self
            .recent_searches
            .iter()
            .find(|r| r.query.trim().to_lowercase() == key)
            .cloned()
        {
            return self.push(Screen::SearchResults {
                query: recent.query,
                results: recent.results,
                tried: recent.tried,
                page: 0,
            });
        }
        self.run_global_search(query)
    }

    fn run_search(&mut self, source: &Option<SourceEntry>, query: &str) -> Result<()> {
        match source {
            Some(source) => self.run_source_search(source, query),
            None => self.run_global_search(query),
        }
    }

    /// Search one source for `query`, retrying with its known title variants
    /// (from MyAnimeList) when the raw query finds nothing — a source often
    /// lists a manga under its romanised Japanese title while the user typed
    /// the English one, or the other way around. The variant list is looked
    /// up lazily (once per search, only after a miss) and cached in
    /// `variants`, so a query that hits everywhere never pays for the lookup.
    /// A variant that errors is skipped; only the raw query's error
    /// propagates.
    fn search_with_variants(
        &self,
        source_id: &str,
        query: &str,
        variants: &mut Option<Vec<String>>,
    ) -> Result<Vec<MangaEntry>> {
        let mangas = self.gateway.search_manga(source_id, query)?;
        if !mangas.is_empty() {
            return Ok(mangas);
        }
        let variants = variants.get_or_insert_with(|| self.gateway.title_variants(query));
        for variant in variants.iter() {
            if let Ok(mangas) = self.gateway.search_manga(source_id, variant) {
                if !mangas.is_empty() {
                    return Ok(mangas);
                }
            }
        }
        Ok(Vec::new())
    }

    /// Search one source; results open as a normal manga list.
    fn run_source_search(&mut self, source: &SourceEntry, query: &str) -> Result<()> {
        self.ensure_online()?;
        self.show_status(&[&format!("Searching for \"{query}\"…")])?;
        let mut variants = None;
        let mangas = self
            .search_with_variants(&source.id, query, &mut variants)
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
    /// errors is skipped (noted to stderr) — one broken source must not kill
    /// the search. The results screen always opens, even with no hits, so its
    /// "Search more sources" row can widen to uninstalled sources.
    fn run_global_search(&mut self, query: &str) -> Result<()> {
        self.ensure_online()?;
        let sources = self.gateway.installed_sources()?;
        let mut results: Vec<(SourceEntry, MangaEntry)> = Vec::new();
        let mut tried: Vec<String> = Vec::new();
        let mut variants = None;
        for (i, source) in sources.iter().enumerate() {
            // One status screen for the whole search, partially updated
            // per source — N full flashes made an N-source search strobe.
            self.show_status(&[
                &format!("Searching for \"{query}\"…"),
                &format!("{}/{}: {}…", i + 1, sources.len(), source.name),
            ])?;
            tried.push(source.id.clone());
            match self.search_with_variants(&source.id, query, &mut variants) {
                Ok(mangas) => {
                    results.extend(mangas.into_iter().map(|m| (source.clone(), m)));
                }
                Err(e) => {
                    eprintln!("gideon: search on {} failed: {e:#}", source.name);
                }
            }
        }
        self.remember_search(query, &results, &tried);
        self.push(Screen::SearchResults {
            query: query.to_string(),
            results,
            tried,
            page: 0,
        })
    }

    /// Widen the current global search to not-yet-installed sources: pull in
    /// up to [`crate::manga::WIDEN_BATCH`] candidates that haven't been tried yet,
    /// search each, keep the ones that matched (merging their hits) and
    /// uninstall the rest. Updates the results screen in place.
    fn widen_search(&mut self) -> Result<()> {
        let Some(Screen::SearchResults {
            query,
            results,
            tried,
            ..
        }) = self.stack.last()
        else {
            return Ok(());
        };
        let query = query.clone();
        let mut results = results.clone();
        let mut tried = tried.clone();

        self.ensure_online()?;
        let available = match self.gateway.available_sources() {
            Ok(available) => available,
            Err(e) => {
                return self.push(Screen::Message {
                    title: "Search more".to_string(),
                    body: format!("Couldn't load the source list:\n{e:#}"),
                });
            }
        };
        let candidates: Vec<SourceEntry> = available
            .into_iter()
            .filter(|s| !tried.iter().any(|id| id == &s.id))
            .take(crate::manga::WIDEN_BATCH)
            .collect();
        if candidates.is_empty() {
            return self.push(Screen::Message {
                title: "Search more".to_string(),
                body: "No more sources to try — every source in your lists has been searched."
                    .to_string(),
            });
        }

        // Sources the user already had: a widen must never remove one of
        // these, even if `tried` is stale (e.g. a reopened recent search, or
        // a source that failed to load). It only cleans up what it adds.
        let preinstalled: std::collections::HashSet<String> = self
            .gateway
            .installed_sources()
            .unwrap_or_default()
            .into_iter()
            .map(|s| s.id)
            .collect();
        let before = results.len();
        let mut failures = 0usize;
        let mut variants = None;
        for (i, source) in candidates.iter().enumerate() {
            self.show_status(&[
                &format!("Searching more for \"{query}\"…"),
                &format!("{}/{}: {}…", i + 1, candidates.len(), source.name),
            ])?;
            tried.push(source.id.clone());
            // Only discard a source this widen actually installed.
            let added_here = !preinstalled.contains(&source.id);
            // Installing or searching can fail (bad source, network); skip it
            // and make sure nothing half-installed is left behind.
            if self.gateway.install_source(&source.id).is_err() {
                failures += 1;
                if added_here {
                    let _ = self.gateway.uninstall_source(&source.id);
                }
                continue;
            }
            match self.search_with_variants(&source.id, &query, &mut variants) {
                Ok(mangas) if !mangas.is_empty() => {
                    // Had a hit — keep it installed and merge its results.
                    results.extend(mangas.into_iter().map(|m| (source.clone(), m)));
                }
                Ok(_) => {
                    if added_here {
                        let _ = self.gateway.uninstall_source(&source.id);
                    }
                }
                Err(_) => {
                    failures += 1;
                    if added_here {
                        let _ = self.gateway.uninstall_source(&source.id);
                    }
                }
            }
        }
        let found = results.len() - before;
        self.remember_search(&query, &results, &tried);

        // Fold the widened results back into the results screen.
        if let Some(screen @ Screen::SearchResults { .. }) = self.stack.last_mut() {
            *screen = Screen::SearchResults {
                query: query.clone(),
                results,
                tried,
                page: 0,
            };
        }
        if found == 0 {
            // Underlying screen is already updated; the message sits on top.
            // Distinguish "tried and missed" from "couldn't reach anything" so
            // a dropped Wi-Fi mid-widen doesn't read as a definitive no-result.
            let body = if failures == candidates.len() {
                "Couldn't reach the extra sources — check Wi-Fi and try again.".to_string()
            } else {
                format!("Tried {} more source(s); no new matches.", candidates.len())
            };
            return self.push(Screen::Message {
                title: "Search more".to_string(),
                body,
            });
        }
        self.render_current(RefreshMode::Full)
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
            sort: ChapterSort::default(),
        })
    }

    fn check_updates(&mut self) -> Result<()> {
        self.ensure_online()?;
        self.show_status(&["Checking for updates…"])?;
        let check = match self.gateway.check_updates() {
            Ok(check) => check,
            Err(e) => {
                // `ensure_online()` above already confirmed the connection, so a
                // failure here is the update server (GitHub) being unreachable —
                // not the user's Wi-Fi. Show that, and note the release may just
                // not be out yet, instead of the misleading "check that Wi-Fi is
                // on". The real error still goes to the log for diagnosis.
                eprintln!("gideon: update check failed: {e:#}");
                return self.push(Screen::Message {
                    title: "Updates".to_string(),
                    body: update_error_body(&e),
                });
            }
        };
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
        record_chapter_in_index(
            &self.library_dir,
            &self.index_guard,
            source,
            manga,
            &chapter.id,
            &cbz_path,
        );
        // Keep within the storage budget: evict the least-recently-read
        // downloads if this one pushed us over. The just-downloaded chapter is
        // newest, so it's never the one evicted.
        self.enforce_storage_limit();
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
            // Queue background pre-downloading of the next chapters *before* the
            // reader opens, so they fetch (at idle priority) while the user
            // reads this one. Queue-only — never blocks the first page.
            self.predownload_ahead(source, manga, chapters, &chapter.id);
            let key = progress_key(&self.library_dir, &cbz_path);
            let outcome = self.run_reader(&cbz_path, &key, next.is_some())?;
            // The cover fetch (a network round-trip) runs after the
            // session, never between the tap and the first page.
            if outcome != ReaderOutcome::Quit {
                self.fetch_cover_if_missing(manga, &cbz_path);
            }
            match outcome {
                ReaderOutcome::Quit => return Ok(Flow::Quit(Exit::Close)),
                // Leaving the chapter does NOT kick off more downloading — the
                // look-ahead window was already queued when the reader opened.
                // (Re-triggering here is what made it feel like it "kept
                // downloading every time you leave".)
                ReaderOutcome::Back => return Ok(Flow::Continue),
                // Turning past the last page flows into the next chapter; the
                // loop re-queues the window from the new chapter.
                ReaderOutcome::NextChapter => {
                    chapter = next.expect("NextChapter only with a next");
                }
            }
        }
    }

    /// The look-ahead window for a source/manga/chapter list, as
    /// [`lookahead_targets`] computes it. Test-facing wrapper.
    #[cfg(test)]
    fn predownload_targets(
        &self,
        source: &SourceEntry,
        manga: &MangaEntry,
        chapters: &[ChapterEntry],
        after_id: &str,
    ) -> Vec<ChapterEntry> {
        let count = self.load_settings().predownload_unread_chapters as usize;
        if count == 0 || chapters.is_empty() {
            return Vec::new();
        }
        let plan = LookaheadPlan {
            source: source.clone(),
            manga: manga.clone(),
            chapters: chapters.to_vec(),
            after_id: after_id.to_string(),
        };
        lookahead_targets(&plan, &self.library_dir, count)
    }

    /// Build the background pre-downloader on first use, if the gateway
    /// supports it. Returns whether one is available now.
    fn ensure_predownloader(&mut self) -> bool {
        if self.predownloader.is_some() {
            return true;
        }
        if let Some(gateway) = self.gateway.background_clone() {
            let storage_limit = self.load_settings().storage_size_limit.bytes();
            self.predownloader = Some(Predownloader::spawn(
                gateway,
                self.library_dir.clone(),
                Arc::clone(&self.index_guard),
                storage_limit,
            ));
            return true;
        }
        false
    }

    /// Stock up the next few chapters ahead of `after_id` so they're ready
    /// offline. This **only ever queues** onto the background worker and returns
    /// immediately — pre-download must never block the UI. If the gateway has no
    /// background worker (some tests), it's simply a no-op: pre-download is a
    /// nicety, never something the user waits on.
    fn predownload_ahead(
        &mut self,
        source: &SourceEntry,
        manga: &MangaEntry,
        chapters: &[ChapterEntry],
        after_id: &str,
    ) {
        // Remember the request even if there's no worker to run it: waking up
        // re-fires it (see [`Self::rekick_lookahead`]).
        self.lookahead = Some(LookaheadPlan {
            source: source.clone(),
            manga: manga.clone(),
            chapters: chapters.to_vec(),
            after_id: after_id.to_string(),
        });
        self.rekick_lookahead();
    }

    /// Fire (or re-fire) the stored look-ahead request.
    ///
    /// Called on wake as well as on every chapter open: a suspend takes the
    /// network down with it, so look-ahead jobs that ran while the radio was
    /// gone failed and left the next chapter un-stocked. Turning past the last
    /// page then had nothing to flow into, which is what forced the trip back
    /// out to the chapter list. Re-queueing is cheap — chapters already on disk
    /// are no-ops on the worker, and ones still queued are deduped.
    ///
    /// Takes the fields it needs one by one rather than going through `&self`
    /// helpers: the reader holds a mutable borrow of the display for its whole
    /// session, and the wake path inside it has to be able to call this.
    fn rekick_lookahead(&mut self) {
        Self::fire_lookahead(
            &self.lookahead,
            &mut self.predownloader,
            &self.gateway,
            &self.library_dir,
            &self.index_guard,
            self.settings_dir.as_deref(),
        );
    }

    /// The body of [`Self::rekick_lookahead`], over individual fields so the
    /// reader's wake path (which holds `self.display`) can call it too.
    fn fire_lookahead(
        lookahead: &Option<LookaheadPlan>,
        predownloader: &mut Option<Predownloader>,
        gateway: &G,
        library_dir: &Path,
        index_guard: &Arc<Mutex<()>>,
        settings_dir: Option<&Path>,
    ) {
        let Some(plan) = lookahead else {
            return; // nothing being read from a source
        };
        let settings = settings_in(settings_dir);
        let count = settings.predownload_unread_chapters as usize;
        if count == 0 || plan.chapters.is_empty() {
            return;
        }
        // Build the worker on first use; without one (some tests) the look-ahead
        // is simply skipped — never a foreground stall.
        if predownloader.is_none() {
            if let Some(gateway) = gateway.background_clone() {
                *predownloader = Some(Predownloader::spawn(
                    gateway,
                    library_dir.to_path_buf(),
                    Arc::clone(index_guard),
                    settings.storage_size_limit.bytes(),
                ));
            }
        }
        let Some(worker) = predownloader.as_mut() else {
            return;
        };
        let epoch = worker.epoch();
        for chapter in lookahead_targets(plan, library_dir, count) {
            worker.queue(PreloadJob {
                source: plan.source.clone(),
                manga: plan.manga.clone(),
                chapter_id: chapter.id,
                epoch,
                persistent: false,
            });
        }
    }

    /// Download a contiguous run of chapters — `chapters[start..start+count]`,
    /// skipping any already on disk — so they're ready offline. Returns how many
    /// were actually queued/fetched.
    ///
    /// The normal path queues them onto the background worker as **persistent**
    /// jobs: a deliberate "download these" must survive leaving the manga (unlike
    /// the auto look-ahead, which is abandoned when you move on). If the gateway
    /// has no background worker (some tests), it falls back to a foreground
    /// download so an explicit request never silently does nothing.
    fn queue_batch_download(
        &mut self,
        source: &SourceEntry,
        manga: &MangaEntry,
        chapters: &[ChapterEntry],
        start: usize,
        count: usize,
    ) -> Result<usize> {
        let end = (start + count).min(chapters.len());
        let targets: Vec<ChapterEntry> = chapters
            .get(start..end)
            .unwrap_or(&[])
            .iter()
            .filter(|c| self.downloaded_chapter_path(source, manga, &c.id).is_none())
            .cloned()
            .collect();
        if targets.is_empty() {
            return Ok(0);
        }
        // Bring Wi-Fi up if needed so the worker has a connection to fetch over.
        self.ensure_online()?;
        if self.ensure_predownloader() {
            let worker = self.predownloader.as_mut().expect("just ensured");
            let epoch = worker.epoch();
            for chapter in &targets {
                worker.queue(PreloadJob {
                    source: source.clone(),
                    manga: manga.clone(),
                    chapter_id: chapter.id.clone(),
                    epoch,
                    persistent: true,
                });
            }
        } else {
            // No background worker: download in the foreground with progress so
            // the explicit request still completes.
            for chapter in &targets {
                let cbz = self.download_to_library(source, manga, chapter)?;
                self.fetch_cover_if_missing(manga, &cbz);
            }
        }
        Ok(targets.len())
    }

    /// Delete a downloaded chapter's CBZ and drop it from the series index, so
    /// the storage budget frees up and the row stops showing as downloaded.
    /// Empty series directories are removed too.
    fn delete_download(&self, key: &str) -> Result<()> {
        let path = self.library_dir.join(key);
        std::fs::remove_file(&path)
            .with_context(|| format!("couldn't delete {}", path.display()))?;
        let series_dir = series_key_of(key);
        if let Some(file) = path.file_name() {
            let mut index = gideon_core::SeriesIndex::load(&self.library_dir);
            index.forget_download(series_dir, &file.to_string_lossy());
            let _ = index.save(&self.library_dir);
        }
        if let Some(parent) = path.parent() {
            if parent != self.library_dir
                && std::fs::read_dir(parent)
                    .map(|mut d| d.next().is_none())
                    .unwrap_or(false)
            {
                let _ = std::fs::remove_dir(parent);
            }
        }
        self.invalidate_progress_cache();
        Ok(())
    }

    /// After deleting a download, rebuild the underlying downloaded-chapters
    /// list from disk so the removed row disappears. No-op on other screens
    /// (the source chapter list recomputes its download marks on repaint).
    fn refresh_downloaded_in_place(&mut self) -> Result<()> {
        let (title, sort) = match self.stack.last() {
            Some(Screen::DownloadedChapters { title, sort, .. }) => (title.clone(), *sort),
            _ => return Ok(()),
        };
        let entries = self.downloaded_entries(&title);
        if entries.is_empty() {
            // Nothing left in the series — drop the now-empty list.
            self.pop()?;
        } else if let Some(screen @ Screen::DownloadedChapters { .. }) = self.stack.last_mut() {
            *screen = Screen::DownloadedChapters {
                title,
                entries,
                page: 0,
                sort,
            };
            self.render_current(RefreshMode::Full)?;
        }
        Ok(())
    }

    /// Downloaded-content usage: total bytes, chapter count and series count of
    /// the index-tracked downloads currently on disk. Side-loaded files aren't
    /// counted — the budget governs what gideon downloaded.
    fn storage_stats(&self) -> StorageStats {
        let _g = self.index_guard.lock().unwrap_or_else(|e| e.into_inner());
        let index = gideon_core::SeriesIndex::load(&self.library_dir);
        let mut stats = StorageStats::default();
        for (dir, series) in index.iter() {
            let mut any = false;
            for file in series.downloaded.values() {
                if let Ok(meta) = std::fs::metadata(self.library_dir.join(dir).join(file)) {
                    stats.used += meta.len();
                    stats.chapters += 1;
                    any = true;
                }
            }
            if any {
                stats.series += 1;
            }
        }
        stats
    }

    /// Evict least-recently-read downloads until within the configured storage
    /// limit. Returns bytes freed. The live counterpart to the (previously
    /// unused) size-budget engine — wired into the download paths so the
    /// "Storage limit" setting actually takes effect.
    fn enforce_storage_limit(&self) -> u64 {
        let settings = self.load_settings();
        // Finished-and-stale chapters go first. This is not the same thing as
        // the size budget below: eviction runs because the disk is full and
        // takes the least-recently-read chapter whether or not you finished
        // it, while this takes only chapters you are done with. Running it
        // first means a full disk reclaims what you no longer want before it
        // starts taking what you might.
        let reclaimed = self.reclaim_finished(settings.finished_cleanup_hours);
        reclaimed
            + evict_to_storage_limit(
                &self.library_dir,
                &self.index_guard,
                settings.storage_size_limit.bytes(),
            )
    }

    /// Delete chapters finished longer ago than `hours`, returning the bytes
    /// freed. `0` disables it.
    ///
    /// Best-effort by design: this is housekeeping, and a failure here must
    /// never surface as an error in front of someone who was reading. The
    /// engine's own refusals (never an unfinished chapter, never the one you
    /// have open, never a series' last chapter) are what make that safe.
    fn reclaim_finished(&self, hours: u32) -> u64 {
        if hours == 0 {
            return 0;
        }
        let _g = self.index_guard.lock().unwrap_or_else(|e| e.into_inner());
        let mut index = gideon_core::SeriesIndex::load(&self.library_dir);
        let store =
            gideon_core::ProgressStore::load(&progress_path(&self.library_dir)).unwrap_or_default();
        match gideon_sources::run_finished_cleanup(&self.library_dir, &store, &mut index, hours) {
            Ok(summary) => {
                if !summary.is_empty() {
                    eprintln!(
                        "gideon: cleaned up {} finished chapter(s), {} bytes",
                        summary.files(),
                        summary.bytes()
                    );
                }
                summary.bytes()
            }
            Err(e) => {
                eprintln!("gideon: finished-chapter cleanup failed: {e}");
                0
            }
        }
    }

    /// Open the storage-usage screen from Settings.
    fn open_storage(&mut self) -> Result<()> {
        self.push(Screen::Storage)
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
            // Past the last downloaded chapter, a series that came from a
            // source keeps going online — so the page turn is still live.
            let can_continue_online =
                next.is_none() && self.series_origin(&entry.relative_path).is_some();
            let next_available = next.is_some() || can_continue_online;
            match self.run_reader(&entry.path, &entry.relative_path, next_available)? {
                ReaderOutcome::Quit => return Ok(Flow::Quit(Exit::Close)),
                ReaderOutcome::Back => return Ok(Flow::Continue),
                ReaderOutcome::NextChapter => match next {
                    Some(next) => entry = next,
                    // Ran out of downloads: fetch the chapter list and carry on
                    // from the source instead of dead-ending here.
                    None => return self.continue_from_source(&entry),
                },
            }
        }
    }

    /// Read downloaded chapters starting at `entries[start]`, continuing to the
    /// next in the list when the reader turns past the last page. Fully local —
    /// the offline counterpart to [`Self::download_and_read`].
    fn read_downloaded_chain(&mut self, entries: &[LibraryEntry], start: usize) -> Result<Flow> {
        let mut i = start;
        loop {
            let entry = &entries[i];
            let more_on_disk = i + 1 < entries.len();
            // As on the shelf: the end of the downloads isn't the end of the
            // series when it came from a source.
            let can_continue_online =
                !more_on_disk && self.series_origin(&entry.relative_path).is_some();
            match self.run_reader(
                &entry.path,
                &entry.relative_path,
                more_on_disk || can_continue_online,
            )? {
                ReaderOutcome::Quit => return Ok(Flow::Quit(Exit::Close)),
                ReaderOutcome::Back => return Ok(Flow::Continue),
                ReaderOutcome::NextChapter if more_on_disk => i += 1,
                ReaderOutcome::NextChapter => {
                    let entry = entries[i].clone();
                    return self.continue_from_source(&entry);
                }
            }
        }
    }

    /// Where a downloaded chapter came from: its source, its manga, and the
    /// chapter id the file was downloaded under. `None` for sideloaded files and
    /// for series whose origin was never recorded.
    fn series_origin(&self, relative_path: &str) -> Option<(SourceEntry, MangaEntry, String)> {
        let series_dir = series_key_of(relative_path);
        let file = relative_path.rsplit('/').next()?;
        let index = gideon_core::SeriesIndex::load(&self.library_dir);
        let origin = index.get(series_dir)?;
        let chapter_id = origin
            .downloaded
            .iter()
            .find(|(_, name)| name.as_str() == file)
            .map(|(id, _)| id.clone())?;
        Some((
            SourceEntry {
                id: origin.source_id.clone(),
                name: origin.source_name.clone(),
            },
            MangaEntry {
                id: origin.manga_id.clone(),
                title: origin.manga_title.clone(),
                cover_url: origin.cover_url.clone(),
            },
            chapter_id,
        ))
    }

    /// Turned past the last page of the last **downloaded** chapter: go get the
    /// next one from the source and keep reading.
    ///
    /// This is the case the library shelf used to dead-end on. Reading a series
    /// from the shelf only ever chained through files already on disk, so when
    /// the look-ahead hadn't managed to stock the next chapter — typically after
    /// a sleep, with the radio down — the page turn did nothing and the only way
    /// forward was backing out to the chapter list and tapping the next chapter
    /// by hand. Now the turn itself fetches, and hands over to
    /// [`Self::download_and_read`], which resumes the normal look-ahead from
    /// there on.
    fn continue_from_source(&mut self, entry: &LibraryEntry) -> Result<Flow> {
        let Some((source, manga, chapter_id)) = self.series_origin(&entry.relative_path) else {
            return Ok(Flow::Continue); // sideloaded: nothing to continue from
        };
        self.show_status(&["Looking for the next chapter…"])?;
        // Bring the radio up if the nap took it down; a failure here still falls
        // through to the fetch, which surfaces the offline message.
        self.ensure_online()?;
        let chapters = match self.gateway.chapters(&source.id, &manga.id) {
            Ok(chapters) if !chapters.is_empty() => chapters,
            Ok(_) | Err(_) => {
                return self
                    .push(Screen::Message {
                        title: "Next chapter".to_string(),
                        body: "Couldn't reach the source to fetch the next chapter.\n\
                               Check Wi-Fi and try again — everything downloaded\n\
                               is still readable from the library."
                            .to_string(),
                    })
                    .map(|_| Flow::Continue);
            }
        };
        let Some(next) = next_chapter(&chapters, &chapter_id) else {
            return self
                .push(Screen::Message {
                    title: "Next chapter".to_string(),
                    body: format!("You're up to date with {}.", manga.title),
                })
                .map(|_| Flow::Continue);
        };
        self.download_and_read(&source, &manga, &next, &chapters)
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

        // Record this chapter as the series' last-opened the moment it opens —
        // BEFORE the first page even paints — so "resume" always lands here,
        // even if the app is killed (Nickel home button) before any later save.
        store.set_last_opened(series_key_of(key), key);
        // merge_save (not save): a background sync may be writing this file too;
        // furthest-page-wins folds in rather than clobbering, and never rewinds.
        let _ = store.merge_save(&progress_file);

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
            reader.set_auto_rotate_spreads(self.auto_rotate_spreads);
            reader.resume_from(&store, key);
            // Warm the render-ahead at the resume page before the first
            // paint: the decode + scale + dither run on the prefetch
            // thread, and the first paint just takes the finished render.
            reader.warm();
            reader.show_current_page()?;
            loop {
                // Same idle auto-suspend as the menu loop: a reader left
                // open (user fell asleep, device without a sleep cover)
                // suspends after 15 idle minutes instead of burning the
                // battery all night. Synthesizes a Sleep event so the arm
                // below saves progress exactly like a cover close. The
                // idle clock is wall-clock time and starts fresh each time
                // we wait for input (see the menu loop): handling an event
                // can block for minutes (a chapter download at the end of
                // a page), and that time is the user's activity, not idle.
                let event = if self.sleeper.is_some() {
                    let idle_since = std::time::Instant::now();
                    loop {
                        match self.input.poll_event(IDLE_SUSPEND_TICK) {
                            Ok(Some(event)) => break Ok(event),
                            Ok(None) => {
                                if idle_since.elapsed() >= self.idle_suspend {
                                    break Ok(UiEvent::Sleep);
                                }
                            }
                            Err(e) => break Err(e),
                        }
                    }
                } else {
                    self.input.next_event()
                };
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
                                        self.auto_rotate_spreads,
                                    )?;
                                }
                                Some(SHEET_ROW_AUTO_SPREAD) => {
                                    // Toggle auto-rotate spreads live: repaint the
                                    // page (it may rotate now) and keep the sheet
                                    // up with the flipped label.
                                    self.auto_rotate_spreads = !self.auto_rotate_spreads;
                                    let on = self.auto_rotate_spreads;
                                    reader.set_auto_rotate_spreads(on);
                                    persist_settings(self.settings_dir.as_deref(), |s| {
                                        s.auto_rotate_spreads = on;
                                    });
                                    reader.show_current_page()?;
                                    show_controls_sheet(
                                        &mut reader,
                                        &panel,
                                        rotation,
                                        rotation_locked,
                                        on,
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
                // Page-level navigation, rotation and mid-screen gestures leave
                // panel zoom first, then run normally against the full page.
                // Frame-stepping (taps) and re-targeting (long-press) are
                // handled inside their own arms below.
                if reader.panel_zoom_active()
                    && matches!(
                        &event,
                        Ok(UiEvent::PageForward)
                            | Ok(UiEvent::PageBack)
                            | Ok(UiEvent::RemoteNext)
                            | Ok(UiEvent::RemotePrev)
                            | Ok(UiEvent::Rotate { .. })
                            | Ok(UiEvent::Swipe { .. })
                    )
                {
                    reader.exit_panel_zoom()?;
                }
                match event {
                    Err(_) => {
                        outcome = ReaderOutcome::Quit;
                        break;
                    }
                    // Tap zones follow the reading orientation, not the panel.
                    // While zoomed, the same zones step frames instead of pages
                    // (right → next frame, left → previous, centre → exit zoom).
                    Ok(UiEvent::Tap { x, y }) if reader.panel_zoom_active() => {
                        match panel.reader_zone_rotated(x, y, rotation) {
                            ReaderZone::NextPage => {
                                reader.next_panel()?;
                            }
                            ReaderZone::PrevPage => {
                                reader.prev_panel()?;
                            }
                            ReaderZone::Back => {
                                reader.exit_panel_zoom()?;
                            }
                        }
                    }
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
                    //
                    // A Bluetooth remote is a separate object in your hand — it
                    // does NOT rotate with the device — so its direction is
                    // absolute: next is always next, at every orientation.
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
                    Ok(ev @ (UiEvent::RemoteNext | UiEvent::RemotePrev)) => {
                        if matches!(ev, UiEvent::RemoteNext) {
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
                    // Every reader gesture follows the READING orientation, the
                    // same as taps: map both swipe endpoints into the reading
                    // frame FIRST, then decide edges and direction there. The
                    // reader's right edge is brightness and its left edge is
                    // night-light warmth at every rotation, and "up" always
                    // increases — otherwise, in panel space, the controls land
                    // on the wrong edge and invert when the device is turned.
                    Ok(UiEvent::Swipe { x0, y0, x1, y1 }) => {
                        let (rx0, ry0) =
                            layout::map_reader_tap(x0, y0, panel.width, panel.height, rotation);
                        let (rx1, ry1) =
                            layout::map_reader_tap(x1, y1, panel.width, panel.height, rotation);
                        let (reading_w, reading_h) = if rotation % 180 == 90 {
                            (panel.height, panel.width)
                        } else {
                            (panel.width, panel.height)
                        };
                        let edge = (reading_w / 8).max(1);
                        let on_right = rx0 >= reading_w - edge && rx1 >= reading_w - edge;
                        let on_left = rx0 < edge && rx1 < edge;
                        if !on_right && !on_left {
                            // Mid-screen gestures: swipe down to leave the manga,
                            // swipe up to rotate 90° clockwise — for reading on
                            // your side in bed. Both demand deliberate travel (a
                            // quarter of the reading height): a sloppy page-turn
                            // tap drifting past the 30px slop must never exit,
                            // and certainly never rotate the whole reader.
                            let min_travel = (reading_h / 4).max(1);
                            let vertical = ry0.abs_diff(ry1) > rx0.abs_diff(rx1);
                            // An up-swipe STARTING in the bottom eighth of the
                            // reading frame opens the controls sheet — distinct
                            // from the mid-screen rotate gesture below, which
                            // starts higher up. An eighth of travel is enough:
                            // it's a flick off the bezel.
                            let sheet_band = reading_h.saturating_sub((reading_h / 8).max(1));
                            if ry0 > ry1
                                && vertical
                                && ry0 > sheet_band
                                && ry0 - ry1 >= (reading_h / 8).max(1)
                            {
                                sheet_open = true;
                                show_controls_sheet(
                                    &mut reader,
                                    &panel,
                                    rotation,
                                    rotation_locked,
                                    self.auto_rotate_spreads,
                                )?;
                                continue;
                            }
                            if ry1 > ry0 && vertical && ry1 - ry0 >= min_travel {
                                break;
                            }
                            if ry0 > ry1 && vertical && ry0 - ry1 >= min_travel {
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
                        // Sliding up (in the reading frame) increases; the full
                        // reading height is the full 0–100 range.
                        let delta =
                            ((ry0 as i64 - ry1 as i64) * 100 / reading_h.max(1) as i64) as i32;
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
                    // Press-and-hold zooms into the comic frame under the finger
                    // (KOReader's panel zoom); holding again leaves zoom. While
                    // zoomed, taps step frames and a centre tap also exits.
                    Ok(UiEvent::LongPress { x, y }) => {
                        if reader.panel_zoom_active() {
                            reader.exit_panel_zoom()?;
                        } else {
                            let (rx, ry) =
                                layout::map_reader_tap(x, y, panel.width, panel.height, rotation);
                            if !reader.enter_panel_zoom(rx, ry)? {
                                // No frames on this page (full-bleed art /
                                // splash): say so rather than zoom into nothing.
                                reader.show_banner("No panels detected")?;
                            }
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
                        store.merge_save(&progress_file)?;
                        // …and get it off the device while there's still a
                        // network to send it over. This is the common case for
                        // "the web never updated": you finish a chapter, put
                        // the Kobo down, and it idles into suspend without ever
                        // leaving the reader. Bounded, and skipped when offline.
                        crate::sync::sync_before_sleep(
                            &self.library_dir,
                            self.gateway.background_clone(),
                        );
                        let mut result = self.sleeper.as_mut().expect("checked above")();
                        self.last_wake = Some(std::time::Instant::now());
                        if matches!(result, Ok(SleepResult::Skipped)) {
                            let Some(charger) = self.charger.as_ref() else {
                                continue; // still awake, screen untouched
                            };
                            // Wait out the charger and finish the nap once
                            // unplugged — same as the menu path, but with a
                            // reader banner instead of a status screen.
                            reader.show_banner("Plugged in — will sleep once unplugged")?;
                            match sleep_once_unplugged(
                                &mut self.input,
                                charger,
                                self.sleeper.as_mut().expect("checked above"),
                            )? {
                                UnplugWait::Aborted => {
                                    reader.repaint_full()?;
                                    continue;
                                }
                                UnplugWait::Slept(slept) => {
                                    self.last_wake = Some(std::time::Instant::now());
                                    result = slept;
                                }
                            }
                            if matches!(result, Ok(SleepResult::Skipped)) {
                                reader.repaint_full()?;
                                continue;
                            }
                        }
                        if let Err(e) = &result {
                            eprintln!("gideon: suspend failed: {e:#}");
                        }
                        // Drop the wake key press FIRST, then reopen the
                        // possibly re-registered input nodes — reopening hands
                        // us fresh fds and can take ~3s, so draining after it
                        // would eat a press the user makes post-wake (e.g. the
                        // button that advances the last page into the next
                        // chapter, which "sometimes" failed after sleep).
                        self.input.discard_queued();
                        self.input.refresh_devices();
                        // Snap the reader to the device's current orientation on
                        // wake (auto mode only): the gsensor reports only on
                        // *change*, so otherwise it stays at the pre-sleep
                        // orientation until the device is physically moved — the
                        // "screen won't rotate after sleep" bug. Field accesses
                        // only here (reader still borrows self.display).
                        if !rotation_locked {
                            if let Some(UiEvent::Rotate { rotation: target }) =
                                self.input.resync_orientation()
                            {
                                let target = target % 360;
                                if target != rotation {
                                    reader.set_rotation(target);
                                    rotation = target;
                                    self.reader_rotation = target;
                                }
                            }
                        }
                        // Proactively rejoin Wi-Fi after the suspend (detached,
                        // non-blocking; no-op if still connected) so a download
                        // at the end of the chapter just works — unless the
                        // user turned auto-connect off. A FAILED suspend also
                        // took the radio down before dying, so restore it even
                        // then: the user turned off auto-connect, not the radio.
                        // Bluetooth (if it was on) also needs the shared
                        // radio, so a pending BT resume forces the bring-up.
                        if self.wifi_auto_connect
                            || result.is_err()
                            || gideon_device::bluetooth::resume_pending()
                        {
                            gideon_device::network::reconnect_after_wake();
                        }
                        // Restore Bluetooth and re-connect the page-turn
                        // remote (detached, no-op unless suspend took it down).
                        gideon_device::bluetooth::reconnect_after_wake();
                        // Re-stock the chapters ahead. Waking up mid-chapter is
                        // exactly when the next one is likeliest to be missing
                        // (queued while the radio was down), and it's needed a
                        // few page turns from now — so ask again here rather
                        // than discovering the gap at the last page. (Field-wise,
                        // not through `&mut self`: the reader holds the display.)
                        Self::fire_lookahead(
                            &self.lookahead,
                            &mut self.predownloader,
                            &self.gateway,
                            &self.library_dir,
                            &self.index_guard,
                            self.settings_dir.as_deref(),
                        );
                        // Anything the pre-sleep flush couldn't send goes out
                        // once the radio is genuinely back (not now — it isn't).
                        crate::sync::spawn_sync_when_online(
                            &self.library_dir,
                            self.gateway.background_clone(),
                        );
                        if let Some(lights) = self.lights.as_mut() {
                            lights.reapply();
                        }
                        reader.repaint_full()?;
                    }
                }
            }
            reader.save_progress(&mut store, key);
        }
        store.merge_save(&progress_file)?;
        // The shelf's cached store is stale now — the session moved pages.
        self.invalidate_progress_cache();
        // Reading advanced this chapter's page: push it (and pull anything new)
        // in the background so another device picks up where we stopped.
        self.trigger_sync();
        // The session may have rotated the reading orientation: the menus
        // follow it, so rebuild the layout before repainting them.
        self.rebuild_layout();

        if outcome == ReaderOutcome::Back {
            // Repaint the screen the reader covered. (NextChapter goes
            // straight into the next reader session — no repaint between.)
            self.render_current(RefreshMode::Full)?;
            // The gesture that left the reader — especially a swipe-down —
            // often trails a stray touch (the finger settling/lifting) that the
            // panel reports as a separate tap. Without this it would land on the
            // library underneath and open a book at random. Drain after the full
            // repaint, by which point the tail has queued. (Menus ignore
            // swipes, but not taps.)
            self.input.discard_queued();
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
    /// Open the Wi-Fi screen: scan for nearby networks (the radio is brought
    /// up first if it's off, so a scan always has something to find) and show
    /// the list.
    fn open_wifi(&mut self) -> Result<()> {
        self.show_status(&["Scanning for Wi-Fi…"])?;
        if !gideon_device::network::is_online() {
            gideon_device::network::bring_up_wifi();
        }
        let networks = scan_wifi_sorted();
        self.push(Screen::WifiList { networks })
    }

    /// Tapping a network: already connected → nothing; a saved or open network
    /// connects directly; a new secured one asks for a password first.
    fn tap_wifi_network(&mut self, net: &gideon_device::network::WifiNetwork) -> Result<()> {
        if net.connected {
            return Ok(());
        }
        if net.saved || !net.secured {
            self.connect_to_network(&net.ssid, None)
        } else {
            self.keyboard_paints = 0;
            self.keyboard_shift = false;
            self.push(Screen::WifiPassword {
                ssid: net.ssid.clone(),
                password: String::new(),
            })
        }
    }

    /// Keyboard tap on the password screen: edit the password in place, or
    /// (action key) connect with it.
    fn tap_wifi_password(&mut self, ssid: &str, password: &str, x: u32, y: u32) -> Result<()> {
        let key = self.layout.key_at(x, y);
        if key == Some(Key::Search) {
            return self.connect_to_network(ssid, Some(password));
        }
        if self.toggle_shift_if_pressed(key)? {
            return Ok(());
        }
        if let Some(p) = key.and_then(|key| apply_key_edit(password, key, self.keyboard_shift)) {
            if let Some(Screen::WifiPassword { password, .. }) = self.stack.last_mut() {
                *password = p;
            }
            self.keyboard_repaint()?;
        }
        Ok(())
    }

    /// Connect to `ssid` (`password = None` for open/saved), waiting for an
    /// address with a cancellable heartbeat, then refresh the Wi-Fi list in
    /// place (dropping the password keyboard if we came from it).
    fn connect_to_network(&mut self, ssid: &str, password: Option<&str>) -> Result<()> {
        self.last_wifi_fail = None;
        gideon_device::network::connect_network(ssid, password);
        let start = std::time::Instant::now();
        let mut last_kick = start;
        let mut online = gideon_device::network::is_online();
        let mut sleep_requested = false;
        while !online && start.elapsed() < WIFI_CONNECT_TIMEOUT {
            self.show_status(&[
                &format!("Connecting to {ssid}…"),
                &format!("({}s) · tap to cancel", start.elapsed().as_secs()),
            ])?;
            // A cover close is not a cancel — sleep after leaving the loop.
            match self.input.poll_event(std::time::Duration::from_secs(1))? {
                Some(UiEvent::Sleep) => {
                    sleep_requested = true;
                    break;
                }
                Some(_) => break,
                None => {}
            }
            online = gideon_device::network::is_online();
            // Re-issue the connect if it hasn't taken yet (the first associate
            // can miss right after the radio comes up).
            if !online && last_kick.elapsed() >= WIFI_REKICK_INTERVAL {
                gideon_device::network::connect_network(ssid, password);
                last_kick = std::time::Instant::now();
            }
        }
        if matches!(self.stack.last(), Some(Screen::WifiPassword { .. })) {
            self.stack.pop();
        }
        if sleep_requested {
            self.sleep_now()?;
        }
        self.refresh_wifi_list()
    }

    /// Re-scan and replace the Wi-Fi list in place (or push one if we're not
    /// already on it), then repaint.
    fn refresh_wifi_list(&mut self) -> Result<()> {
        self.show_status(&["Scanning for Wi-Fi…"])?;
        // If we're offline, bring the radio up / reassociate as part of the
        // scan, so "Scan again" doubles as a reconnect.
        if !gideon_device::network::is_online() {
            gideon_device::network::bring_up_wifi();
        }
        let networks = scan_wifi_sorted();
        match self.stack.last_mut() {
            Some(s @ Screen::WifiList { .. }) => *s = Screen::WifiList { networks },
            _ => self.stack.push(Screen::WifiList { networks }),
        }
        self.render_current(RefreshMode::Full)
    }

    /// Manual reconnect from Home's offline row: an explicit user request, so
    /// ignore the failure-backoff and force a scan + connect (with the same
    /// tap-to-cancel status as the automatic path), then repaint Home — the
    /// reconnect row disappears if we're back online.
    fn reconnect_wifi(&mut self) -> Result<()> {
        self.last_wifi_fail = None;
        self.connect_wifi()?;
        self.render_current(RefreshMode::Full)
    }

    /// Automatic connectivity check before a network action: respects the
    /// `wifi_auto_connect` preference (off = never auto-connect; the user
    /// connects manually from the Wi-Fi controls).
    fn ensure_online(&mut self) -> Result<()> {
        if !self.wifi_auto_connect {
            return Ok(());
        }
        self.connect_wifi()
    }

    /// Bring Wi-Fi up and wait for an address (cancellable), if offline. The
    /// shared core of the automatic and manual paths; does NOT consult
    /// `wifi_auto_connect` (a manual reconnect must work even with auto off).
    fn connect_wifi(&mut self) -> Result<()> {
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
        // Wait for association with a live per-second heartbeat (a motionless
        // "Connecting…" reads as a crash) — but the screen is NOT locked: each
        // tick we poll input for up to a second, and ANY tap/button/cover
        // cancels the wait instead of holding the whole UI hostage. The radio
        // needs a moment to come up after sleep, so we keep scanning until the
        // timeout or the user gives up.
        let start = std::time::Instant::now();
        let mut last_kick = start;
        let mut online = gideon_device::network::is_online();
        let mut cancelled = false;
        let mut sleep_requested = false;
        while !online && start.elapsed() < WIFI_CONNECT_TIMEOUT {
            self.show_status(&[
                "Connecting to Wi-Fi…",
                &format!("({}s) · tap to cancel", start.elapsed().as_secs()),
            ])?;
            // Poll input for ~1s rather than sleeping blind: a deliberate
            // press means "stop waiting, I'll deal with it". A cover close
            // is NOT a cancel to swallow — the device must still sleep
            // (handled after the loop; every other drain in the app is
            // equally careful to preserve Sleep).
            match self.input.poll_event(std::time::Duration::from_secs(1))? {
                Some(UiEvent::Sleep) => {
                    sleep_requested = true;
                    cancelled = true;
                    break;
                }
                Some(_) => {
                    cancelled = true;
                    break;
                }
                None => {}
            }
            online = gideon_device::network::is_online();
            // Keep nudging the chip to re-scan/re-associate instead of waiting
            // passively for a first attempt that may have missed.
            if !online && last_kick.elapsed() >= WIFI_REKICK_INTERVAL {
                gideon_device::network::bring_up_wifi();
                last_kick = std::time::Instant::now();
            }
        }
        // Back off after a failure OR a cancel, so the next tap doesn't
        // immediately re-enter a long wait the user just dismissed.
        self.last_wifi_fail = (!online).then(std::time::Instant::now);
        if sleep_requested {
            // The cover closed mid-connect: honor it now instead of
            // silently staying awake in a bag.
            return self.sleep_now();
        }
        if cancelled {
            self.show_status(&["Wi-Fi cancelled."])?;
        }
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
        // Refresh Home's offline state once per paint (one is_online probe),
        // so the "reconnect" row and the tap dispatch agree on the offset.
        if matches!(self.stack.last(), Some(Screen::Home)) {
            self.home_offline = !self.is_online();
        }
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
        // The stats screen is composed in RGB unconditionally: the heatmap is
        // the one widget whose whole job is a colour ramp, and on a panel
        // without a colour filter the ramp collapses to its greys anyway.
        if matches!(self.stack.last(), Some(Screen::Stats)) {
            return Ok(Some(self.compose_stats()));
        }
        if matches!(self.stack.last(), Some(Screen::Home)) {
            let rows = HOME_ROWS.len() + usize::from(self.home_offline);
            let mut canvas = RgbPage::from_gray(&self.compose_current()?);
            // No band (nothing read yet, or no room) means the plain menu is
            // all there is — fall back to the grayscale path so Home does not
            // pay for a colour refresh it makes no use of.
            if self.compose_home_band(&mut canvas, rows).is_none() {
                return Ok(None);
            }
            return Ok(Some(canvas));
        }
        let Some(Screen::Library { items, page }) = self.stack.last() else {
            return Ok(None);
        };
        if self.load_settings().library_view == "list" {
            return Ok(Some(self.compose_library_list(items, *page)));
        }
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

    /// The reading-stats screen: four totals across the top, then the
    /// activity heatmap.
    ///
    /// Composed in RGB because the heatmap is a colour ramp; on a panel with
    /// no colour filter the ramp collapses to its own greys, which is why
    /// every ramp is monotonic in luma (`gideon_render::heatmap`).
    ///
    /// The stats are derived from THIS profile's progress store, so switching
    /// profiles shows that reader's numbers rather than the device's.
    fn compose_stats(&self) -> RgbPage {
        let l = &self.layout;
        let stats = self.with_progress(|_, store| gideon_core::ReadingStats::from_store(store));
        let palette = heatmap::Palette::from_setting(&self.load_settings().color_profile);

        let mut canvas = RgbPage::from_gray(&compose_chrome(l, "Reading stats", 0, 1));
        let mut gray = GrayPage::new_white(l.width, l.height);

        // Four totals, two per row, so the labels have room at any panel width
        // rather than being ellipsised into uselessness on the narrow ones.
        let tiles = [
            (
                stats.current_streak.to_string(),
                format!("day streak - best {}", stats.longest_streak),
            ),
            (
                stats.chapters_finished.to_string(),
                format!("chapters - {} series", stats.series_count),
            ),
            (stats.pages_read.to_string(), "pages read".to_string()),
            (stats.active_days.to_string(), "days read".to_string()),
        ];
        let col_w = (l.width - l.pad * 2) / 2;
        let tile_h = l.row_h;
        let mut y = l.content_top() + l.pad;
        for (i, (value, label)) in tiles.iter().enumerate() {
            let x = l.pad + (i as u32 % 2) * col_w;
            if i % 2 == 0 && i > 0 {
                y += tile_h;
            }
            draw_text(&mut gray, x, y, l.text_px * 1.15, value, col_w, true);
            draw_text(
                &mut gray,
                x,
                y + (l.text_px * 1.3) as u32,
                l.text_px * 0.62,
                label,
                col_w,
                false,
            );
        }
        y += tile_h + l.pad;

        draw_text(
            &mut gray,
            l.pad,
            y,
            l.text_px * 0.62,
            "READING ACTIVITY",
            l.width - l.pad * 2,
            true,
        );
        y += (l.text_px * 0.95) as u32;

        copy_gray_into_rgb(&mut canvas, &gray);

        // Size the grid to the panel instead of hardcoding a cell, so this is
        // right on a 1072-wide Clara as well as a 1264-wide Libra Colour.
        let weeks = STATS_HEATMAP_WEEKS;
        let layout = heatmap::HeatmapLayout::fit(l.pad, y, weeks, l.width - l.pad * 2, 6);
        heatmap::draw_heatmap(
            &mut canvas,
            &layout,
            &stats.heatmap(weeks as usize),
            &palette,
        );
        canvas
    }

    /// Home's data band: the reading totals and the activity heatmap, drawn
    /// in the space below the menu rows.
    ///
    /// The menu keeps every row index it has always had — the band lives
    /// strictly *below* the last row, so taps, row geometry and the tests
    /// that depend on them are untouched. On a 1264x1680 Libra Colour the
    /// seven rows fill 816px of 1680, so this is otherwise dead panel.
    ///
    /// Returns `None` when there is nothing to say (no reading recorded yet)
    /// or no room to say it, so a fresh device shows the plain menu rather
    /// than a band of zeroes.
    fn compose_home_band(&self, canvas: &mut RgbPage, first_free_row: usize) -> Option<()> {
        let l = &self.layout;
        let top = l.row_top(first_free_row) + l.pad;
        let bottom = l.height.saturating_sub(l.nav_h);
        if bottom.saturating_sub(top) < l.row_h * 2 {
            return None;
        }
        let stats = self.with_progress(|_, store| gideon_core::ReadingStats::from_store(store));
        if stats.chapters_tracked == 0 {
            return None;
        }
        let theme = widgets::Theme::from_setting(&self.load_settings().color_profile);
        let inner = l.width.saturating_sub(l.pad * 2);

        // A separator sets the band apart from the tappable rows above it, so
        // nothing here reads as another menu entry.
        fill_rect_rgb(canvas, l.pad, top, inner, 1, [0x55, 0x55, 0x55]);

        let tiles_y = top + l.pad;
        let tile_h = l.row_h;
        widgets::draw_stat_tiles(
            canvas,
            l.pad,
            tiles_y,
            inner,
            tile_h,
            &[
                widgets::StatTile {
                    value: &stats.current_streak.to_string(),
                    label: "day streak",
                    sub: &format!("best {}", stats.longest_streak),
                },
                widgets::StatTile {
                    value: &stats.chapters_finished.to_string(),
                    label: "chapters",
                    sub: &format!("{} series", stats.series_count),
                },
                widgets::StatTile {
                    value: &stats.pages_read.to_string(),
                    label: "pages",
                    sub: &format!("{} days", stats.active_days),
                },
            ],
            l.text_px,
            &theme,
        );

        // Fit the heatmap to whatever is left rather than a fixed cell size,
        // so this is right on a 1072-wide Clara as well as a Libra Colour.
        let grid_y = tiles_y + tile_h + l.pad;
        if bottom.saturating_sub(grid_y) < l.row_h {
            return Some(());
        }
        let weeks = STATS_HEATMAP_WEEKS;
        let layout = heatmap::HeatmapLayout::fit(l.pad, grid_y, weeks, inner, 6);
        if grid_y + layout.height() <= bottom {
            heatmap::draw_heatmap(
                canvas,
                &layout,
                &stats.heatmap(weeks as usize),
                &heatmap::Palette::from_setting(&self.load_settings().color_profile),
            );
        }
        Some(())
    }

    /// How many dense library rows fit on a page. Each row carries a cover
    /// thumbnail, the title, the MyAnimeList line and the progress column,
    /// so it needs about twice a menu row.
    fn list_row_height(&self) -> u32 {
        self.layout.row_h * 2
    }

    fn list_rows_per_page(&self) -> usize {
        ((self.layout.content_height() / self.list_row_height()).max(1)) as usize
    }

    /// The Library as a dense list: one row per series carrying everything
    /// the device knows about it — score, publication status, genres, how
    /// much is downloaded, how far in you are and what is next.
    ///
    /// This is the view the metadata exists for. The cover shelf stays the
    /// default for people who browse by art; `library_view` picks between
    /// them and the title bar toggles it.
    fn compose_library_list(&self, items: &[SeriesCard], page: usize) -> RgbPage {
        let l = &self.layout;
        let per_page = self.list_rows_per_page();
        let page_count = items.len().div_ceil(per_page).max(1);
        let theme = widgets::Theme::from_setting(&self.load_settings().color_profile);
        let index = gideon_core::SeriesIndex::load(&self.library_dir);

        let mut canvas = RgbPage::from_gray(&compose_chrome(l, "Library", page, page_count));
        let row_h = self.list_row_height();

        self.with_progress(|app, store| {
            for (i, card) in items
                .iter()
                .skip(page * per_page)
                .take(per_page)
                .enumerate()
            {
                let y = l.content_top() + i as u32 * row_h;
                let meta = card
                    .series
                    .as_deref()
                    .and_then(|dir| index.get(dir))
                    .and_then(|r| r.meta.as_ref());

                let finished = card
                    .chapters
                    .iter()
                    .filter(|c| store.get(&c.relative_path).is_some_and(|p| p.is_finished()))
                    .count();
                // Total prefers MyAnimeList's chapter count — what the series
                // actually has — and falls back to what is on disk, so an
                // un-looked-up series still reads honestly instead of
                // claiming to be complete.
                let total = meta
                    .and_then(|m| m.total_chapters)
                    .map(|t| t as usize)
                    .unwrap_or(card.chapters.len())
                    .max(finished);
                let downloaded = card.chapters.len();
                // "18 downloaded" alone does not answer the question you
                // actually have in front of a library — how much of what is
                // on the device is still waiting for you. Unread is the
                // number that decides whether to open this series or another.
                let unread = card
                    .chapters
                    .iter()
                    .filter(|c| !store.get(&c.relative_path).is_some_and(|p| p.is_finished()))
                    .count();
                let genres = meta.map(|m| m.genres.join(" · ")).unwrap_or_default();
                let when = card.latest_read_at(store);

                let row = widgets::LibraryRow {
                    title: &card.title(),
                    score: meta.and_then(|m| m.score),
                    status: meta.and_then(|m| m.status.as_deref()),
                    genres: &genres,
                    downloads: &if unread == 0 {
                        format!("{downloaded} downloaded · all read")
                    } else {
                        format!("{downloaded} downloaded · {unread} unread")
                    },
                    when: &app.ago(when),
                    read: &format!("{finished} / {total}"),
                    pct: if total == 0 {
                        0.0
                    } else {
                        finished as f32 / total as f32
                    },
                    // Three distinct states, which an Option chain quietly
                    // collapsed into one: nothing read yet, a next chapter
                    // waiting on disk, and read as far as what is downloaded.
                    // The last is the common case mid-series and must not
                    // read as "start reading".
                    next: &match card.furthest_read(store) {
                        None => "start reading".to_string(),
                        Some(current) => match card.next_after(current) {
                            // Just the chapter, not "Series — Chapter":
                            // the row is already titled with the series, and
                            // repeating it crowds out the part that matters.
                            Some(next) => format!("next: {}", chapter_label(&next.relative_path)),
                            None if finished >= total => "finished".to_string(),
                            None => "caught up".to_string(),
                        },
                    },
                    finished: finished > 0 && finished >= total,
                };
                // Cover art first, then hand the row widget the space that
                // is left. Decoding and caching stay here so the widget
                // remains a pure drawing routine.
                let inset = l.pad;
                let cover_h = row_h.saturating_sub(inset * 2);
                let cover_w = cover_h * 2 / 3;
                let thumb = app.shelf_cover(card.cover_entry(), (cover_w, cover_h), per_page);
                blit_thumb(&mut canvas, &thumb, inset, y + inset, cover_w, cover_h);

                let text_x = inset + cover_w + inset;
                widgets::draw_library_row(
                    &mut canvas,
                    text_x,
                    y,
                    l.width.saturating_sub(text_x),
                    row_h,
                    &row,
                    l.text_px,
                    &theme,
                );
                // A hairline between rows, not around them: the list should
                // read as one continuous column, not a stack of cards.
                if i + 1 < per_page {
                    fill_rect_rgb(
                        &mut canvas,
                        inset,
                        y + row_h - 1,
                        l.width.saturating_sub(inset * 2),
                        1,
                        [0xDD, 0xDD, 0xDD],
                    );
                }
            }
        });
        canvas
    }

    /// "3 days ago" for a unix timestamp, or an empty string when nothing
    /// has been read — the row then simply omits the age rather than
    /// claiming the epoch.
    fn ago(&self, at: u64) -> String {
        if at == 0 {
            return String::new();
        }
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(at);
        let secs = now.saturating_sub(at);
        match secs {
            s if s < 3600 => "just now".to_string(),
            s if s < 86_400 => format!("{}h ago", s / 3600),
            s if s < 2_592_000 => format!("{}d ago", s / 86_400),
            s => format!("{}mo ago", s / 2_592_000),
        }
    }

    /// Swap the Library between the cover shelf and the dense metadata list,
    /// persisting the choice so it survives a restart.
    fn toggle_library_view(&mut self) -> Result<()> {
        let mut settings = self.load_settings();
        settings.library_view = if settings.library_view == "list" {
            "shelf".to_string()
        } else {
            "list".to_string()
        };
        self.save_settings(&settings);
        // Full refresh: the whole content area is being replaced, and a
        // partial would leave the previous view ghosting under the new one.
        self.render_current(RefreshMode::Full)
    }

    fn compose_current(&self) -> Result<GrayPage> {
        let l = &self.layout;
        let per_page = l.rows_per_page();
        let screen = self.stack.last().expect("stack never empty");
        Ok(match screen {
            Screen::Home => {
                // When offline (device only), a "reconnect Wi-Fi" row sits at
                // the very top; the standard entries follow.
                let mut rows: Vec<(String, bool)> = Vec::new();
                if self.home_offline {
                    rows.push((HOME_RECONNECT_ROW.to_string(), true));
                }
                rows.extend(HOME_ROWS.iter().map(|r| (r.to_string(), true)));
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
                // Status-icon row, right to left: power (slot 0), then a bell
                // when the web has queued sends, then the Bluetooth glyph when a
                // remote is connected — each in the next slot so they never
                // overlap.
                draw_power_icon(&mut canvas, l);
                let mut slot = 1;
                if !crate::sync::cached_sends(&self.library_dir).is_empty() {
                    draw_bell_icon(&mut canvas, l, slot);
                    slot += 1;
                }
                if self.input.bluetooth_connected() {
                    draw_bluetooth_icon(&mut canvas, l, slot);
                }
                canvas
            }
            Screen::ChapterMenu {
                title,
                key,
                finished,
                download,
            } => {
                let rows: Vec<(String, bool)> = chapter_menu_rows(download, key, *finished)
                    .into_iter()
                    .map(|(label, enabled, _)| (label, enabled))
                    .collect();
                compose_list(l, title, &rows, 0, 1)
            }
            Screen::DownloadAheadMenu {
                manga,
                chapters,
                index,
                ..
            } => {
                let remaining = chapters.len().saturating_sub(*index);
                let rows: Vec<(String, bool)> = download_ahead_rows(remaining)
                    .into_iter()
                    .map(|(label, _)| (label, true))
                    .collect();
                let title = format!("Download — {}", manga.title);
                compose_list(l, &title, &rows, 0, 1)
            }
            Screen::BookMenu {
                series_dir,
                read_key,
                ..
            } => {
                let unread = match read_key {
                    Some(key) => format!("Mark \"{}\" as unread", entry_title(key)),
                    None => "Mark as unread".to_string(),
                };
                let rows = vec![
                    ("All chapters".to_string(), true),
                    (unread, read_key.is_some()),
                    ("Delete this chapter".to_string(), true),
                    ("Delete whole series".to_string(), true),
                ];
                compose_list(l, series_dir, &rows, 0, 1)
            }
            Screen::ConfirmDelete {
                entry,
                series_dir,
                scope,
            } => {
                let (title, confirm) = match scope {
                    DeleteScope::Chapter => (
                        format!("Delete \"{}\"?", entry_title(&entry.relative_path)),
                        "Delete this chapter",
                    ),
                    DeleteScope::Series => (
                        format!("Delete all of \"{series_dir}\"?"),
                        "Delete whole series",
                    ),
                };
                let rows = vec![(confirm.to_string(), true), ("Cancel".to_string(), true)];
                compose_list(l, &title, &rows, 0, 1)
            }
            Screen::ConfirmRemoveSource { source } => {
                let rows = vec![
                    ("Remove source".to_string(), true),
                    ("Cancel".to_string(), true),
                    // Not tappable — reassurance that the library is safe.
                    ("(downloaded chapters stay)".to_string(), false),
                ];
                compose_list(l, &format!("Remove \"{}\"?", source.name), &rows, 0, 1)
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
                // The default profile's library IS the library root — give it a
                // name and it becomes an ordinary "@name" profile like the rest.
                // Offered only while a default profile still exists.
                if profiles.iter().any(|p| p == gideon_core::DEFAULT_PROFILE) {
                    rows.push(("Name the default profile…".to_string(), true));
                }
                compose_list(l, "Profiles", &rows, 0, 1)
            }
            Screen::NewProfile { name } => {
                compose_keyboard(l, "New profile", name, "Create", self.keyboard_shift)
            }
            Screen::ConvertDefault { name } => compose_keyboard(
                l,
                "Name the default profile",
                name,
                "Convert",
                self.keyboard_shift,
            ),
            Screen::Stats => self.compose_stats().to_gray(),
            Screen::Settings => {
                let settings = self.load_settings();
                let mut rows = settings_rows(&settings);
                // A trailing tappable row summarizing storage use; opens the
                // storage detail screen (SETTINGS_STORAGE_ROW).
                let stats = self.storage_stats();
                rows.push((
                    format!(
                        "Storage: {} of {} ›",
                        gideon_core::StorageSize(stats.used),
                        settings.storage_size_limit
                    ),
                    true,
                ));
                // Trailing sync-account row (SETTINGS_ACCOUNT_ROW): shows the
                // signed-in email, or an invitation to sign in.
                rows.push((
                    match self.account_email() {
                        Some(email) => format!("Account: {email} ›"),
                        None => "Account: sign in to sync ›".to_string(),
                    },
                    true,
                ));
                compose_list(l, "Settings", &rows, 0, 1)
            }
            Screen::Storage => self.compose_storage()?,
            Screen::AccountMenu => {
                let rows = match self.account_email() {
                    Some(email) => vec![
                        (format!("Signed in as {email}"), false),
                        ("Sync now".to_string(), true),
                        ("Sign out".to_string(), true),
                    ]
                    .into_iter()
                    // Last: what sync actually did, so a silently failing
                    // background sync stops being invisible. Appended (not
                    // inserted) so the action rows keep their tap indices.
                    .chain(crate::sync::status_line().map(|line| (line, false)))
                    .collect(),
                    None => vec![
                        ("Sign in with email".to_string(), true),
                        (
                            "Use the email + password you set up on the web.".to_string(),
                            false,
                        ),
                    ],
                };
                compose_list(l, "Sync account", &rows, 0, 1)
            }
            Screen::AccountEmail { email } => {
                compose_keyboard(l, "Your email", email, "Next", self.keyboard_shift)
            }
            Screen::AccountPassword { email, password } => compose_keyboard(
                l,
                &format!("Password for {email}"),
                password,
                "Sign in",
                self.keyboard_shift,
            ),
            Screen::PowerMenu => {
                // Wi-Fi networks at the top — scan/connect without digging into
                // Settings; the live status hints at what tapping does.
                let wifi = if self.is_online() {
                    "Wi-Fi: connected (tap to manage)"
                } else {
                    "Wi-Fi: off (tap to scan)"
                };
                let rows = vec![
                    (wifi.to_string(), true),
                    ("Restart gideon".to_string(), true),
                    ("Close gideon".to_string(), true),
                ];
                compose_list(l, "Power", &rows, 0, 1)
            }
            Screen::SentList { items } => {
                let rows: Vec<(String, bool)> =
                    items.iter().map(|s| (s.title.clone(), true)).collect();
                compose_list(l, "Sent to you — tap to find & add", &rows, 0, 1)
            }
            Screen::WifiList { networks } => {
                let nets: Vec<WifiRow> = networks
                    .iter()
                    .map(|n| WifiRow {
                        ssid: n.ssid.clone(),
                        secured: n.secured,
                        saved: n.saved,
                        connected: n.connected,
                        bars: n.bars(),
                    })
                    .collect();
                let title = if networks.is_empty() {
                    "Wi-Fi — no networks found"
                } else {
                    "Wi-Fi"
                };
                // On this screen Wi-Fi is up (we scanned), so the toggle reads
                // on; tapping it turns Wi-Fi off.
                compose_wifi_list(l, title, &nets, true)
            }
            Screen::WifiPassword { ssid, password } => compose_keyboard(
                l,
                &format!("Password — {ssid}"),
                password,
                "Connect",
                self.keyboard_shift,
            ),
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
                compose_search(l, scope, query, self.keyboard_shift)
            }
            Screen::RecentSearches { recents } => {
                let mut rows: Vec<(String, bool)> = vec![("New search…".to_string(), true)];
                for (query, count) in recents {
                    rows.push((format!("\"{query}\"  ({count})"), true));
                }
                compose_list(l, "Search all sources", &rows, 0, 1)
            }
            Screen::SearchResults {
                query,
                results,
                page,
                ..
            } => {
                // Results, plus a trailing "Search more sources" row, paged
                // together so the widen row is reachable on the last page.
                let mut labels: Vec<(String, bool)> = results
                    .iter()
                    .map(|(s, m)| (format!("{} — {}", m.title, s.name), true))
                    .collect();
                labels.push((SEARCH_MORE_ROW.to_string(), true));
                let total = labels.len();
                let rows = paged(&labels, *page, per_page).to_vec();
                let title = if results.is_empty() {
                    format!("\"{query}\" — none in your sources")
                } else {
                    format!("\"{query}\"")
                };
                compose_list(l, &title, &rows, *page, l.page_count(total))
            }
            Screen::Popular { mangas, page } => {
                let rows: Vec<(String, bool)> = paged(mangas, *page, per_page)
                    .iter()
                    .map(|m| (m.title.clone(), true))
                    .collect();
                compose_list(l, "Popular manga", &rows, *page, l.page_count(mangas.len()))
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
                sort,
            } => {
                // A download icon marks what's on disk; a book icon marks
                // what's been read (finished). Downloaded chapters open
                // instantly.
                let index = gideon_core::SeriesIndex::load(&self.library_dir);
                let (dir, downloaded) = match index.find_manga(&source.id, &manga.id) {
                    Some((dir, series)) => (dir.to_string(), series.downloaded.clone()),
                    None => (String::new(), Default::default()),
                };
                let nums: Vec<Option<f32>> = chapters.iter().map(|c| c.num).collect();
                let order = chapter_display_order(&nums, *sort);
                let rows: Vec<(String, bool, bool)> = self.with_progress(|_, store| {
                    order
                        .iter()
                        .skip(*page * per_page)
                        .take(per_page)
                        .map(|&i| {
                            let c = &chapters[i];
                            let on_disk = downloaded.contains_key(&c.id);
                            let key = downloaded.get(&c.id).map(|file| format!("{dir}/{file}"));
                            let finished = key
                                .as_deref()
                                .and_then(|k| store.get(k))
                                .is_some_and(|p| p.is_finished());
                            let is_last =
                                key.is_some() && store.last_opened(&dir) == key.as_deref();
                            (label_with_last(c.label(), is_last), on_disk, finished)
                        })
                        .collect()
                });
                compose_chapter_list(
                    l,
                    &manga.title,
                    &rows,
                    *page,
                    l.page_count(chapters.len()),
                    *sort,
                )
            }
            Screen::DownloadedChapters {
                title,
                entries,
                page,
                sort,
            } => {
                // Everything here is on disk by definition; the book icon still
                // marks what's been read (finished).
                let nums: Vec<Option<f32>> = entries
                    .iter()
                    .map(|e| label_chapter_num(&chapter_label(&e.relative_path)))
                    .collect();
                let order = chapter_display_order(&nums, *sort);
                let rows: Vec<(String, bool, bool)> = self.with_progress(|_, store| {
                    order
                        .iter()
                        .skip(*page * per_page)
                        .take(per_page)
                        .map(|&i| {
                            let e = &entries[i];
                            let finished =
                                store.get(&e.relative_path).is_some_and(|p| p.is_finished());
                            let is_last = store.last_opened(series_key_of(&e.relative_path))
                                == Some(e.relative_path.as_str());
                            (
                                label_with_last(chapter_label(&e.relative_path), is_last),
                                true,
                                finished,
                            )
                        })
                        .collect()
                });
                compose_chapter_list(l, title, &rows, *page, l.page_count(entries.len()), *sort)
            }
            Screen::UpdatePrompt { body } => compose_message(l, "Update available", body),
            Screen::Message { title, body } => compose_message(l, title, body),
        })
    }

    /// The storage detail screen: how much downloaded content is on disk
    /// against the budget, plus the manual "free up space now" action.
    fn compose_storage(&self) -> Result<GrayPage> {
        let l = &self.layout;
        let settings = self.load_settings();
        let stats = self.storage_stats();
        let limit = settings.storage_size_limit;
        let used = gideon_core::StorageSize(stats.used);
        let pct = if limit.bytes() > 0 {
            (stats.used.saturating_mul(100) / limit.bytes()).min(100)
        } else {
            0
        };
        let rows = vec![
            (format!("Used: {used} of {limit} ({pct}%)"), false),
            (
                format!("{} chapters · {} series", stats.chapters, stats.series),
                false,
            ),
            (
                "Auto-cleanup removes least-recently-read first".to_string(),
                false,
            ),
            ("Free up space now".to_string(), true),
        ];
        Ok(compose_list(l, "Storage", &rows, 0, 1))
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

    /// Forget a chapter's reading progress ("mark as unread"), persist it, and
    /// drop the cache. Returns whether anything was actually cleared.
    fn mark_unread(&self, key: &str) -> Result<bool> {
        let path = progress_path(&self.library_dir);
        // forget() does the load-remove-write under the store lock, so a
        // background sync's concurrent addition isn't dropped by a stale save.
        let removed = ProgressStore::forget(&path, key)?;
        if removed {
            self.invalidate_progress_cache();
        }
        Ok(removed)
    }

    /// Mark a downloaded chapter as read (finished): record progress at its last
    /// page so it shows the read icon and the shelf skips it when resuming. Page
    /// count comes from the CBZ (a local zip-directory read) so `percent()` and
    /// resume are honest.
    fn mark_read(&self, key: &str) -> Result<()> {
        let file = self.library_dir.join(key);
        let total = CbzDocument::open(&file)
            .map(|d| d.page_count())
            .unwrap_or(1)
            .max(1);
        let path = progress_path(&self.library_dir);
        let mut store = ProgressStore::load(&path).unwrap_or_default();
        store.update(key, total - 1, total);
        // Mark-read advances to the last page; merge_save folds in (furthest
        // wins) so a concurrent sync write to another chapter survives.
        store.merge_save(&path)?;
        self.invalidate_progress_cache();
        Ok(())
    }

    /// Open the per-chapter action menu (the ⋮ button). `key` is the chapter's
    /// progress key when it's downloaded, else `None`; `download` carries the
    /// source context for the download actions (`None` from the offline list).
    fn open_chapter_menu(
        &mut self,
        title: String,
        key: Option<String>,
        download: Option<DownloadContext>,
    ) -> Result<Flow> {
        let finished = key
            .as_ref()
            .map(|k| self.with_progress(|_, s| s.get(k).is_some_and(|p| p.is_finished())))
            .unwrap_or(false);
        self.push(Screen::ChapterMenu {
            title,
            key,
            finished,
            download,
        })?;
        Ok(Flow::Continue)
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
const SHEET_ROW_AUTO_SPREAD: usize = 2;
const SHEET_ROW_CLOSE: usize = 3;
const SHEET_ROW_COUNT: u32 = 4;

fn controls_sheet_labels(locked: bool, auto_spread: bool) -> [String; 4] {
    [
        "Rotate 90°".to_string(),
        format!("Orientation: {}", if locked { "locked" } else { "auto" }),
        format!(
            "Auto-rotate spreads: {}",
            if auto_spread { "on" } else { "off" }
        ),
        "Close".to_string(),
    ]
}

/// The controls sheet as a reading-frame strip (the caller rotates it
/// into the panel): full-width rows with a dark top border.
fn compose_controls_sheet(
    reading_w: u32,
    row_h: u32,
    text_px: f32,
    pad: u32,
    locked: bool,
    auto_spread: bool,
) -> GrayPage {
    let mut sheet = GrayPage::new_white(reading_w, SHEET_ROW_COUNT * row_h.max(1));
    hline(&mut sheet, 0, 0x00);
    for (i, label) in controls_sheet_labels(locked, auto_spread)
        .iter()
        .enumerate()
    {
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
    auto_spread: bool,
) -> Result<()> {
    let reading_w = if rotation % 180 == 90 {
        panel.height
    } else {
        panel.width
    };
    let sheet = compose_controls_sheet(
        reading_w,
        panel.row_h,
        panel.text_px,
        panel.pad,
        locked,
        auto_spread,
    );
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
    compose_chrome_reserved(l, title, page, page_count, show_back, 0)
}

/// The shared chrome, with `right_reserved` pixels held free at the right of
/// the title bar (so a chapter list can park its sort button there without the
/// page indicator overlapping it).
fn compose_chrome_reserved(
    l: &UiLayout,
    title: &str,
    page: usize,
    page_count: usize,
    show_back: bool,
    right_reserved: u32,
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
            l.width.saturating_sub(w + l.pad + right_reserved),
            text_y(0, l.title_h),
            l.text_px,
            &label,
            w,
            false,
        );
    }
    hline(&mut canvas, l.title_h - 1, 0x55);

    // Bottom navigation bar. A single-page list shows just [< Back]; a
    // multi-page one splits into [< Back][First][Prev][Next][Last] (see
    // [`UiLayout::nav_buttons`]).
    hline(&mut canvas, l.nav_top(), 0x55);
    let nav_y = text_y(l.nav_top(), l.nav_h);
    for (target, bx, bw) in l.nav_buttons(page_count > 1) {
        let label = match target {
            TapTarget::Back if show_back => "< Back",
            TapTarget::Back => continue,
            TapTarget::First => "First",
            TapTarget::Prev => "Prev",
            TapTarget::Next => "Next",
            TapTarget::Last => "Last",
            _ => continue,
        };
        draw_text(
            &mut canvas,
            bx + l.pad,
            nav_y,
            l.text_px,
            label,
            bw.saturating_sub(2 * l.pad),
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

/// The chapter list: like [`compose_list`] but each row carries a **download**
/// icon when it's on disk and a **book** icon when it's been read, in a left
/// gutter (instead of cryptic text marks).
fn compose_chapter_list(
    l: &UiLayout,
    title: &str,
    rows: &[(String, bool, bool)],
    page: usize,
    page_count: usize,
    sort: ChapterSort,
) -> GrayPage {
    let sort_w = sort_button_width(l);
    let mut canvas = compose_chrome_reserved(l, title, page, page_count, true, sort_w + l.pad);
    // The sort button lives at the right edge of the title bar.
    draw_text(
        &mut canvas,
        sort_button_x(l) + l.pad,
        l.title_h.saturating_sub(l.text_px as u32 + 4) / 2,
        l.text_px,
        sort.label(),
        sort_w.saturating_sub(l.pad),
        false,
    );
    let icon = (l.row_h as f32 * 0.5) as u32;
    let gap = 5u32;
    let gutter = 2 * icon + 2 * gap;
    let text_x = l.pad + gutter;
    // Reserve the right edge for the ⋮ (kebab) read-status button.
    let kebab_x = chapter_kebab_x(l);
    let text_w = kebab_x.saturating_sub(gap).saturating_sub(text_x);
    for (i, (text, downloaded, read)) in rows.iter().take(l.rows_per_page()).enumerate() {
        let top = l.row_top(i);
        let icon_y = top + l.row_h.saturating_sub(icon) / 2;
        if *downloaded {
            draw_download_icon(&mut canvas, l.pad, icon_y, icon, 0x00);
        }
        if *read {
            draw_book_icon(&mut canvas, l.pad + icon + gap, icon_y, icon, 0x00);
        }
        draw_text(
            &mut canvas,
            text_x,
            top + l.row_h.saturating_sub(l.text_px as u32 + 4) / 2,
            l.text_px,
            text,
            text_w,
            false,
        );
        draw_kebab_icon(&mut canvas, kebab_x, icon_y, icon, 0x00);
        let sep_y = top + l.row_h - 1;
        if sep_y < l.nav_top() {
            hline(&mut canvas, sep_y, 0xDD);
        }
    }
    canvas
}

/// Source context a chapter's ⋮ menu needs to drive its download actions:
/// which source/manga it belongs to, the full chapter list and this chapter's
/// position in it (so "download from here" can walk forward). Absent when the
/// menu is opened from the offline downloaded-chapters list.
#[derive(Debug, Clone)]
struct DownloadContext {
    source: SourceEntry,
    manga: MangaEntry,
    chapters: Vec<ChapterEntry>,
    index: usize,
}

/// One row of a chapter's ⋮ menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChapterMenuAction {
    /// Download just this chapter (foreground, with progress).
    DownloadThis,
    /// Open the "download from here…" count picker.
    DownloadAhead,
    MarkRead,
    MarkUnread,
    /// Delete this chapter's downloaded file.
    DeleteDownload,
}

/// The rows of a chapter's ⋮ menu, computed identically by the renderer (for
/// labels + enabled state) and the tap handler (to map a row to its action),
/// so the two never disagree about what sits where.
///
/// Download actions appear when there's a source link; read-status and delete
/// appear once the chapter is on disk. The offline downloaded list (no source)
/// therefore shows only the read-status + delete rows.
fn chapter_menu_rows(
    download: &Option<DownloadContext>,
    key: &Option<String>,
    finished: bool,
) -> Vec<(String, bool, ChapterMenuAction)> {
    let mut rows = Vec::new();
    let downloaded = key.is_some();
    if let Some(ctx) = download {
        if !downloaded {
            rows.push((
                "Download this chapter".to_string(),
                true,
                ChapterMenuAction::DownloadThis,
            ));
        }
        let ahead = ctx.chapters.len().saturating_sub(ctx.index);
        rows.push((
            "Download from here…".to_string(),
            ahead > 1,
            ChapterMenuAction::DownloadAhead,
        ));
    }
    if downloaded {
        rows.push((
            "Mark as read".to_string(),
            !finished,
            ChapterMenuAction::MarkRead,
        ));
        rows.push((
            "Mark as unread".to_string(),
            finished,
            ChapterMenuAction::MarkUnread,
        ));
        rows.push((
            "Delete download".to_string(),
            true,
            ChapterMenuAction::DeleteDownload,
        ));
    }
    if rows.is_empty() {
        // No source link and nothing on disk: there's nothing to act on yet.
        rows.push((
            "Download to track read status".to_string(),
            false,
            ChapterMenuAction::DownloadThis,
        ));
    }
    rows
}

/// The chapter counts offered by the "download from here…" picker: the round
/// batch sizes smaller than what's left, then always an "all remaining" option.
/// `remaining` is the number of chapters from the chosen one to the end.
fn download_ahead_counts(remaining: usize) -> Vec<usize> {
    let mut counts: Vec<usize> = [5usize, 10, 25]
        .into_iter()
        .filter(|&n| n < remaining)
        .collect();
    counts.push(remaining); // "all remaining"
    counts
}

/// Labelled rows for the "download from here…" picker, shared by the renderer
/// and the tap handler so they agree on row order.
fn download_ahead_rows(remaining: usize) -> Vec<(String, usize)> {
    download_ahead_counts(remaining)
        .into_iter()
        .map(|n| {
            let label = if n == remaining {
                format!("Download all remaining ({n})")
            } else {
                format!("Download {n} chapters")
            };
            (label, n)
        })
        .collect()
}

/// Width reserved for the title-bar sort button: the widest label plus
/// padding on each side. Same value for draw and hit-test.
fn sort_button_width(l: &UiLayout) -> u32 {
    let widest = [
        ChapterSort::Source,
        ChapterSort::Ascending,
        ChapterSort::Descending,
    ]
    .iter()
    .map(|s| measure_text(l.text_px, s.label(), false))
    .max()
    .unwrap_or(0);
    widest + 2 * l.pad
}

/// The left x of the title-bar sort button (it runs to the right edge).
fn sort_button_x(l: &UiLayout) -> u32 {
    l.width.saturating_sub(sort_button_width(l) + l.pad)
}

/// Best-effort chapter number parsed from a downloaded file's label, used only
/// for sorting. Prefers the number right after a "chapter"/"ch"/"#" marker
/// (so "Vol.01 Ch.012" sorts by 12, not the volume), else the first number in
/// the string. `None` when there's nothing numeric to go on.
fn label_chapter_num(label: &str) -> Option<f32> {
    let lower = label.to_ascii_lowercase();
    for marker in ["chapter", "ch", "#"] {
        if let Some(pos) = lower.find(marker) {
            if let Some(n) = first_number(&lower[pos + marker.len()..]) {
                return Some(n);
            }
        }
    }
    first_number(&lower)
}

/// The first decimal number in `s` (e.g. "012.5" → 12.5), or `None`.
fn first_number(s: &str) -> Option<f32> {
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i].is_ascii_digit() {
            let start = i;
            while i < bytes.len() && (bytes[i].is_ascii_digit() || bytes[i] == b'.') {
                i += 1;
            }
            return s[start..i].trim_end_matches('.').parse().ok();
        }
        i += 1;
    }
    None
}

/// The x where a chapter row's ⋮ (kebab) read-status button sits — shared by
/// the renderer (to draw it) and the tap handler (to detect a hit on it).
fn chapter_kebab_x(l: &UiLayout) -> u32 {
    let icon = (l.row_h as f32 * 0.5) as u32;
    l.width.saturating_sub(l.pad + icon)
}

/// Whether a tap at `x` landed on a chapter row's ⋮ button (its right-edge
/// zone, made generous for fat fingers) rather than the row body.
fn chapter_kebab_tapped(l: &UiLayout, x: u32) -> bool {
    let icon = (l.row_h as f32 * 0.5) as u32;
    x >= chapter_kebab_x(l).saturating_sub(icon)
}

/// Scan for Wi-Fi and order the results for the list: the connected network
/// first, then saved ones, then the rest by signal strength — so the network
/// you're on (and the ones you'll likely want) sit at the top.
fn scan_wifi_sorted() -> Vec<gideon_device::network::WifiNetwork> {
    let mut nets = gideon_device::network::scan_networks();
    nets.sort_by(|a, b| {
        b.connected
            .cmp(&a.connected)
            .then(b.saved.cmp(&a.saved))
            .then(b.signal.cmp(&a.signal))
    });
    nets
}

/// One network as the Wi-Fi list needs to draw it: label, plus the flags that
/// pick the row's glyphs. Kept as a plain tuple-struct so the renderer doesn't
/// depend on `gideon_device`.
struct WifiRow {
    ssid: String,
    secured: bool,
    saved: bool,
    connected: bool,
    bars: u8,
}

/// The Wi-Fi list, styled like a phone's Wi-Fi settings. **Row 0** is a "Wi-Fi"
/// row with an on/off **toggle switch** on the right; then the networks — a
/// **checkmark** on the connected one (a faint dot on a saved one), the SSID, a
/// **lock** for secured networks and **signal bars** on the right; then a final
/// **Scan again** row. Row order here must match the screen's tap mapping.
fn compose_wifi_list(l: &UiLayout, title: &str, nets: &[WifiRow], wifi_on: bool) -> GrayPage {
    let mut canvas = compose_chrome_opts(l, title, 0, 1, true);
    let icon = (l.row_h as f32 * 0.5) as u32;
    let gap = 6u32;
    let text_x = l.pad + icon + gap;
    let bars_w = icon;
    let bars_x = l.width.saturating_sub(l.pad + bars_w);
    let lock_x = bars_x.saturating_sub(gap + icon);
    let text_w = lock_x.saturating_sub(gap).saturating_sub(text_x);
    let text_y = |top: u32| top + l.row_h.saturating_sub(l.text_px as u32 + 4) / 2;
    // Row 0 (toggle) + networks + a "Scan again" row.
    let total = nets.len() + 2;
    for i in 0..total.min(l.rows_per_page()) {
        let top = l.row_top(i);
        let icon_y = top + l.row_h.saturating_sub(icon) / 2;
        if i == 0 {
            // The Wi-Fi on/off toggle.
            draw_text(
                &mut canvas,
                l.pad,
                text_y(top),
                l.text_px,
                "Wi-Fi",
                text_w,
                true,
            );
            let sw_w = 2 * icon + gap;
            let sw_h = icon.min(l.row_h.saturating_sub(8));
            let sw_x = l.width.saturating_sub(l.pad + sw_w);
            let sw_y = top + l.row_h.saturating_sub(sw_h) / 2;
            draw_toggle_switch(&mut canvas, sw_x, sw_y, sw_w, sw_h, wifi_on, 0x00);
        } else if i <= nets.len() {
            let n = &nets[i - 1];
            if n.connected {
                draw_check_icon(&mut canvas, l.pad, icon_y, icon, 0x00);
            } else if n.saved {
                draw_dot_icon(&mut canvas, l.pad, icon_y, icon, 0x66);
            }
            draw_text(
                &mut canvas,
                text_x,
                text_y(top),
                l.text_px,
                &n.ssid,
                text_w,
                n.connected,
            );
            if n.secured {
                draw_lock_icon(&mut canvas, lock_x, icon_y, icon, 0x00);
            }
            draw_wifi_bars(&mut canvas, bars_x, icon_y, bars_w, n.bars, 0x00);
        } else {
            draw_text(
                &mut canvas,
                l.pad,
                text_y(top),
                l.text_px,
                "Scan again",
                l.width.saturating_sub(2 * l.pad),
                true,
            );
        }
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
fn apply_key_edit(buffer: &str, key: Key, shift: bool) -> Option<String> {
    match key {
        Key::Char(c) => {
            let mut b = buffer.to_string();
            // Shift types the key's upper register: upper-case letters and the
            // symbols (`!`, `#`, `?` …) — passwords need both.
            b.push(if shift { layout::shifted_char(c) } else { c });
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
        // Shift toggles case; handled by the caller (it mutates keyboard state
        // and repaints). Search runs the action. Neither edits the buffer.
        Key::Shift | Key::Search => None,
    }
}

/// The search screen: chrome + the query line + the on-screen keyboard.
fn compose_search(l: &UiLayout, source_name: &str, query: &str, shift: bool) -> GrayPage {
    compose_keyboard(l, &format!("Search {source_name}"), query, "Search", shift)
}

/// A keyboard screen: chrome + the edited line + the on-screen keyboard,
/// with the action key labeled `action` ("Search", "Create"…).
fn compose_keyboard(
    l: &UiLayout,
    title: &str,
    buffer: &str,
    action: &str,
    shift: bool,
) -> GrayPage {
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
            // Show the character that will actually be typed.
            Key::Char(c) if shift => layout::shifted_char(c).to_string(),
            Key::Char(c) => c.to_string(),
            Key::Backspace => "<del".to_string(),
            Key::Space => "space".to_string(),
            Key::Shift if shift => "SHIFT".to_string(),
            Key::Shift => "shift".to_string(),
            Key::Search => action.to_string(),
        };
        // Bold the action key, and the Shift key while it's active, so its state
        // is visible.
        let bold = key == Key::Search || (key == Key::Shift && shift);
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

/// Center x of title-bar status icon `slot`: 0 is the power symbol at the far
/// right, higher numbers step left by one title-bar height each, so the power /
/// notification / Bluetooth icons sit in a tidy row that never overlaps.
fn title_icon_cx(l: &UiLayout, slot: u32) -> f32 {
    (l.width.saturating_sub(l.title_h / 2 + l.pad) as f32) - slot as f32 * l.title_h as f32
}

/// A bell in the title bar (with a small "new" badge dot) shown when the web
/// has queued manga to send to this device. Drawn at status-icon `slot`.
fn draw_bell_icon(canvas: &mut GrayPage, l: &UiLayout, slot: u32) {
    let cx = title_icon_cx(l, slot);
    let cy = (l.title_h as f32) * 0.55;
    let h = l.title_h as f32 * 0.32;
    let w = l.title_h as f32 * 0.24;
    let top = cy - h;
    let shoulder = cy - h * 0.35;
    let rim = cy + h * 0.5;
    let neck = w * 0.5;
    let mut seg = |ax: f32, ay: f32, bx: f32, by: f32| {
        line(canvas, ax as i32, ay as i32, bx as i32, by as i32, 0x00);
    };
    seg(cx, top, cx, top - 2.0); // handle
    seg(cx - neck, shoulder, cx, top); // dome left
    seg(cx + neck, shoulder, cx, top); // dome right
    seg(cx - neck, shoulder, cx - w, rim); // left flare
    seg(cx + neck, shoulder, cx + w, rim); // right flare
    seg(cx - w, rim, cx + w, rim); // rim
    seg(cx, rim, cx, rim + h * 0.22); // clapper
                                      // The "you've got something" badge: a filled dot at the top-right.
    let (bx, by) = (cx + w * 0.85, top + 2.0);
    let br = (l.title_h as f32 * 0.11).max(2.0);
    for dy in -(br as i32)..=(br as i32) {
        for dx in -(br as i32)..=(br as i32) {
            if (dx * dx + dy * dy) as f32 <= br * br {
                plot(canvas, bx as i32 + dx, by as i32 + dy, 0x00);
            }
        }
    }
}

/// Draw the Bluetooth rune in the title bar (at status-icon `slot`) to show a
/// page-turn remote is connected. The classic glyph: a vertical spine with two
/// right-hand triangular "flags" whose long diagonals cross through the center
/// to the left-hand tips.
fn draw_bluetooth_icon(canvas: &mut GrayPage, l: &UiLayout, slot: u32) {
    let cx = title_icon_cx(l, slot);
    let cy = (l.title_h as f32) * 0.55;
    let h = (l.title_h as f32) * 0.30; // half height
    let w = (l.title_h as f32) * 0.16; // half width
    let (y0, y1, y3, y4) = (cy - h, cy - h / 2.0, cy + h / 2.0, cy + h);
    let (xl, xc, xr) = (cx - w, cx, cx + w);
    let mut seg = |ax: f32, ay: f32, bx: f32, by: f32| {
        line(canvas, ax as i32, ay as i32, bx as i32, by as i32, 0x00);
    };
    seg(xc, y0, xc, y4); // spine
    seg(xc, y0, xr, y1); // top → upper-right
    seg(xr, y1, xl, y3); // upper-right → lower-left (crossing)
    seg(xc, y4, xr, y3); // bottom → lower-right
    seg(xr, y3, xl, y1); // lower-right → upper-left (crossing)
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

/// Plot a single pixel, bounds-checked.
fn plot(canvas: &mut GrayPage, x: i32, y: i32, value: u8) {
    if x >= 0 && y >= 0 && (x as u32) < canvas.width && (y as u32) < canvas.height {
        canvas.pixels[(y as u32 * canvas.width + x as u32) as usize] = value;
    }
}

/// A 2px-ish line between two points (Bresenham, thickened vertically by 1).
fn line(canvas: &mut GrayPage, x0: i32, y0: i32, x1: i32, y1: i32, value: u8) {
    let (dx, dy) = ((x1 - x0).abs(), -(y1 - y0).abs());
    let (sx, sy) = (if x0 < x1 { 1 } else { -1 }, if y0 < y1 { 1 } else { -1 });
    let (mut x, mut y, mut err) = (x0, y0, dx + dy);
    loop {
        plot(canvas, x, y, value);
        plot(canvas, x + 1, y, value);
        if x == x1 && y == y1 {
            break;
        }
        let e2 = 2 * err;
        if e2 >= dy {
            err += dy;
            x += sx;
        }
        if e2 <= dx {
            err += dx;
            y += sy;
        }
    }
}

/// A download icon (arrow into a tray) in the `s`×`s` box at (`x`, `y`):
/// what's stocked on disk.
fn draw_download_icon(canvas: &mut GrayPage, x: u32, y: u32, s: u32, value: u8) {
    let (x, y, s) = (x as i32, y as i32, s as i32);
    let cx = x + s / 2;
    // Down-arrow shaft.
    line(canvas, cx, y + s / 6, cx, y + (s * 3) / 5, value);
    // Arrowhead.
    line(
        canvas,
        cx,
        y + (s * 2) / 3,
        x + s / 4,
        y + (s * 2) / 5,
        value,
    );
    line(
        canvas,
        cx,
        y + (s * 2) / 3,
        x + (s * 3) / 4,
        y + (s * 2) / 5,
        value,
    );
    // Tray.
    let tray_y = y + (s * 5) / 6;
    line(canvas, x + s / 6, tray_y, x + (s * 5) / 6, tray_y, value);
    line(canvas, x + s / 6, tray_y, x + s / 6, tray_y - s / 6, value);
    line(
        canvas,
        x + (s * 5) / 6,
        tray_y,
        x + (s * 5) / 6,
        tray_y - s / 6,
        value,
    );
}

/// A book icon in the `s`×`s` box at (`x`, `y`): the chapter's been read.
fn draw_book_icon(canvas: &mut GrayPage, x: u32, y: u32, s: u32, value: u8) {
    let (x, y, s) = (x as i32, y as i32, s as i32);
    let (l, t, r, b) = (x + s / 6, y + s / 6, x + (s * 5) / 6, y + (s * 5) / 6);
    let cx = x + s / 2;
    // Cover outline + central spine (an open book).
    line(canvas, l, t, r, t, value);
    line(canvas, l, b, r, b, value);
    line(canvas, l, t, l, b, value);
    line(canvas, r, t, r, b, value);
    line(canvas, cx, t, cx, b, value);
    // A couple of page lines per side.
    line(canvas, l + s / 8, t + s / 4, cx - s / 12, t + s / 4, value);
    line(canvas, cx + s / 12, t + s / 4, r - s / 8, t + s / 4, value);
}

/// A vertical "kebab" (⋮ — three stacked dots) in the `s`×`s` box at (`x`, `y`):
/// the per-chapter overflow/read-status button.
fn draw_kebab_icon(canvas: &mut GrayPage, x: u32, y: u32, s: u32, value: u8) {
    let (x, y, s) = (x as i32, y as i32, s as i32);
    let cx = x + s / 2;
    let r = (s / 12).max(1);
    for k in 0..3 {
        let cy = y + s / 4 + k * (s / 4);
        for dy in -r..=r {
            let dx = ((r * r - dy * dy) as f32).sqrt().round() as i32;
            line(canvas, cx - dx, cy + dy, cx + dx, cy + dy, value);
        }
    }
}

/// Signal-strength bars (4 ascending) in the `s`×`s` box at (`x`, `y`): the
/// first `bars` are filled solid, the rest faint — like a phone's Wi-Fi meter.
fn draw_wifi_bars(canvas: &mut GrayPage, x: u32, y: u32, s: u32, bars: u8, value: u8) {
    let (x, y, s) = (x as i32, y as i32, s as i32);
    let bw = (s / 6).max(1);
    let gap = (s / 12).max(1);
    let base = y + s;
    for i in 0..4i32 {
        let h = (s * (i + 1) / 4).max(2);
        let bx = x + i * (bw + gap);
        let v = if (i as u8) < bars { value } else { 0xCC };
        for xx in bx..bx + bw {
            line(canvas, xx, base - h, xx, base, v);
        }
    }
}

/// A padlock in the `s`×`s` box at (`x`, `y`): the network is secured.
fn draw_lock_icon(canvas: &mut GrayPage, x: u32, y: u32, s: u32, value: u8) {
    let (x, y, s) = (x as i32, y as i32, s as i32);
    let (bl, br) = (x + s / 4, x + (s * 3) / 4);
    let (bt, bb) = (y + s / 2, y + (s * 5) / 6);
    // Body.
    line(canvas, bl, bt, br, bt, value);
    line(canvas, bl, bb, br, bb, value);
    line(canvas, bl, bt, bl, bb, value);
    line(canvas, br, bt, br, bb, value);
    // Shackle.
    let (sl, sr, st) = (x + s / 3, x + (s * 2) / 3, y + s / 4);
    line(canvas, sl, bt, sl, st, value);
    line(canvas, sr, bt, sr, st, value);
    line(canvas, sl, st, sr, st, value);
}

/// A checkmark in the `s`×`s` box at (`x`, `y`): the connected network.
fn draw_check_icon(canvas: &mut GrayPage, x: u32, y: u32, s: u32, value: u8) {
    let (x, y, s) = (x as i32, y as i32, s as i32);
    line(
        canvas,
        x + s / 6,
        y + s / 2,
        x + (s * 2) / 5,
        y + (s * 2) / 3,
        value,
    );
    line(
        canvas,
        x + (s * 2) / 5,
        y + (s * 2) / 3,
        x + (s * 5) / 6,
        y + s / 4,
        value,
    );
}

/// A small filled dot in the `s`×`s` box at (`x`, `y`): a saved (but not
/// currently connected) network.
fn draw_dot_icon(canvas: &mut GrayPage, x: u32, y: u32, s: u32, value: u8) {
    let (x, y, s) = (x as i32, y as i32, s as i32);
    let (cx, cy, r) = (x + s / 2, y + s / 2, s / 8);
    for yy in cy - r..=cy + r {
        line(canvas, cx - r, yy, cx + r, yy, value);
    }
}

/// A toggle switch in a `w`×`h` pill at (`x`, `y`): the knob sits **right** and
/// the track is filled when `on`, **left** on an open track when off — like a
/// phone settings toggle.
fn draw_toggle_switch(canvas: &mut GrayPage, x: u32, y: u32, w: u32, h: u32, on: bool, value: u8) {
    let (x, y, w, h) = (x as i32, y as i32, w as i32, h as i32);
    let r = h / 2;
    // Pill outline: straight top/bottom between the rounded ends, plus end caps.
    line(canvas, x + r, y, x + w - r, y, value);
    line(canvas, x + r, y + h, x + w - r, y + h, value);
    for (cx, sweep) in [(x + r, (90i32, 270i32)), (x + w - r, (-90i32, 90i32))] {
        let cy = y + r;
        for d in sweep.0..=sweep.1 {
            let a = (d as f32).to_radians();
            plot(
                canvas,
                cx + (a.cos() * r as f32).round() as i32,
                cy + (a.sin() * r as f32).round() as i32,
                value,
            );
        }
    }
    // Knob: a filled disc at the on/off end.
    let cx = if on { x + w - r } else { x + r };
    let cy = y + r;
    let kr = r - 2;
    for dy in -kr..=kr {
        let dx = ((kr * kr - dy * dy) as f32).sqrt().round() as i32;
        line(canvas, cx - dx, cy + dy, cx + dx, cy + dy, value);
    }
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
/// Overlay a full-size grayscale layer onto an RGB canvas, keeping only its
/// ink: white stays transparent so text can be drawn over colour without
/// punching a white box through it.
/// Draw a decoded cover into an RGB canvas, scaled to fit its box and centred
/// in it, clipped to the canvas. Covers keep their colour — this is the one
/// place on the panel where Kaleido earns its keep.
fn blit_thumb(dst: &mut RgbPage, img: &image::DynamicImage, x: u32, y: u32, w: u32, h: u32) {
    if w == 0 || h == 0 || x >= dst.width || y >= dst.height {
        return;
    }
    let scaled = img.resize(w, h, image::imageops::FilterType::Triangle);
    let rgb = scaled.to_rgb8();
    let (iw, ih) = (rgb.width(), rgb.height());
    // Centre whatever the aspect-preserving resize produced, so a square page
    // and a tall cover both sit in the middle of their box.
    let off_x = x + w.saturating_sub(iw) / 2;
    let off_y = y + h.saturating_sub(ih) / 2;
    for row in 0..ih.min(dst.height.saturating_sub(off_y)) {
        for col in 0..iw.min(dst.width.saturating_sub(off_x)) {
            let px = rgb.get_pixel(col, row).0;
            let idx = (((off_y + row) * dst.width + off_x + col) * 3) as usize;
            dst.pixels[idx..idx + 3].copy_from_slice(&px);
        }
    }
}

/// Fill a rectangle on an RGB canvas, clipped to it. Anything outside is
/// dropped rather than wrapping onto the next row.
fn fill_rect_rgb(dst: &mut RgbPage, x: u32, y: u32, w: u32, h: u32, color: [u8; 3]) {
    if x >= dst.width || y >= dst.height {
        return;
    }
    let w = w.min(dst.width - x);
    let h = h.min(dst.height - y);
    for row in 0..h {
        let start = (((y + row) * dst.width + x) * 3) as usize;
        for col in 0..w {
            let idx = start + (col * 3) as usize;
            dst.pixels[idx..idx + 3].copy_from_slice(&color);
        }
    }
}

fn copy_gray_into_rgb(dst: &mut RgbPage, src: &GrayPage) {
    for y in 0..src.height.min(dst.height) {
        for x in 0..src.width.min(dst.width) {
            let g = src.pixel(x, y);
            if g == 0xFF {
                continue;
            }
            let idx = ((y * dst.width + x) * 3) as usize;
            dst.pixels[idx..idx + 3].copy_from_slice(&[g, g, g]);
        }
    }
}

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

/// A chapter queued for background pre-download. Carries owned, `Send` data so
/// it can cross to the worker thread (the gateway there is a separate instance).
struct PreloadJob {
    source: SourceEntry,
    manga: MangaEntry,
    chapter_id: String,
    /// The cancellation epoch this job was queued under. The worker drops the
    /// job if the epoch has since moved on (the user left the manga).
    epoch: u64,
    /// A deliberate "download these" request (the ⋮ menu's batch download), as
    /// opposed to the automatic read-ahead. Persistent jobs ignore the epoch so
    /// leaving the manga doesn't abandon a download the user explicitly asked
    /// for.
    persistent: bool,
}

/// Background chapter pre-downloader: a single worker thread that owns its own
/// [`SourceGateway`] and drains a queue of [`PreloadJob`]s, downloading each
/// chapter that isn't already on disk and recording it in the series index.
/// Single-threaded by design — chapters fetch one at a time in queue order, and
/// re-queued or already-stored chapters are cheap no-ops.
///
/// Leaving a manga bumps `epoch`, so chapters queued for it that haven't started
/// yet are skipped instead of trickling down in the background after you've
/// moved on. The chapter already mid-download finishes (it can't be torn out
/// from under the source), then the worker stops.
struct Predownloader {
    tx: mpsc::Sender<PreloadJob>,
    /// Chapter keys already handed to the worker, so repeated kicks (every
    /// reader open / page advance) don't enqueue the same chapter twice.
    ///
    /// Shared with the worker, which **removes** a key whose download failed
    /// (offline, source hiccup). A failed look-ahead must not be remembered as
    /// "done": otherwise the chapter you were about to read stays un-stocked for
    /// the rest of the session and no later kick — the next page turn, or the
    /// re-kick after waking from sleep — can ever retry it.
    queued: Arc<Mutex<HashSet<String>>>,
    /// The current cancellation epoch, shared with the worker. Queueing stamps
    /// jobs with it; [`Self::cancel_pending`] bumps it.
    epoch: Arc<AtomicU64>,
}

/// How many times the worker attempts a chapter before giving up on it, and how
/// long it waits between attempts. Sized for the common failure: waking up with
/// the radio still coming back, where the first attempt fires before Wi-Fi has
/// associated. Idle-priority and bounded, so a dead network costs a background
/// thread half a minute per chapter and nothing else.
const PRELOAD_ATTEMPTS: usize = 3;
const PRELOAD_RETRY_WAIT: std::time::Duration = std::time::Duration::from_secs(10);

impl Predownloader {
    fn spawn(
        gateway: Box<dyn SourceGateway + Send>,
        library_dir: PathBuf,
        index_guard: Arc<Mutex<()>>,
        storage_limit: u64,
    ) -> Self {
        let (tx, rx) = mpsc::channel::<PreloadJob>();
        let epoch = Arc::new(AtomicU64::new(0));
        let worker_epoch = Arc::clone(&epoch);
        let queued: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));
        let worker_queued = Arc::clone(&queued);
        let _ = std::thread::Builder::new()
            .name("gideon-predownload".into())
            .spawn(move || {
                // Run at idle CPU/IO priority: the reader must never stutter
                // because chapters are pre-fetching behind it.
                gideon_device::power::lower_current_thread_to_idle();
                // Chapters whose fetch failed, waiting out their backoff. Kept
                // OUT of the channel so a retry never delays a freshly queued
                // chapter — the one the reader is about to need always goes
                // first. Each entry is (job, attempts so far, when to retry).
                let mut retries: Vec<(PreloadJob, usize, std::time::Instant)> = Vec::new();
                loop {
                    // Take the next queued chapter, waiting at most until the
                    // soonest retry is due. Ends when the sender (and thus the
                    // app) is dropped.
                    let job = match retries.iter().map(|(_, _, due)| *due).min() {
                        Some(due) => {
                            let wait = due.saturating_duration_since(std::time::Instant::now());
                            match rx.recv_timeout(wait) {
                                Ok(job) => Some(job),
                                // Nothing new queued: the retry is due now.
                                Err(mpsc::RecvTimeoutError::Timeout) => None,
                                Err(mpsc::RecvTimeoutError::Disconnected) => break,
                            }
                        }
                        None => match rx.recv() {
                            Ok(job) => Some(job),
                            Err(_) => break,
                        },
                    };
                    let (job, attempts) = match job {
                        Some(job) => (job, 0),
                        None => {
                            // Pop the due retry (earliest first).
                            let Some(i) = retries
                                .iter()
                                .enumerate()
                                .min_by_key(|(_, (_, _, due))| *due)
                                .map(|(i, _)| i)
                            else {
                                continue;
                            };
                            let (job, attempts, _) = retries.swap_remove(i);
                            (job, attempts)
                        }
                    };
                    // The user left this manga after the job was queued — drop it
                    // instead of downloading chapters they've navigated away from.
                    // Persistent (explicitly requested) downloads ignore the epoch
                    // and always run.
                    if !job.persistent && job.epoch != worker_epoch.load(Ordering::Relaxed) {
                        continue;
                    }
                    if chapter_on_disk(
                        &library_dir,
                        &index_guard,
                        &job.source.id,
                        &job.manga.id,
                        &job.chapter_id,
                    ) {
                        continue; // already stored — nothing to do
                    }
                    let mut noop = |_done: usize, _total: usize| {};
                    match gateway.download_chapter(
                        &job.source.id,
                        &job.manga.id,
                        &job.chapter_id,
                        &library_dir,
                        &mut noop,
                    ) {
                        Ok(cbz) => {
                            record_chapter_in_index(
                                &library_dir,
                                &index_guard,
                                &job.source,
                                &job.manga,
                                &job.chapter_id,
                                &cbz,
                            );
                            // Stay within the storage budget as the batch lands.
                            evict_to_storage_limit(&library_dir, &index_guard, storage_limit);
                        }
                        // Offline / source error. The usual cause is a radio that
                        // hasn't finished coming back after a suspend, which fixes
                        // itself in seconds — so schedule another attempt rather
                        // than leaving the chapter un-stocked.
                        Err(_) => {
                            // Drop the dedup entry: a kick that comes in before
                            // the backoff expires — the next page turn, or the
                            // re-kick after waking with the radio back — must be
                            // able to try again straight away rather than wait
                            // out a schedule it can't see. A duplicate attempt
                            // is a cheap no-op once the chapter is on disk.
                            if let Ok(mut queued) = worker_queued.lock() {
                                queued.remove(&preload_key(
                                    &job.source.id,
                                    &job.manga.id,
                                    &job.chapter_id,
                                ));
                            }
                            let attempts = attempts + 1;
                            if attempts < PRELOAD_ATTEMPTS {
                                retries.push((
                                    job,
                                    attempts,
                                    std::time::Instant::now()
                                        + PRELOAD_RETRY_WAIT * attempts as u32,
                                ));
                            }
                        }
                    }
                }
            });
        Self { tx, queued, epoch }
    }

    /// Enqueue a chapter.
    ///
    /// The automatic look-ahead is deduped so every reader open / page advance
    /// doesn't re-enqueue the same chapter. An **explicit** (persistent) request
    /// — the ⋮ menu's "Download from here…" — must NOT be deduped: it's a
    /// deliberate download, the worker already no-ops anything on disk, and a
    /// stale dedup entry (a prior look-ahead, or an earlier attempt that failed
    /// on the worker and left nothing on disk) must never silently swallow it.
    /// Without this, a batch that didn't land the first time could never be
    /// re-requested in the same session — "it says it's downloading but never
    /// does".
    fn queue(&mut self, job: PreloadJob) {
        let key = preload_key(&job.source.id, &job.manga.id, &job.chapter_id);
        // Record the key regardless (so a later look-ahead won't re-add it), but
        // only *gate* on it for non-persistent look-ahead jobs.
        let fresh = match self.queued.lock() {
            Ok(mut queued) => queued.insert(key),
            Err(_) => true, // poisoned: never silently swallow the request
        };
        if job.persistent || fresh {
            // If the worker is gone the send just fails; nothing else to do.
            let _ = self.tx.send(job);
        }
    }

    /// The epoch new jobs should be stamped with.
    fn epoch(&self) -> u64 {
        self.epoch.load(Ordering::Relaxed)
    }

    /// Abandon everything queued so far: bump the epoch (so jobs still sitting in
    /// the channel are dropped when the worker reaches them) and clear the
    /// dedup set so a later visit can re-queue. Call this when the user leaves
    /// the manga — they shouldn't keep downloading its chapters in the
    /// background once they've moved on.
    fn cancel_pending(&mut self) {
        self.epoch.fetch_add(1, Ordering::Relaxed);
        if let Ok(mut queued) = self.queued.lock() {
            queued.clear();
        }
    }
}

/// Settings read straight from their directory, for paths that can't take a
/// `&self` borrow (the reader holds the display for its whole session).
fn settings_in(dir: Option<&Path>) -> gideon_core::Settings {
    dir.map(|dir| gideon_core::Settings::load(dir).unwrap_or_default())
        .unwrap_or_default()
}

/// The chapters to pre-fetch: a **fixed window** of the next `count` chapters
/// (by position) after the plan's anchor, minus any already on disk.
///
/// The window is bounded to `count` *positions* — NOT "the next `count` missing
/// chapters". That distinction is the whole point: if it walked past downloaded
/// chapters hunting for `count` missing ones, then every time the look-ahead
/// re-fired from the same chapter it would march one window further into the
/// series (c2,c3 → c4,c5 → c6,c7 …) and eventually download everything.
/// Anchored positionally, re-firing from the same chapter yields the same
/// window — all already stored — so it does nothing. That's what makes the
/// re-kick on wake safe to fire as often as we like.
fn lookahead_targets(plan: &LookaheadPlan, library_dir: &Path, count: usize) -> Vec<ChapterEntry> {
    let index = gideon_core::SeriesIndex::load(library_dir);
    let stored = index.find_manga(&plan.source.id, &plan.manga.id);
    let mut id = plan.after_id.clone();
    let mut out = Vec::new();
    for _ in 0..count {
        let Some(next) = next_chapter(&plan.chapters, &id) else {
            break; // reached the end of the chapter list
        };
        id = next.id.clone();
        let on_disk = stored.is_some_and(|(dir, series)| {
            series
                .downloaded
                .get(&next.id)
                .is_some_and(|file| library_dir.join(dir).join(file).exists())
        });
        if !on_disk {
            out.push(next); // in-window and not yet stored
        }
    }
    out
}

/// Dedup key for a queued chapter, shared by the queueing side and the worker
/// (which drops the key again when the download failed).
fn preload_key(source_id: &str, manga_id: &str, chapter_id: &str) -> String {
    format!("{source_id}\u{1f}{manga_id}\u{1f}{chapter_id}")
}

/// Downloaded-content usage for the storage screen.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct StorageStats {
    used: u64,
    chapters: usize,
    series: usize,
}

/// Evict least-recently-read downloads until the index-tracked downloads fit
/// within `limit_bytes`. Removes the CBZ, forgets it from the series index and
/// prunes a now-empty series directory; returns the number of bytes freed.
///
/// Recency is `max(atime, mtime)` per chapter (the same LRU signal the storage
/// engine uses): a chapter you just read or just downloaded is newest and goes
/// last. Guarded by `index_guard` so it never races a concurrent index rewrite.
fn evict_to_storage_limit(library_dir: &Path, guard: &Mutex<()>, limit_bytes: u64) -> u64 {
    let _g = guard.lock().unwrap_or_else(|e| e.into_inner());
    let mut index = gideon_core::SeriesIndex::load(library_dir);

    // (series_dir, file, path, size, recency) for every tracked, on-disk chapter.
    let mut items: Vec<(String, String, PathBuf, u64, std::time::SystemTime)> = Vec::new();
    for (dir, series) in index.iter() {
        for file in series.downloaded.values() {
            let path = library_dir.join(dir).join(file);
            if let Ok(meta) = std::fs::metadata(&path) {
                let atime = meta.accessed().unwrap_or(std::time::UNIX_EPOCH);
                let mtime = meta.modified().unwrap_or(std::time::UNIX_EPOCH);
                items.push((
                    dir.to_string(),
                    file.clone(),
                    path,
                    meta.len(),
                    atime.max(mtime),
                ));
            }
        }
    }

    let mut total: u64 = items.iter().map(|i| i.3).sum();
    if total <= limit_bytes {
        return 0;
    }

    items.sort_by_key(|i| i.4); // least-recently-touched first
    let mut freed = 0u64;
    let mut touched_dirs: Vec<String> = Vec::new();
    for (dir, file, path, size, _) in items {
        if total <= limit_bytes {
            break;
        }
        if std::fs::remove_file(&path).is_ok() {
            index.forget_download(&dir, &file);
            total = total.saturating_sub(size);
            freed += size;
            if !touched_dirs.contains(&dir) {
                touched_dirs.push(dir);
            }
        }
    }

    if freed > 0 {
        if let Err(e) = index.save(library_dir) {
            eprintln!("gideon: couldn't save the series index after eviction: {e}");
        }
        // Drop series dirs left empty by eviction.
        for dir in touched_dirs {
            let path = library_dir.join(&dir);
            if path != *library_dir
                && std::fs::read_dir(&path)
                    .map(|mut d| d.next().is_none())
                    .unwrap_or(false)
            {
                let _ = std::fs::remove_dir(&path);
            }
        }
    }
    freed
}

/// Whether a chapter's CBZ is recorded in the series index *and* present on
/// disk. Guarded so it never races a concurrent index rewrite.
fn chapter_on_disk(
    library_dir: &Path,
    guard: &Mutex<()>,
    source_id: &str,
    manga_id: &str,
    chapter_id: &str,
) -> bool {
    let _g = guard.lock().unwrap_or_else(|e| e.into_inner());
    let index = gideon_core::SeriesIndex::load(library_dir);
    if let Some((dir, series)) = index.find_manga(source_id, manga_id) {
        if let Some(file) = series.downloaded.get(chapter_id) {
            return library_dir.join(dir).join(file).exists();
        }
    }
    false
}

/// Record a freshly-downloaded chapter in the series index: where the series
/// came from (so its card relinks to the source) and which file holds the
/// chapter (so it opens instantly and shows as downloaded). Guarded by
/// `index_guard` so the foreground and background download paths serialize
/// their whole-file index rewrites instead of clobbering each other.
fn record_chapter_in_index(
    library_dir: &Path,
    guard: &Mutex<()>,
    source: &SourceEntry,
    manga: &MangaEntry,
    chapter_id: &str,
    cbz_path: &Path,
) {
    let Some(dir) = cbz_path.parent().and_then(|p| p.file_name()) else {
        return;
    };
    let dir = dir.to_string_lossy().to_string();
    let _g = guard.lock().unwrap_or_else(|e| e.into_inner());
    let mut index = gideon_core::SeriesIndex::load(library_dir);
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
        index.record_download(&dir, chapter_id, &file.to_string_lossy());
    }
    if let Err(e) = index.save(library_dir) {
        eprintln!("gideon: couldn't save the series index: {e}");
    }
    // Drop the index guard before the metadata lookup: it reloads and saves
    // the index itself, and it may touch the network. Holding the lock across
    // a request would stall every other download's bookkeeping behind it.
    drop(_g);
    // Best-effort, and deliberately after the chapter is safely on disk: this
    // can't fail the download, and it no-ops when offline, when the series
    // already has metadata, or when MyAnimeList has never heard of it.
    crate::manga::cache_series_metadata(library_dir, &dir);
}

/// Card name for a library entry: "Series — Chapter" when it lives in a
/// The series key a chapter key belongs to: its top-level directory
/// ("Series/vol3.cbz" → "Series"), or the whole key for a loose root file.
/// Used to record/look up the series' last-opened chapter.
fn series_key_of(chapter_key: &str) -> &str {
    chapter_key.split('/').next().unwrap_or(chapter_key)
}

/// series directory, just the file stem otherwise.
fn entry_title(relative_path: &str) -> String {
    let mut parts = relative_path.rsplitn(2, '/');
    let file = parts.next().unwrap_or(relative_path);
    let stem = file
        .strip_suffix(".cbz")
        .or_else(|| file.strip_suffix(".CBZ"))
        .unwrap_or(file);
    match parts.next() {
        Some(series) if !series.is_empty() => {
            format!("{} — {}", tidy_title(series), tidy_title(stem))
        }
        _ => tidy_title(stem),
    }
}

/// Display cleanup for names that came through the FAT32-safe filename
/// sanitizer (`gideon_sources::storage::sanitize`): characters like ':',
/// '?' and '*' in a source's title were stored as '_' in the directory name,
/// which then read as gibberish on the shelf ("Frieren_ Beyond Journey_s
/// End"). Collapse underscore runs to a single space for DISPLAY only —
/// paths and progress keys stay untouched. A name that was all underscores
/// keeps its original form rather than vanishing.
fn tidy_title(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut prev_space = false;
    for c in raw.chars() {
        let c = if c == '_' { ' ' } else { c };
        if c == ' ' {
            if prev_space {
                continue;
            }
            prev_space = true;
        } else {
            prev_space = false;
        }
        out.push(c);
    }
    let trimmed = out.trim();
    if trimmed.is_empty() {
        raw.to_string()
    } else {
        trimmed.to_string()
    }
}

/// Just the chapter's file stem ("Chapter 5"), without the series prefix —
/// for rows inside a single series' downloaded list.
fn chapter_label(relative_path: &str) -> String {
    let file = relative_path.rsplit('/').next().unwrap_or(relative_path);
    file.strip_suffix(".cbz")
        .or_else(|| file.strip_suffix(".CBZ"))
        .unwrap_or(file)
        .to_string()
}

/// Tag a chapter-row label with " · last opened" when it's the series'
/// last-opened chapter (the one a cover tap resumes), so it's visible in the
/// list.
fn label_with_last(label: String, is_last: bool) -> String {
    if is_last {
        format!("{label} · last opened")
    } else {
        label
    }
}

fn placeholder_cover() -> image::DynamicImage {
    image::DynamicImage::ImageLuma8(image::GrayImage::from_pixel(3, 4, image::Luma([0xCC])))
}

/// The message shown when an update check fails *after* connectivity was
/// confirmed. A transport failure that reached here means Wi-Fi is up but the
/// update server (GitHub) couldn't be reached — so don't blame Wi-Fi (the old
/// error did, which is confusing when Wi-Fi is clearly on). Note the release
/// may simply not be published yet. Any other failure keeps its detail.
fn update_error_body(err: &anyhow::Error) -> String {
    let unreachable = err
        .downcast_ref::<gideon_sources::Error>()
        .is_some_and(|e| matches!(e, gideon_sources::Error::Offline));
    if unreachable {
        "Couldn't reach GitHub to check for updates.\n\
         Wi-Fi is connected, so GitHub may be temporarily unavailable — or the \
         newest release simply isn't published yet. Try again later."
            .to_string()
    } else {
        format!("Update check failed: {err:#}")
    }
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
    // Live Wi-Fi status (one probe per Settings paint); off-device this reads
    // "connected" and the controls no-op.
    let wifi = if gideon_device::network::is_online() {
        "Wi-Fi: connected (tap to manage)".to_string()
    } else {
        "Wi-Fi: off (tap to scan)".to_string()
    };
    let auto_connect = if s.wifi_auto_connect { "on" } else { "off" };
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
        (wifi, true),
        (format!("Auto-connect Wi-Fi: {auto_connect}"), true),
        (
            format!(
                "Rotate wide spreads: {}",
                if s.auto_rotate_spreads { "on" } else { "off" }
            ),
            true,
        ),
        (
            format!(
                "Sleep when idle: {}",
                idle_suspend_label(s.idle_suspend_minutes)
            ),
            true,
        ),
        (format!("Colour profile: {}", s.color_profile), true),
        (
            format!(
                "Delete finished chapters: {}",
                cleanup_label(s.finished_cleanup_hours)
            ),
            true,
        ),
    ]
}

/// How the finished-chapter cleanup delay reads on the settings row.
/// "never" rather than "0 hours", because zero is a decision, not a duration.
fn cleanup_label(hours: u32) -> String {
    match hours {
        0 => "never".to_string(),
        24 => "after 1 day".to_string(),
        168 => "after 1 week".to_string(),
        h if h % 24 == 0 => format!("after {} days", h / 24),
        h => format!("after {h} hours"),
    }
}

/// The colour profiles the settings row cycles through, in the order the
/// palette reference presents them.
const COLOR_PROFILE_STEPS: [&str; 5] = ["ink-rust", "indigo", "sumi", "botanical", "mono"];

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
    gideon_core::profile::library_dir(base, profile)
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
